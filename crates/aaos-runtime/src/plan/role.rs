//! Role catalog: per-role YAML files loaded from /etc/aaos/roles/.
//!
//! A Role is the declarative description of an agent type — its model,
//! capabilities, system prompt, parameter schema, default budget and retry
//! policy. The Plan references roles by name; the RoleCatalog resolves the
//! name to the loaded Role.

use std::collections::HashMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ParameterType {
    String,
    Path,
    StringList,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ParameterSchema {
    #[serde(rename = "type")]
    pub param_type: ParameterType,
    #[serde(default)]
    pub required: bool,
    #[serde(default)]
    pub description: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoleBudget {
    pub max_input_tokens: u64,
    pub max_output_tokens: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoleRetry {
    pub max_attempts: u32,
    #[serde(default)]
    pub on: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Role {
    pub name: String,
    pub model: String,
    #[serde(default)]
    pub parameters: HashMap<String, ParameterSchema>,
    #[serde(default)]
    pub capabilities: Vec<String>,
    pub system_prompt: String,
    pub message_template: String,
    pub budget: RoleBudget,
    pub retry: RoleRetry,
}

/// In-memory catalog of loaded roles, keyed by role name.
#[derive(Debug, Clone, Default)]
pub struct RoleCatalog {
    roles: HashMap<String, Role>,
}

impl RoleCatalog {
    pub fn new() -> Self {
        Self::default()
    }

    /// Load every `*.yaml` under `dir` into the catalog. Invalid files error
    /// with a clear message naming the offending path. Caller decides whether
    /// to fail the daemon start or continue.
    pub fn load_from_dir(dir: &Path) -> Result<Self, RoleCatalogError> {
        let mut roles = HashMap::new();
        let entries = std::fs::read_dir(dir).map_err(|e| {
            RoleCatalogError::Io(format!("cannot read role dir {}: {}", dir.display(), e))
        })?;
        for entry in entries {
            let entry =
                entry.map_err(|e| RoleCatalogError::Io(format!("dir entry error: {e}")))?;
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) != Some("yaml") {
                continue;
            }
            let contents = std::fs::read_to_string(&path).map_err(|e| {
                RoleCatalogError::Io(format!("read {}: {e}", path.display()))
            })?;
            let role: Role = serde_yaml::from_str(&contents).map_err(|e| {
                RoleCatalogError::Parse(format!("parse {}: {e}", path.display()))
            })?;
            if role.name.is_empty() {
                return Err(RoleCatalogError::Parse(format!(
                    "role at {} has empty name",
                    path.display()
                )));
            }
            if roles.contains_key(&role.name) {
                return Err(RoleCatalogError::Parse(format!(
                    "duplicate role name '{}' at {}",
                    role.name,
                    path.display()
                )));
            }
            roles.insert(role.name.clone(), role);
        }
        Ok(Self { roles })
    }

    pub fn get(&self, name: &str) -> Option<&Role> {
        self.roles.get(name)
    }

    pub fn names(&self) -> Vec<&str> {
        let mut names: Vec<&str> = self.roles.keys().map(|s| s.as_str()).collect();
        names.sort();
        names
    }

    pub fn len(&self) -> usize {
        self.roles.len()
    }

    pub fn is_empty(&self) -> bool {
        self.roles.is_empty()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum RoleCatalogError {
    #[error("role catalog I/O: {0}")]
    Io(String),
    #[error("role parse error: {0}")]
    Parse(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn write(dir: &std::path::Path, name: &str, body: &str) {
        std::fs::write(dir.join(name), body).unwrap();
    }

    const FETCHER_YAML: &str = r#"
name: fetcher
model: deepseek-chat
parameters:
  url:
    type: string
    required: true
    description: The URL to fetch.
  workspace:
    type: path
    required: true
    description: Output path.
capabilities:
  - "tool: web_fetch"
system_prompt: "you are a fetcher"
message_template: "fetch {url} to {workspace}"
budget:
  max_input_tokens: 20000
  max_output_tokens: 2000
retry:
  max_attempts: 2
  on: ["timeout"]
"#;

    #[test]
    fn load_single_role_yaml() {
        let dir = tempdir().unwrap();
        write(dir.path(), "fetcher.yaml", FETCHER_YAML);
        let cat = RoleCatalog::load_from_dir(dir.path()).unwrap();
        assert_eq!(cat.len(), 1);
        let role = cat.get("fetcher").unwrap();
        assert_eq!(role.name, "fetcher");
        assert_eq!(role.model, "deepseek-chat");
        assert_eq!(role.parameters.len(), 2);
        assert!(role.parameters["url"].required);
        assert_eq!(role.parameters["url"].param_type, ParameterType::String);
        assert_eq!(
            role.parameters["workspace"].param_type,
            ParameterType::Path
        );
        assert_eq!(role.capabilities.len(), 1);
        assert_eq!(role.budget.max_input_tokens, 20000);
        assert_eq!(role.retry.max_attempts, 2);
    }

    #[test]
    fn skip_non_yaml_files() {
        let dir = tempdir().unwrap();
        write(dir.path(), "fetcher.yaml", FETCHER_YAML);
        write(dir.path(), "README.md", "# notes");
        write(dir.path(), ".hidden", "ignored");
        let cat = RoleCatalog::load_from_dir(dir.path()).unwrap();
        assert_eq!(cat.len(), 1);
        assert!(cat.get("fetcher").is_some());
    }

    #[test]
    fn duplicate_role_name_errors() {
        let dir = tempdir().unwrap();
        write(dir.path(), "fetcher.yaml", FETCHER_YAML);
        write(dir.path(), "dup.yaml", FETCHER_YAML);
        let err = RoleCatalog::load_from_dir(dir.path()).unwrap_err();
        matches!(err, RoleCatalogError::Parse(_));
    }

    #[test]
    fn malformed_yaml_errors_with_path() {
        let dir = tempdir().unwrap();
        write(dir.path(), "bad.yaml", "not: valid: yaml: ::: ::");
        let result = RoleCatalog::load_from_dir(dir.path());
        assert!(result.is_err());
    }

    #[test]
    fn missing_dir_errors() {
        let result = RoleCatalog::load_from_dir(std::path::Path::new("/does/not/exist"));
        assert!(result.is_err());
    }

    #[test]
    fn names_returned_sorted() {
        let dir = tempdir().unwrap();
        write(dir.path(), "fetcher.yaml", FETCHER_YAML);
        let alt = FETCHER_YAML.replace("name: fetcher", "name: analyzer");
        write(dir.path(), "analyzer.yaml", &alt);
        let cat = RoleCatalog::load_from_dir(dir.path()).unwrap();
        assert_eq!(cat.names(), vec!["analyzer", "fetcher"]);
    }
}
