//! Tool for agents to read skill instructions (progressive disclosure tier 2+3).
//! Agents see the skill catalog in their system prompt, then call this tool
//! to load full instructions or reference files when they activate a skill.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Value};

use aaos_core::{AuditLog, Skill, ToolDefinition};
use crate::Tool;
use crate::context::InvocationContext;

/// Registry of loaded skills, keyed by name.
pub struct SkillRegistry {
    skills: HashMap<String, Skill>,
}

impl SkillRegistry {
    pub fn new(skills: Vec<Skill>) -> Self {
        let mut map = HashMap::new();
        for skill in skills {
            map.insert(skill.meta.name.clone(), skill);
        }
        Self { skills: map }
    }

    pub fn get(&self, name: &str) -> Option<&Skill> {
        self.skills.get(name)
    }

    pub fn catalog(&self) -> String {
        if self.skills.is_empty() {
            return String::new();
        }
        let mut lines = vec!["## Available Skills".to_string()];
        lines.push("Use the `skill_read` tool to load a skill's full instructions.".to_string());
        lines.push(String::new());
        for skill in self.skills.values() {
            lines.push(skill.catalog_entry());
        }
        lines.join("\n")
    }

    pub fn names(&self) -> Vec<&str> {
        self.skills.keys().map(|s| s.as_str()).collect()
    }
}

/// Tool that lets agents read skill instructions and reference files.
pub struct SkillReadTool {
    registry: Arc<SkillRegistry>,
    audit_log: Arc<dyn AuditLog>,
}

impl SkillReadTool {
    pub fn new(registry: Arc<SkillRegistry>, audit_log: Arc<dyn AuditLog>) -> Self {
        Self { registry, audit_log }
    }
}

