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

impl Role {
    /// Validate submitted Plan parameters against this role's schema.
    ///
    /// Returns a single human-friendly error describing all problems so the
    /// Planner's replan loop can correct them in one shot rather than having
    /// to iterate on one error at a time.
    pub fn validate_params(&self, params: &serde_json::Value) -> Result<(), String> {
        let obj = params
            .as_object()
            .ok_or_else(|| format!("role '{}' expected params object, got non-object", self.name))?;

        let mut problems: Vec<String> = Vec::new();

        for (name, schema) in &self.parameters {
            if schema.required && !obj.contains_key(name) {
                problems.push(format!("missing required param '{name}'"));
            }
        }

        for name in obj.keys() {
            if !self.parameters.contains_key(name) {
                problems.push(format!("unknown param '{name}'"));
            }
        }

        for (name, value) in obj {
            if let Some(schema) = self.parameters.get(name) {
                let ok = match schema.param_type {
                    ParameterType::String | ParameterType::Path => value.is_string(),
                    ParameterType::StringList => value
                        .as_array()
                        .map(|arr| arr.iter().all(|v| v.is_string()))
                        .unwrap_or(false),
                };
                if !ok {
                    let expected = match schema.param_type {
                        ParameterType::String => "string",
                        ParameterType::Path => "string (path)",
                        ParameterType::StringList => "array of strings",
                    };
                    problems.push(format!("param '{name}' must be {expected}"));
                }
            }
        }

        if problems.is_empty() {
            Ok(())
        } else {
            Err(format!(
                "role '{}' param validation failed: {}",
                self.name,
                problems.join("; ")
            ))
        }
    }

    /// Produce the child agent's first message by substituting `{param}`
    /// tokens in `message_template`. String-list params render as comma-space
    /// separated values.
    pub fn render_message(&self, params: &serde_json::Value) -> String {
        let mut out = self.message_template.clone();
        if let Some(obj) = params.as_object() {
            for (name, value) in obj {
                let token = format!("{{{name}}}");
                let rendered = render_value(value);
                out = out.replace(&token, &rendered);
            }
        }
        out
    }

    /// Produce the YAML manifest string for instantiating a child of this role
    /// with the given params. Path-sensitive capability patterns (`{workspace}`,
    /// `{output}`) are substituted; anything else is emitted verbatim. The
    /// result is parseable via `AgentManifest::from_yaml`.
    pub fn render_manifest(&self, params: &serde_json::Value) -> String {
        let caps: Vec<String> = self
            .capabilities
            .iter()
            .map(|c| substitute_tokens(c, params))
            .collect();

        let caps_yaml: String = caps
            .iter()
            .map(|c| format!("  - \"{}\"\n", c.replace('"', "\\\"")))
            .collect();

        format!(
            "name: {name}\nmodel: {model}\nsystem_prompt: |\n{prompt}\ncapabilities:\n{caps}",
            name = self.name,
            model = self.model,
            prompt = indent(&self.system_prompt, "  "),
            caps = caps_yaml,
        )
    }
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

    #[cfg(test)]
    pub fn roles_mut(&mut self) -> &mut std::collections::HashMap<String, Role> {
        &mut self.roles
    }
}

#[derive(Debug, thiserror::Error)]
pub enum RoleCatalogError {
    #[error("role catalog I/O: {0}")]
    Io(String),
    #[error("role parse error: {0}")]
    Parse(String),
}

fn render_value(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Array(arr) => arr
            .iter()
            .map(render_value)
            .collect::<Vec<_>>()
            .join(", "),
        serde_json::Value::Null => String::new(),
        other => other.to_string(),
    }
}

fn substitute_tokens(template: &str, params: &serde_json::Value) -> String {
    let mut out = template.to_string();
    if let Some(obj) = params.as_object() {
        for (name, value) in obj {
            let token = format!("{{{name}}}");
            out = out.replace(&token, &render_value(value));
        }
    }
    out
}

