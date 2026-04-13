//! AgentSkills loader — parses SKILL.md files per the AgentSkills specification.
//! https://agentskills.io/specification

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{CoreError, Result};

/// Parsed skill metadata from SKILL.md frontmatter.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillMetadata {
    pub name: String,
    pub description: String,
    #[serde(default)]
    pub license: Option<String>,
    #[serde(default)]
    pub compatibility: Option<String>,
    #[serde(default)]
    pub metadata: HashMap<String, String>,
    #[serde(default, rename = "allowed-tools")]
    pub allowed_tools: Option<String>,
}

/// A loaded skill — metadata + body + location on disk.
#[derive(Debug, Clone)]
pub struct Skill {
    pub meta: SkillMetadata,
    /// Markdown body (instructions). Loaded on activation.
    pub body: String,
    /// Path to the skill directory.
    pub path: PathBuf,
}

impl Skill {
    /// Load a skill from a directory containing SKILL.md.
    pub fn load(skill_dir: &Path) -> Result<Self> {
        let skill_md = skill_dir.join("SKILL.md");
        if !skill_md.exists() {
            return Err(CoreError::InvalidManifest(format!(
                "no SKILL.md in {}",
                skill_dir.display()
            )));
        }

        let content = std::fs::read_to_string(&skill_md).map_err(|e| {
            CoreError::InvalidManifest(format!(
                "failed to read {}: {e}",
                skill_md.display()
            ))
        })?;

        let (meta, body) = parse_skill_md(&content)?;

        Ok(Self {
            meta,
            body,
            path: skill_dir.to_path_buf(),
        })
    }

    /// Generate the catalog entry (~50-100 tokens) for system prompt injection.
    pub fn catalog_entry(&self) -> String {
        format!(
            "- **{}**: {}",
            self.meta.name, self.meta.description
        )
    }

    /// Generate the full activation prompt for injection into agent system prompt.
    pub fn activation_prompt(&self) -> String {
        format!(
            "<skill name=\"{}\">\n{}\n</skill>",
            self.meta.name, self.body
        )
    }

    /// List reference files available in this skill (scripts/, references/, assets/).
    pub fn reference_files(&self) -> Vec<PathBuf> {
        let mut files = Vec::new();
        for subdir in &["scripts", "references", "assets"] {
            let dir = self.path.join(subdir);
            if dir.is_dir() {
                if let Ok(entries) = std::fs::read_dir(&dir) {
                    for entry in entries.flatten() {
                        if entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
                            files.push(entry.path());
                        }
                    }
                }
            }
        }
        files
    }

    /// Parse the allowed-tools field into individual tool permissions.
    /// Format: "Bash(git:*) Bash(jq:*) Read" → vec of tool permission strings.
    pub fn allowed_tool_list(&self) -> Vec<String> {
        match &self.meta.allowed_tools {
            Some(tools) => tools
                .split_whitespace()
                .map(|s| s.to_string())
                .collect(),
            None => Vec::new(),
        }
    }
}

/// Scan a directory for skill subdirectories (each containing SKILL.md).
pub fn discover_skills(base_dir: &Path) -> Vec<Skill> {
    let mut skills = Vec::new();

    let entries = match std::fs::read_dir(base_dir) {
        Ok(e) => e,
        Err(_) => return skills,
    };

    for entry in entries.flatten() {
        let path = entry.path();
        // Skip non-directories and hidden/special dirs
        if !path.is_dir() {
            continue;
        }
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if name_str.starts_with('.') || name_str == "node_modules" {
            continue;
        }

        if path.join("SKILL.md").exists() {
            match Skill::load(&path) {
                Ok(skill) => {
                    tracing::info!(skill = %skill.meta.name, path = %path.display(), "skill loaded");
                    skills.push(skill);
                }
                Err(e) => {
                    tracing::warn!(path = %path.display(), error = %e, "failed to load skill");
                }
            }
        }
    }

    skills
}

