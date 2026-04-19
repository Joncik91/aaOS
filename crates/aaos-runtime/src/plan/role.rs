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

/// Retry / iteration configuration for a role.
///
/// `max_attempts` is the number of times the executor will retry a failed
/// tool call within a single subtask, filtered by the `on` list (e.g.
/// `["tool_error", "timeout"]`). The LLM-loop iteration cap — the hard
/// ceiling on how many turns an agent can take inside one subtask — is
/// derived from this by the executor as `max(max_attempts + 10, 10)` (see
/// `PlanExecutor::spawn_subtask`). The extra ten-turn headroom covers
/// setup and verification turns that happen around the retry-eligible
/// tool calls, so bumping `max_attempts` by N yields N+10 more turns on
/// the first bump and N thereafter.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoleRetry {
    pub max_attempts: u32,
    #[serde(default)]
    pub on: Vec<String>,
}

/// Opt-in runtime-side execution mode for roles whose work is purely
/// mechanical (fetch-and-write, read-and-transform, etc.). When set, the
/// PlanExecutor skips the LLM loop entirely and dispatches the role's work
/// via deterministic Rust code keyed on the `kind` string. This closes the
/// "LLM emits a plausible ack without actually calling the tool" failure
/// mode observed for LLM-powered fetchers.
///
/// Current kinds:
///   * "fetcher" — web_fetch(url) → file_write(workspace, body) → return
///     workspace. See PlanExecutor::run_scaffold.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RoleScaffold {
    pub kind: String,
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
    /// Scheduling-priority hint. Lower numbers get their turn earlier when
    /// two subtasks share the same TTL deadline. Missing in YAML = 128
    /// (mid-bucket). Roles that produce critical-path work (writer,
    /// analyzer) can declare e.g. `priority: 64`.
    #[serde(default = "default_role_priority")]
    pub priority: u8,
    /// Ordered list of models to try for a subtask in this role. Tier 0 is
    /// `role.model`; escalation walks up the ladder on failure signals.
    /// Missing or empty = single-tier routing with `role.model`.
    #[serde(default)]
    pub model_ladder: Vec<String>,

    /// Which observable signals trigger escalation to the next tier on
    /// replan. Missing = all three signals active (see
    /// `default_escalation_signals`).
    #[serde(default = "crate::plan::escalation::default_escalation_signals")]
    pub escalate_on: Vec<crate::plan::EscalationSignal>,
    /// When present, the runtime executes this role via a deterministic
    /// scaffold instead of an LLM loop. Absence (None) = LLM-powered role.
    #[serde(default)]
    pub scaffold: Option<RoleScaffold>,
}

fn default_role_priority() -> u8 {
    128
}