fn indent(s: &str, prefix: &str) -> String {
    s.lines()
        .map(|l| format!("{prefix}{l}"))
        .collect::<Vec<_>>()
        .join("\n")
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

    use serde_json::json;

    fn fetcher_role() -> Role {
        serde_yaml::from_str(FETCHER_YAML).unwrap()
    }

    #[test]
    fn validate_accepts_correct_params() {
        let r = fetcher_role();
        let params = json!({"url": "https://x.com", "workspace": "/tmp/x.html"});
        r.validate_params(&params).unwrap();
    }

    #[test]
    fn validate_rejects_missing_required() {
        let r = fetcher_role();
        let params = json!({"url": "https://x.com"});
        let err = r.validate_params(&params).unwrap_err();
        assert!(err.contains("workspace"), "err was: {}", err);
        assert!(err.contains("missing"), "err was: {}", err);
    }

    #[test]
    fn validate_rejects_wrong_type() {
        let r = fetcher_role();
        let params = json!({"url": 42, "workspace": "/tmp/x.html"});
        let err = r.validate_params(&params).unwrap_err();
        assert!(err.contains("url"), "err was: {}", err);
        assert!(err.contains("string"), "err was: {}", err);
    }

    #[test]
    fn validate_rejects_unknown_param() {
        let r = fetcher_role();
        let params = json!({
            "url": "https://x.com",
            "workspace": "/tmp/x.html",
            "bogus": "nope"
        });
        let err = r.validate_params(&params).unwrap_err();
        assert!(err.contains("bogus"), "err was: {}", err);
        assert!(err.contains("unknown"), "err was: {}", err);
    }

    #[test]
    fn validate_string_list_requires_array_of_strings() {
        let r: Role = serde_yaml::from_str(
            r#"
name: writer
model: deepseek-chat
parameters:
  inputs:
    type: string_list
    required: true
    description: files
capabilities: []
system_prompt: "x"
message_template: "x"
budget: { max_input_tokens: 1000, max_output_tokens: 500 }
retry: { max_attempts: 1, on: [] }
"#,
        )
        .unwrap();
        r.validate_params(&json!({"inputs": ["/a", "/b"]})).unwrap();
        assert!(r.validate_params(&json!({"inputs": "single"})).is_err());
        assert!(r.validate_params(&json!({"inputs": [1, 2]})).is_err());
    }

    #[test]
    fn render_message_substitutes_string_params() {
        let r = fetcher_role();
        let params = json!({"url": "https://x.com", "workspace": "/tmp/x.html"});
        let m = r.render_message(&params);
        assert!(m.contains("https://x.com"));
        assert!(m.contains("/tmp/x.html"));
    }

    #[test]
    fn render_message_formats_string_list_as_comma_sep() {
        let r: Role = serde_yaml::from_str(
            r#"
name: writer
model: deepseek-chat
parameters:
  inputs:
    type: string_list
    required: true
    description: inputs
  output:
    type: path
    required: true
    description: out
  title:
    type: string
    required: true
    description: title
capabilities: []
system_prompt: "x"
message_template: "title={title} out={output} inputs={inputs}"
budget: { max_input_tokens: 1000, max_output_tokens: 500 }
retry: { max_attempts: 1, on: [] }
"#,
        )
        .unwrap();
        let params = json!({
            "inputs": ["/a.html", "/b.html"],
            "output": "/out.md",
            "title": "Report"
        });
        let m = r.render_message(&params);
        assert!(m.contains("title=Report"));
        assert!(m.contains("out=/out.md"));
        assert!(m.contains("/a.html, /b.html"), "got: {}", m);
    }

    #[test]
    fn render_manifest_produces_parseable_agent_manifest() {
        let r: Role = serde_yaml::from_str(
            r#"
name: fetcher
model: deepseek-chat
parameters:
  url:
    type: string
    required: true
    description: url
  workspace:
    type: path
    required: true
    description: ws
capabilities:
  - "tool: web_fetch"
  - "file_write: {workspace}"
system_prompt: "fetcher prompt"
message_template: "fetch {url} to {workspace}"
budget: { max_input_tokens: 1000, max_output_tokens: 500 }
retry: { max_attempts: 1, on: [] }
"#,
        )
        .unwrap();
        let params = json!({"url": "https://x.com", "workspace": "/tmp/x.html"});
        let yaml = r.render_manifest(&params);
        let manifest = aaos_core::AgentManifest::from_yaml(&yaml).unwrap();
        assert_eq!(manifest.name, "fetcher");
        assert_eq!(manifest.capabilities.len(), 2);
        // Ensure that {workspace} was substituted correctly
        assert!(yaml.contains("/tmp/x.html"));
        assert!(!yaml.contains("{workspace}"), "yaml: {}", yaml);
    }
}
