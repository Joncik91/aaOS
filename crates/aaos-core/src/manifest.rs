use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::error::{CoreError, Result};

/// How the system prompt is sourced.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum PromptSource {
    Inline(String),
    File(PathBuf),
}

/// Agent lifecycle policy.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Lifecycle {
    #[default]
    OnDemand,
    Persistent,
    Scheduled(String),
}

/// Memory configuration for an agent.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemoryConfig {
    #[serde(default = "default_context_window")]
    pub context_window: String,
    #[serde(default)]
    pub max_history_messages: Option<usize>,
    #[serde(default)]
    pub summarization_model: Option<String>,
    #[serde(default)]
    pub summarization_threshold: Option<f32>,
    #[serde(default)]
    pub archive_ttl_days: Option<u32>,
    #[serde(default)]
    pub episodic_store: Option<String>,
}

fn default_context_window() -> String {
    "128k".to_string()
}

impl Default for MemoryConfig {
    fn default() -> Self {
        Self {
            context_window: default_context_window(),
            max_history_messages: None,
            summarization_model: None,
            summarization_threshold: None,
            archive_ttl_days: None,
            episodic_store: None,
        }
    }
}

/// Token budget for context window management.
/// Parses human-readable sizes like "128k" and caps at model max.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenBudget {
    pub max_tokens: u32,
}

impl TokenBudget {
    /// Parse "128k" -> 131_072, "200k" -> 204_800, plain number "50000" -> 50_000.
    /// Caps at min(parsed, model_max).
    pub fn from_config(config_str: &str, model_max: u32) -> Result<Self> {
        let config_str = config_str.trim();
        let parsed = if let Some(prefix) = config_str.strip_suffix('k') {
            let n: u32 = prefix.parse().map_err(|_| {
                CoreError::InvalidManifest(format!("invalid context_window format: '{config_str}'"))
            })?;
            n * 1024
        } else if let Some(prefix) = config_str.strip_suffix('K') {
            let n: u32 = prefix.parse().map_err(|_| {
                CoreError::InvalidManifest(format!("invalid context_window format: '{config_str}'"))
            })?;
            n * 1024
        } else {
            config_str.parse::<u32>().map_err(|_| {
                CoreError::InvalidManifest(format!("invalid context_window format: '{config_str}'"))
            })?
        };
        Ok(Self {
            max_tokens: parsed.min(model_max),
        })
    }
}

/// A capability declaration in the manifest (before token issuance).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum CapabilityDeclaration {
    Simple(String),
    WithParams {
        #[serde(flatten)]
        params: HashMap<String, serde_json::Value>,
    },
}

/// An agent manifest — the declarative bundle that defines an agent process.
///
/// Parsed from YAML. This is the aaOS equivalent of an executable binary:
/// it declares what the agent is, what it can do, and how it should run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentManifest {
    pub name: String,
    pub model: String,
    pub system_prompt: PromptSource,
    #[serde(default)]
    pub capabilities: Vec<CapabilityDeclaration>,
    #[serde(default)]
    pub memory: MemoryConfig,
    #[serde(default)]
    pub lifecycle: Lifecycle,
    #[serde(default)]
    pub metadata: HashMap<String, serde_json::Value>,
    #[serde(default)]
    pub approval_required: Vec<String>,
}

impl AgentManifest {
    /// Parse a manifest from a YAML string.
    pub fn from_yaml(yaml: &str) -> Result<Self> {
        let manifest: Self = serde_yaml::from_str(yaml)?;
        manifest.validate()?;
        Ok(manifest)
    }

    /// Load a manifest from a YAML file.
    pub fn from_file(path: &Path) -> Result<Self> {
        let content = std::fs::read_to_string(path).map_err(|e| {
            CoreError::InvalidManifest(format!("cannot read {}: {e}", path.display()))
        })?;
        Self::from_yaml(&content)
    }