#[async_trait]
impl Tool for SkillReadTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "skill_read".into(),
            description: "Load a skill's full instructions or read a reference file from a skill. Use this when you want to activate a skill from the catalog.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "skill_name": {
                        "type": "string",
                        "description": "Name of the skill to read"
                    },
                    "file": {
                        "type": "string",
                        "description": "Optional: relative path to a reference file within the skill (e.g. 'references/REFERENCE.md')"
                    }
                },
                "required": ["skill_name"]
            }),
        }
    }

    async fn invoke(&self, input: Value, ctx: &InvocationContext) -> aaos_core::Result<Value> {
        let skill_name = input
            .get("skill_name")
            .and_then(|v| v.as_str())
            .ok_or_else(|| aaos_core::CoreError::InvalidManifest("missing skill_name".into()))?;

        let skill = self.registry.get(skill_name).ok_or_else(|| {
            aaos_core::CoreError::ToolNotFound(format!("skill not found: {skill_name}"))
        })?;

        // Check if a specific file is requested (tier 3)
        if let Some(file_path) = input.get("file").and_then(|v| v.as_str()) {
            let full_path = skill.path.join(file_path);

            // Security: ensure the path stays within the skill directory
            let canonical = full_path.canonicalize().map_err(|e| {
                aaos_core::CoreError::Ipc(format!("cannot read skill file: {e}"))
            })?;
            let skill_canonical = skill.path.canonicalize().map_err(|e| {
                aaos_core::CoreError::Ipc(format!("cannot resolve skill path: {e}"))
            })?;
            if !canonical.starts_with(&skill_canonical) {
                return Err(aaos_core::CoreError::Ipc(
                    "path traversal: file must be within skill directory".into(),
                ));
            }

            let content = std::fs::read_to_string(&canonical).map_err(|e| {
                aaos_core::CoreError::Ipc(format!("failed to read {}: {e}", file_path))
            })?;

            self.audit_log.record(aaos_core::AuditEvent::new(
                ctx.agent_id,
                aaos_core::AuditEventKind::ToolInvoked {
                    tool: "skill_read".into(),
                    input_hash: format!("{}:{}", skill_name, file_path),
                },
            ));

            return Ok(json!({
                "skill": skill_name,
                "file": file_path,
                "content": content
            }));
        }

        // Tier 2: return full skill instructions
        self.audit_log.record(aaos_core::AuditEvent::new(
            ctx.agent_id,
            aaos_core::AuditEventKind::ToolInvoked {
                tool: "skill_read".into(),
                input_hash: skill_name.to_string(),
            },
        ));

        Ok(json!({
            "skill": skill_name,
            "instructions": skill.activation_prompt(),
            "reference_files": skill.reference_files()
                .iter()
                .filter_map(|p| p.strip_prefix(&skill.path).ok())
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aaos_core::InMemoryAuditLog;
    use tempfile::tempdir;

    fn test_ctx() -> InvocationContext {
        InvocationContext {
            agent_id: aaos_core::AgentId::new(),
            tokens: vec![],
            capability_registry: std::sync::Arc::new(aaos_core::CapabilityRegistry::new()),
        }
    }

    #[tokio::test]
    async fn read_skill_instructions() {
        let dir = tempdir().unwrap();
        let skill_dir = dir.path().join("test-skill");
        std::fs::create_dir(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: test-skill\ndescription: A test.\n---\n# Instructions\nDo the thing.",
        ).unwrap();

        let skill = Skill::load(&skill_dir).unwrap();
        let registry = Arc::new(SkillRegistry::new(vec![skill]));
        let audit = Arc::new(InMemoryAuditLog::new());
        let tool = SkillReadTool::new(registry, audit);

        let result = tool
            .invoke(json!({"skill_name": "test-skill"}), &test_ctx())
            .await
            .unwrap();

        assert!(result["instructions"].as_str().unwrap().contains("Do the thing"));
    }

    #[tokio::test]
    async fn read_reference_file() {
        let dir = tempdir().unwrap();
        let skill_dir = dir.path().join("ref-skill");
        std::fs::create_dir(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: ref-skill\ndescription: Has refs.\n---\nBody",
        ).unwrap();
        let refs = skill_dir.join("references");
        std::fs::create_dir(&refs).unwrap();
        std::fs::write(refs.join("GUIDE.md"), "# Guide\nDetailed info.").unwrap();

        let skill = Skill::load(&skill_dir).unwrap();
        let registry = Arc::new(SkillRegistry::new(vec![skill]));
        let audit = Arc::new(InMemoryAuditLog::new());
        let tool = SkillReadTool::new(registry, audit);

        let result = tool
            .invoke(json!({"skill_name": "ref-skill", "file": "references/GUIDE.md"}), &test_ctx())
            .await
            .unwrap();

        assert_eq!(result["content"].as_str().unwrap(), "# Guide\nDetailed info.");
    }

    #[tokio::test]
    async fn path_traversal_blocked() {
        let dir = tempdir().unwrap();
        let skill_dir = dir.path().join("safe-skill");
        std::fs::create_dir(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: safe-skill\ndescription: Safe.\n---\nBody",
        ).unwrap();

        let skill = Skill::load(&skill_dir).unwrap();
        let registry = Arc::new(SkillRegistry::new(vec![skill]));
        let audit = Arc::new(InMemoryAuditLog::new());
        let tool = SkillReadTool::new(registry, audit);

        let result = tool
            .invoke(json!({"skill_name": "safe-skill", "file": "../../etc/passwd"}), &test_ctx())
            .await;

        assert!(result.is_err());
    }

    #[tokio::test]
    async fn unknown_skill_returns_error() {
        let registry = Arc::new(SkillRegistry::new(vec![]));
        let audit = Arc::new(InMemoryAuditLog::new());
        let tool = SkillReadTool::new(registry, audit);

        let result = tool
            .invoke(json!({"skill_name": "nonexistent"}), &test_ctx())
            .await;

        assert!(result.is_err());
    }

    #[test]
    fn catalog_generation() {
        let dir = tempdir().unwrap();
        let s1 = dir.path().join("skill-a");
        std::fs::create_dir(&s1).unwrap();
        std::fs::write(s1.join("SKILL.md"), "---\nname: skill-a\ndescription: Does A.\n---\nBody").unwrap();

        let skill = Skill::load(&s1).unwrap();
        let registry = SkillRegistry::new(vec![skill]);
        let catalog = registry.catalog();

        assert!(catalog.contains("## Available Skills"));
        assert!(catalog.contains("**skill-a**: Does A."));
    }
}