/// Parse SKILL.md content into metadata + body.
fn parse_skill_md(content: &str) -> Result<(SkillMetadata, String)> {
    let content = content.trim();

    // Find frontmatter delimiters
    if !content.starts_with("---") {
        return Err(CoreError::InvalidManifest(
            "SKILL.md must start with --- (YAML frontmatter)".into(),
        ));
    }

    let after_first = &content[3..];
    let closing = after_first.find("\n---").ok_or_else(|| {
        CoreError::InvalidManifest("SKILL.md missing closing --- for frontmatter".into())
    })?;

    let yaml_str = &after_first[..closing];
    let body = after_first[closing + 4..].trim().to_string();

    let meta: SkillMetadata = serde_yaml::from_str(yaml_str).map_err(|e| {
        CoreError::InvalidManifest(format!("invalid SKILL.md frontmatter: {e}"))
    })?;

    if meta.name.is_empty() {
        return Err(CoreError::InvalidManifest(
            "SKILL.md name is required".into(),
        ));
    }
    if meta.description.is_empty() {
        return Err(CoreError::InvalidManifest(
            "SKILL.md description is required".into(),
        ));
    }

    Ok((meta, body))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn parse_minimal_skill() {
        let content = r#"---
name: test-skill
description: A test skill for unit testing.
---

# Test Skill

Do something useful.
"#;
        let (meta, body) = parse_skill_md(content).unwrap();
        assert_eq!(meta.name, "test-skill");
        assert_eq!(meta.description, "A test skill for unit testing.");
        assert!(body.contains("# Test Skill"));
    }

    #[test]
    fn parse_skill_with_all_fields() {
        let content = r#"---
name: pdf-processing
description: Extract PDF text, fill forms, merge files.
license: Apache-2.0
compatibility: Requires Python 3.14+
allowed-tools: Bash(git:*) Read
metadata:
  author: example-org
  version: "1.0"
---

# PDF Processing

Step 1: Use pdfplumber.
"#;
        let (meta, body) = parse_skill_md(content).unwrap();
        assert_eq!(meta.name, "pdf-processing");
        assert_eq!(meta.allowed_tools, Some("Bash(git:*) Read".into()));
        assert_eq!(meta.metadata.get("author").unwrap(), "example-org");
        assert!(body.contains("pdfplumber"));
    }

    #[test]
    fn reject_missing_name() {
        let content = "---\ndescription: test\n---\nbody";
        let err = parse_skill_md(content);
        assert!(err.is_err());
    }

    #[test]
    fn reject_missing_frontmatter() {
        let content = "# Just markdown\nNo frontmatter here.";
        let err = parse_skill_md(content);
        assert!(err.is_err());
    }

    #[test]
    fn load_skill_from_directory() {
        let dir = tempdir().unwrap();
        let skill_dir = dir.path().join("my-skill");
        fs::create_dir(&skill_dir).unwrap();
        fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: my-skill\ndescription: Does things.\n---\n# Instructions\nDo it.",
        )
        .unwrap();

        let skill = Skill::load(&skill_dir).unwrap();
        assert_eq!(skill.meta.name, "my-skill");
        assert!(skill.body.contains("# Instructions"));
    }

    #[test]
    fn discover_skills_in_directory() {
        let dir = tempdir().unwrap();

        // Create two skill directories
        let s1 = dir.path().join("skill-a");
        fs::create_dir(&s1).unwrap();
        fs::write(s1.join("SKILL.md"), "---\nname: skill-a\ndescription: A.\n---\nBody A").unwrap();

        let s2 = dir.path().join("skill-b");
        fs::create_dir(&s2).unwrap();
        fs::write(s2.join("SKILL.md"), "---\nname: skill-b\ndescription: B.\n---\nBody B").unwrap();

        // Non-skill directory (no SKILL.md)
        let s3 = dir.path().join("not-a-skill");
        fs::create_dir(&s3).unwrap();
        fs::write(s3.join("README.md"), "just a readme").unwrap();

        let skills = discover_skills(dir.path());
        assert_eq!(skills.len(), 2);
    }

    #[test]
    fn catalog_entry_format() {
        let content = "---\nname: code-review\ndescription: Reviews code for quality.\n---\nBody";
        let (meta, body) = parse_skill_md(content).unwrap();
        let skill = Skill {
            meta,
            body,
            path: PathBuf::from("/tmp/code-review"),
        };
        assert_eq!(
            skill.catalog_entry(),
            "- **code-review**: Reviews code for quality."
        );
    }

    #[test]
    fn allowed_tools_parsing() {
        let content = "---\nname: t\ndescription: d\nallowed-tools: Bash(git:*) Bash(jq:*) Read\n---\nbody";
        let (meta, _) = parse_skill_md(content).unwrap();
        let skill = Skill {
            meta,
            body: String::new(),
            path: PathBuf::new(),
        };
        let tools = skill.allowed_tool_list();
        assert_eq!(tools, vec!["Bash(git:*)", "Bash(jq:*)", "Read"]);
    }

    #[test]
    fn reference_files_discovery() {
        let dir = tempdir().unwrap();
        let skill_dir = dir.path().join("my-skill");
        fs::create_dir(&skill_dir).unwrap();
        fs::write(skill_dir.join("SKILL.md"), "---\nname: my-skill\ndescription: d.\n---\nbody").unwrap();

        let scripts = skill_dir.join("scripts");
        fs::create_dir(&scripts).unwrap();
        fs::write(scripts.join("run.sh"), "#!/bin/bash").unwrap();

        let refs = skill_dir.join("references");
        fs::create_dir(&refs).unwrap();
        fs::write(refs.join("REFERENCE.md"), "# Ref").unwrap();

        let skill = Skill::load(&skill_dir).unwrap();
        let files = skill.reference_files();
        assert_eq!(files.len(), 2);
    }
}