    fn validate(&self) -> Result<()> {
        if self.name.is_empty() {
            return Err(CoreError::InvalidManifest("name is required".into()));
        }
        if self.model.is_empty() {
            return Err(CoreError::InvalidManifest("model is required".into()));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_minimal_manifest() {
        let yaml = r#"
name: test-agent
model: claude-haiku-4-5-20251001
system_prompt: "You are a helpful assistant."
"#;
        let manifest = AgentManifest::from_yaml(yaml).unwrap();
        assert_eq!(manifest.name, "test-agent");
        assert_eq!(manifest.model, "claude-haiku-4-5-20251001");
        assert_eq!(manifest.lifecycle, Lifecycle::OnDemand);
    }

    #[test]
    fn parse_full_manifest() {
        let yaml = r#"
name: research-agent
model: claude-haiku-4-5-20251001
system_prompt: "./prompts/researcher.md"
capabilities:
  - web_search
  - "file_read: /data/project-x/*"
memory:
  context_window: "128k"
  episodic_store: "512MB"
lifecycle: persistent
metadata:
  team: research
  priority: high
"#;
        let manifest = AgentManifest::from_yaml(yaml).unwrap();
        assert_eq!(manifest.name, "research-agent");
        assert_eq!(manifest.capabilities.len(), 2);
        assert_eq!(manifest.lifecycle, Lifecycle::Persistent);
        assert_eq!(manifest.memory.episodic_store, Some("512MB".into()));
    }

    #[test]
    fn reject_empty_name() {
        let yaml = r#"
name: ""
model: claude-haiku-4-5-20251001
system_prompt: "test"
"#;
        let err = AgentManifest::from_yaml(yaml).unwrap_err();
        assert!(err.to_string().contains("name is required"));
    }

    #[test]
    fn manifest_roundtrips_yaml() {
        let yaml = r#"
name: roundtrip-agent
model: claude-haiku-4-5-20251001
system_prompt: "You are a test."
lifecycle: on-demand
"#;
        let manifest = AgentManifest::from_yaml(yaml).unwrap();
        let serialized = serde_yaml::to_string(&manifest).unwrap();
        let reparsed = AgentManifest::from_yaml(&serialized).unwrap();
        assert_eq!(manifest.name, reparsed.name);
        assert_eq!(manifest.model, reparsed.model);
    }

    #[test]
    fn parse_manifest_with_approval_required() {
        let yaml = r#"
name: test-agent
model: claude-haiku-4-5-20251001
system_prompt: "test"
approval_required:
  - file_write
  - spawn_agent
"#;
        let manifest = AgentManifest::from_yaml(yaml).unwrap();
        assert_eq!(
            manifest.approval_required,
            vec!["file_write", "spawn_agent"]
        );
    }

    #[test]
    fn parse_manifest_without_approval_required() {
        let yaml = r#"
name: test-agent
model: claude-haiku-4-5-20251001
system_prompt: "test"
"#;
        let manifest = AgentManifest::from_yaml(yaml).unwrap();
        assert!(manifest.approval_required.is_empty());
    }

    #[test]
    fn parse_manifest_with_max_history() {
        let yaml = r#"
name: persistent-agent
model: claude-haiku-4-5-20251001
system_prompt: "test"
lifecycle: persistent
memory:
  max_history_messages: 100
"#;
        let manifest = AgentManifest::from_yaml(yaml).unwrap();
        assert_eq!(manifest.memory.max_history_messages, Some(100));
        assert_eq!(manifest.lifecycle, Lifecycle::Persistent);
    }

    #[test]
    fn token_budget_parse_128k() {
        let budget = TokenBudget::from_config("128k", 200_000).unwrap();
        assert_eq!(budget.max_tokens, 131_072);
    }

    #[test]
    fn token_budget_parse_200k() {
        let budget = TokenBudget::from_config("200k", 300_000).unwrap();
        assert_eq!(budget.max_tokens, 204_800);
    }

    #[test]
    fn token_budget_parse_plain_number() {
        let budget = TokenBudget::from_config("50000", 200_000).unwrap();
        assert_eq!(budget.max_tokens, 50_000);
    }

    #[test]
    fn token_budget_caps_at_model_max() {
        let budget = TokenBudget::from_config("200k", 100_000).unwrap();
        assert_eq!(budget.max_tokens, 100_000);
    }

    #[test]
    fn token_budget_invalid_format() {
        let err = TokenBudget::from_config("abc", 200_000).unwrap_err();
        assert!(err.to_string().contains("invalid context_window"));
    }

    #[test]
    fn parse_manifest_with_summarization_config() {
        let yaml = r#"
name: context-agent
model: claude-haiku-4-5-20251001
system_prompt: "test"
lifecycle: persistent
memory:
  context_window: "128k"
  max_history_messages: 200
  summarization_model: "claude-haiku-4-5-20251001"
  summarization_threshold: 0.8
  archive_ttl_days: 14
"#;
        let manifest = AgentManifest::from_yaml(yaml).unwrap();
        assert_eq!(manifest.memory.summarization_model, Some("claude-haiku-4-5-20251001".into()));
        assert_eq!(manifest.memory.summarization_threshold, Some(0.8));
        assert_eq!(manifest.memory.archive_ttl_days, Some(14));
    }

    #[test]
    fn memory_config_defaults() {
        let config = MemoryConfig::default();
        assert_eq!(config.context_window, "128k");
        assert_eq!(config.summarization_model, None);
        assert_eq!(config.summarization_threshold, None);
        assert_eq!(config.archive_ttl_days, None);
    }
}