impl Role {
    /// Validate submitted Plan parameters against this role's schema.
    ///
    /// Returns a single human-friendly error describing all problems so the
    /// Planner's replan loop can correct them in one shot rather than having
    /// to iterate on one error at a time.
    pub fn validate_params(&self, params: &serde_json::Value) -> Result<(), String> {
        let obj = params.as_object().ok_or_else(|| {
            format!(
                "role '{}' expected params object, got non-object",
                self.name
            )
        })?;

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
    /// `{output}`) are substituted; array-valued patterns (`{inputs.*}`) expand
    /// into one capability per element. Anything else is emitted verbatim. The
    /// result is parseable via `AgentManifest::from_yaml`.
    pub fn render_manifest(&self, params: &serde_json::Value) -> String {
        self.render_manifest_with_model(&self.model, params)
    }

    /// Canonical model ladder. Substitutes `[role.model]` when
    /// `model_ladder` is empty; validates `model_ladder[0] == role.model`
    /// when non-empty. Returns a human-readable error on drift; the
    /// catalog loader surfaces these at startup.
    pub fn resolved_ladder(&self) -> Result<Vec<String>, String> {
        if self.model_ladder.is_empty() {
            return Ok(vec![self.model.clone()]);
        }
        if self.model_ladder[0] != self.model {
            return Err(format!(
                "role '{}': model_ladder[0] = '{}' but model = '{}' — the two must match (model is the display/back-compat field; the ladder drives routing)",
                self.name, self.model_ladder[0], self.model
            ));
        }
        Ok(self.model_ladder.clone())
    }

    /// Render a manifest targeting a specific model. Used by the executor
    /// when `subtask.current_model_tier > 0` so the tier-bumped subtask
    /// gets a manifest naming the escalated model instead of the tier-0
    /// default baked into `render_manifest`.
    pub fn render_manifest_with_model(&self, model: &str, params: &serde_json::Value) -> String {
        let caps: Vec<String> = self
            .capabilities
            .iter()
            .flat_map(|c| expand_capability(c, params))
            .collect();
        let caps_yaml: String = caps
            .iter()
            .map(|c| format!("  - \"{}\"\n", c.replace('"', "\\\"")))
            .collect();
        format!(
            "name: {name}\nmodel: {model}\nsystem_prompt: |\n{prompt}\ncapabilities:\n{caps}",
            name = self.name,
            model = model,
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
            let entry = entry.map_err(|e| RoleCatalogError::Io(format!("dir entry error: {e}")))?;
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) != Some("yaml") {
                continue;
            }
            let contents = std::fs::read_to_string(&path)
                .map_err(|e| RoleCatalogError::Io(format!("read {}: {e}", path.display())))?;
            let role: Role = serde_yaml::from_str(&contents)
                .map_err(|e| RoleCatalogError::Parse(format!("parse {}: {e}", path.display())))?;
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

        // Validate model_ladder / model consistency. Cheap walk after
        // parsing, surfaces operator-visible drift at startup rather
        // than at first spawn. (Phase F-b sub-project 2.)
        for role in roles.values() {
            role.resolved_ladder().map_err(RoleCatalogError::Parse)?;
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
        serde_json::Value::Array(arr) => {
            arr.iter().map(render_value).collect::<Vec<_>>().join(", ")
        }
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

/// Expand one capability template into zero-or-more concrete capability
/// strings. Handles the `{name.*}` array-expansion syntax used by roles
/// whose input paths are plural (e.g. the writer's `file_read: {inputs.*}`):
/// for each string in the matching array param, emit a copy of the template
/// with `{name.*}` replaced by that element.
///
/// Templates without any `{name.*}` token fall through to plain
/// `substitute_tokens` and produce exactly one string.
///
/// Edge cases:
///   * Param missing → zero entries emitted (the capability silently drops;
///     the runtime's param-validation step has already run by the time this
///     is called, so missing required params are impossible).
///   * Param present but not an array → zero entries. A future version
///     could error, but for now this keeps role YAMLs additive.
///   * Array present but empty → zero entries (correct: no paths, no grants).
fn expand_capability(template: &str, params: &serde_json::Value) -> Vec<String> {
    // Find any `{name.*}` occurrence. Only the first such token drives
    // expansion; the template may still contain other `{name}` tokens
    // handled by substitute_tokens below.
    if let Some((start, end, name)) = find_array_token(template) {
        let Some(arr) = params
            .as_object()
            .and_then(|obj| obj.get(&name))
            .and_then(|v| v.as_array())
        else {
            // Not an array param — drop the capability rather than emit
            // a literal `{name.*}` that won't match anything.
            return Vec::new();
        };
        return arr
            .iter()
            .filter_map(|elem| elem.as_str())
            .map(|elem_str| {
                // Replace `{name.*}` with this element, then run ordinary
                // substitution for any other `{name}` tokens.
                let mut out = String::with_capacity(template.len());
                out.push_str(&template[..start]);
                out.push_str(elem_str);
                out.push_str(&template[end..]);
                substitute_tokens(&out, params)
            })
            .filter(|rendered| !has_unresolved_template_token(rendered))
            .collect();
    }

    // No array expansion — fall back to single-substitution behavior.
    let rendered = substitute_tokens(template, params);
    if has_unresolved_template_token(&rendered) {
        // Dropping the capability is safer than granting the LLM a literal
        // `{placeholder}` as a file path (soak-test Bug 5 from 2026-04-19:
        // the generalist role emitted `file_write: {workspace}` when
        // workspace was unset, and downstream tools then wrote to a file
        // literally named `{workspace}`). A missing-param capability
        // almost certainly means the operator's plan forgot to set that
        // param; silently dropping + logging lets the rest of the role's
        // capabilities still take effect.
        tracing::warn!(
            template = %template,
            "role capability has unresolved template token after substitution; \
             dropping capability. Missing param in plan? Template: {template}"
        );
        return Vec::new();
    }
    vec![rendered]
}

/// True if `s` contains a `{name}` token that no substitution resolved.
/// Used by `expand_capability` to drop capabilities that reference params
/// the plan didn't set, rather than emit a literal template string that
/// would later be interpreted as a real path.
fn has_unresolved_template_token(s: &str) -> bool {
    // Any `{` followed by a closing `}` with non-empty content between
    // means a token survived substitution. False positives on literal
    // braces in prose are acceptable — role capability strings are
    // structured (prefix + path) and don't legitimately contain `{...}`
    // except as template tokens.
    let Some(open) = s.find('{') else {
        return false;
    };
    let Some(close_rel) = s[open + 1..].find('}') else {
        return false;
    };
    close_rel > 0 // non-empty inside the braces
}

/// Locate a `{name.*}` token in `s`. Returns `(start, end, name)` where
/// `start..end` is the byte range of the token including the braces.
fn find_array_token(s: &str) -> Option<(usize, usize, String)> {
    let open = s.find("{")?;
    let close = s[open..].find("}").map(|o| open + o + 1)?;
    let inner = &s[open + 1..close - 1];
    if let Some(name) = inner.strip_suffix(".*") {
        if !name.is_empty() {
            return Some((open, close, name.to_string()));
        }
    }
    // This token wasn't an array token — look deeper in the string.
    let rest_offset = close;
    if let Some((rstart, rend, name)) = find_array_token(&s[rest_offset..]) {
        return Some((rest_offset + rstart, rest_offset + rend, name));
    }
    None
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
        assert_eq!(role.parameters["workspace"].param_type, ParameterType::Path);
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

    #[test]
    fn scaffold_field_defaults_to_none_for_llm_roles() {
        let r = fetcher_role();
        assert!(r.scaffold.is_none());
    }

    #[test]
    fn scaffold_field_parses_when_present() {
        let yaml = format!("{}\nscaffold:\n  kind: fetcher\n", FETCHER_YAML.trim_end());
        let r: Role = serde_yaml::from_str(&yaml).unwrap();
        let s = r.scaffold.expect("scaffold should parse");
        assert_eq!(s.kind, "fetcher");
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

    // ----- {inputs.*} array-expansion tests (2026-04-17 fix) ---------------

    fn writer_role_with_inputs_splat() -> Role {
        serde_yaml::from_str(
            r#"
name: writer
model: deepseek-chat
parameters:
  inputs:
    type: string_list
    required: true
    description: input files
  output:
    type: path
    required: true
    description: output file
  title:
    type: string
    required: true
    description: title
capabilities:
  - "tool: file_read"
  - "tool: file_write"
  - "file_read: {inputs.*}"
  - "file_write: {output}"
system_prompt: "writer"
message_template: "write {title} to {output} from {inputs}"
budget: { max_input_tokens: 1000, max_output_tokens: 500 }
retry: { max_attempts: 1, on: [] }
"#,
        )
        .unwrap()
    }

    fn cap_is(c: &aaos_core::CapabilityDeclaration, want: &str) -> bool {
        matches!(c, aaos_core::CapabilityDeclaration::Simple(s) if s == want)
    }
    fn cap_starts_with(c: &aaos_core::CapabilityDeclaration, prefix: &str) -> bool {
        matches!(c, aaos_core::CapabilityDeclaration::Simple(s) if s.starts_with(prefix))
    }

    #[test]
    fn render_manifest_expands_inputs_splat_to_one_cap_per_file() {
        let r = writer_role_with_inputs_splat();
        let params = json!({
            "inputs": ["/a.html", "/b.html"],
            "output": "/out.md",
            "title": "Report"
        });
        let yaml = r.render_manifest(&params);
        let manifest = aaos_core::AgentManifest::from_yaml(&yaml).unwrap();

        // Expected: tool:file_read, tool:file_write, file_read:/a.html,
        // file_read:/b.html, file_write:/out.md — five capabilities.
        assert_eq!(
            manifest.capabilities.len(),
            5,
            "caps: {:?}",
            manifest.capabilities
        );
        assert!(manifest
            .capabilities
            .iter()
            .any(|c| cap_is(c, "file_read: /a.html")));
        assert!(manifest
            .capabilities
            .iter()
            .any(|c| cap_is(c, "file_read: /b.html")));
        assert!(manifest
            .capabilities
            .iter()
            .any(|c| cap_is(c, "file_write: /out.md")));
        // No stray placeholder survived.
        assert!(!yaml.contains("{inputs.*}"), "yaml: {}", yaml);
        assert!(!yaml.contains("{inputs}"), "yaml: {}", yaml);
    }

    #[test]
    fn render_manifest_expands_single_element_array() {
        let r = writer_role_with_inputs_splat();
        let params = json!({
            "inputs": ["/only.html"],
            "output": "/out.md",
            "title": "t"
        });
        let yaml = r.render_manifest(&params);
        let manifest = aaos_core::AgentManifest::from_yaml(&yaml).unwrap();
        assert_eq!(manifest.capabilities.len(), 4);
        assert!(manifest
            .capabilities
            .iter()
            .any(|c| cap_is(c, "file_read: /only.html")));
    }

    #[test]
    fn render_manifest_empty_inputs_drops_splat_caps() {
        let r = writer_role_with_inputs_splat();
        let params = json!({
            "inputs": [],
            "output": "/out.md",
            "title": "t"
        });
        let yaml = r.render_manifest(&params);
        let manifest = aaos_core::AgentManifest::from_yaml(&yaml).unwrap();
        // tool:file_read, tool:file_write, file_write:/out.md — 3. No file_read:/path.
        assert_eq!(manifest.capabilities.len(), 3);
        assert!(!yaml.contains("{inputs.*}"));
        assert!(!manifest
            .capabilities
            .iter()
            .any(|c| cap_starts_with(c, "file_read: /")));
    }

    #[test]
    fn find_array_token_finds_first_splat() {
        let got = find_array_token("file_read: {inputs.*}");
        assert!(got.is_some());
        let (start, end, name) = got.unwrap();
        assert_eq!(&"file_read: {inputs.*}"[start..end], "{inputs.*}");
        assert_eq!(name, "inputs");
    }

    #[test]
    fn find_array_token_skips_regular_placeholder() {
        let got = find_array_token("file_write: {output}");
        assert!(got.is_none(), "must not match non-splat token");
    }

    #[test]
    fn find_array_token_finds_splat_after_regular_token() {
        // `{output}` comes first, `{inputs.*}` second. The splat finder
        // must walk past the regular placeholder to find the real one.
        let got = find_array_token("write {output} but read {inputs.*}");
        assert!(got.is_some());
        let (_, _, name) = got.unwrap();
        assert_eq!(name, "inputs");
    }

    #[test]
    fn role_priority_defaults_to_128() {
        let yaml = r#"
name: r
model: claude-haiku-4-5-20251001
system_prompt: "x"
message_template: "y"
budget: { max_input_tokens: 1000, max_output_tokens: 1000 }
retry: { max_attempts: 1 }
"#;
        let role: Role = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(
            role.priority, 128,
            "missing priority defaults to 128 (mid-bucket)"
        );
    }

    #[test]
    fn role_priority_explicit_value_roundtrips() {
        let yaml = r#"
name: r
model: claude-haiku-4-5-20251001
system_prompt: "x"
message_template: "y"
budget: { max_input_tokens: 1000, max_output_tokens: 1000 }
retry: { max_attempts: 1 }
priority: 64
"#;
        let role: Role = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(role.priority, 64, "explicit priority must be preserved");
    }

    #[test]
    fn resolved_ladder_missing_field_returns_single_element() {
        let yaml = r#"
name: r
model: deepseek-chat
system_prompt: "x"
message_template: "y"
budget: { max_input_tokens: 1000, max_output_tokens: 1000 }
retry: { max_attempts: 1 }
"#;
        let role: Role = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(
            role.resolved_ladder().unwrap(),
            vec!["deepseek-chat".to_string()]
        );
    }

    #[test]
    fn resolved_ladder_explicit_two_tier() {
        let yaml = r#"
name: r
model: deepseek-chat
model_ladder:
  - deepseek-chat
  - deepseek-reasoner
system_prompt: "x"
message_template: "y"
budget: { max_input_tokens: 1000, max_output_tokens: 1000 }
retry: { max_attempts: 1 }
"#;
        let role: Role = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(
            role.resolved_ladder().unwrap(),
            vec!["deepseek-chat".to_string(), "deepseek-reasoner".to_string()]
        );
    }

    #[test]
    fn resolved_ladder_rejects_drift_between_model_and_first_tier() {
        let yaml = r#"
name: r
model: deepseek-chat
model_ladder:
  - deepseek-reasoner
  - claude-opus-4
system_prompt: "x"
message_template: "y"
budget: { max_input_tokens: 1000, max_output_tokens: 1000 }
retry: { max_attempts: 1 }
"#;
        let role: Role = serde_yaml::from_str(yaml).unwrap();
        let err = role.resolved_ladder().unwrap_err();
        assert!(
            err.contains("deepseek-chat") && err.contains("deepseek-reasoner"),
            "error must name both drifted values; got: {err}"
        );
    }

    #[test]
    fn escalate_on_defaults_to_all_three_signals() {
        use crate::plan::EscalationSignal;
        let yaml = r#"
name: r
model: deepseek-chat
system_prompt: "x"
message_template: "y"
budget: { max_input_tokens: 1000, max_output_tokens: 1000 }
retry: { max_attempts: 1 }
"#;
        let role: Role = serde_yaml::from_str(yaml).unwrap();
        assert!(role.escalate_on.contains(&EscalationSignal::ReplanRetry));
        assert!(role
            .escalate_on
            .contains(&EscalationSignal::ToolRepeatGuard));
        assert!(role.escalate_on.contains(&EscalationSignal::MaxTokens));
    }

    #[test]
    fn escalate_on_explicit_subset() {
        use crate::plan::EscalationSignal;
        let yaml = r#"
name: r
model: deepseek-chat
escalate_on:
  - max_tokens
system_prompt: "x"
message_template: "y"
budget: { max_input_tokens: 1000, max_output_tokens: 1000 }
retry: { max_attempts: 1 }
"#;
        let role: Role = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(role.escalate_on, vec![EscalationSignal::MaxTokens]);
    }

    // --- Unresolved-template-token drop (soak-test Bug 5) ---

    #[test]
    fn expand_capability_drops_unresolved_template() {
        // workspace param is missing from params → the capability
        // `file_write: {workspace}` should drop, not emit a literal.
        let params = serde_json::json!({ "task_description": "some goal" });
        let caps = expand_capability("file_write: {workspace}", &params);
        assert!(
            caps.is_empty(),
            "unresolved {{workspace}} must drop, not emit literal: {caps:?}"
        );
    }

    #[test]
    fn expand_capability_keeps_resolved_template() {
        let params = serde_json::json!({ "workspace": "/tmp/run-x/out.md" });
        let caps = expand_capability("file_write: {workspace}", &params);
        assert_eq!(caps, vec!["file_write: /tmp/run-x/out.md"]);
    }

    #[test]
    fn expand_capability_keeps_no_template_literal() {
        let params = serde_json::json!({});
        let caps = expand_capability("tool: web_fetch", &params);
        assert_eq!(caps, vec!["tool: web_fetch"]);
    }

    #[test]
    fn render_manifest_omits_caps_with_missing_params() {
        // Mirror the real generalist bug: template says `{workspace}` but
        // caller passes only `task_description`. Rendered manifest must
        // NOT carry `file_write: {workspace}` as a grant.
        let role = Role {
            name: "generalist".into(),
            model: "deepseek-chat".into(),
            model_ladder: vec![],
            escalate_on: vec![],
            parameters: Default::default(),
            capabilities: vec!["tool: file_write".into(), "file_write: {workspace}".into()],
            system_prompt: "x".into(),
            message_template: "y".into(),
            budget: RoleBudget {
                max_input_tokens: 1000,
                max_output_tokens: 1000,
            },
            retry: RoleRetry {
                max_attempts: 1,
                on: vec![],
            },
            scaffold: None,
            priority: 128,
        };
        let yaml = role.render_manifest(&serde_json::json!({}));
        assert!(
            !yaml.contains("{workspace}"),
            "rendered manifest must not carry literal {{workspace}}: {yaml}"
        );
        assert!(
            yaml.contains("tool: file_write"),
            "other capabilities must still render: {yaml}"
        );
    }

    #[test]
    fn has_unresolved_template_token_cases() {
        assert!(has_unresolved_template_token("file_write: {workspace}"));
        assert!(has_unresolved_template_token("/some/{placeholder}/path.md"));
        assert!(!has_unresolved_template_token("/resolved/path.md"));
        assert!(!has_unresolved_template_token("tool: web_fetch"));
        // Empty braces aren't a real template token; don't drop for them.
        assert!(!has_unresolved_template_token("literal {} braces"));
    }
}
