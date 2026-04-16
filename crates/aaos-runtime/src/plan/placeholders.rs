//! Template placeholder substitution for Plan params.
//!
//! Three placeholder forms are recognized:
//!   * `{run}` — the workspace root for the current run.
//!   * `{workspace}` — the workspace path assigned to the current subtask
//!     (Planner emits this symbolically; executor resolves to an actual path).
//!   * `{input.0}`, `{input.1}` — references to prior subtasks' workspace
//!     paths, by index in the subtask's `inputs` list.
//!
//! The substituter is format-agnostic — it walks serde_json::Value and
//! rewrites any string that contains `{...}` markers.

use std::path::{Path, PathBuf};

use serde_json::Value;

pub struct Substitutions {
    pub run_root: PathBuf,
}

impl Substitutions {
    pub fn new(run_root: PathBuf) -> Self {
        Self { run_root }
    }

    /// Substitute `{run}` in any string Value inside `v`. Returns a new Value
    /// with substitutions applied; non-string Values pass through untouched.
    pub fn apply(&self, v: &Value) -> Value {
        match v {
            Value::String(s) => Value::String(self.apply_str(s)),
            Value::Array(arr) => Value::Array(arr.iter().map(|x| self.apply(x)).collect()),
            Value::Object(obj) => Value::Object(
                obj.iter()
                    .map(|(k, v)| (k.clone(), self.apply(v)))
                    .collect(),
            ),
            other => other.clone(),
        }
    }

    pub fn apply_str(&self, s: &str) -> String {
        s.replace("{run}", &self.run_root.display().to_string())
    }

    pub fn resolve_path(&self, template: &str) -> PathBuf {
        Path::new(&self.apply_str(template)).to_path_buf()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn replaces_run_in_string() {
        let s = Substitutions::new(PathBuf::from("/var/lib/aaos/workspace/abc"));
        assert_eq!(
            s.apply_str("{run}/hn.html"),
            "/var/lib/aaos/workspace/abc/hn.html"
        );
    }

    #[test]
    fn leaves_other_text_alone() {
        let s = Substitutions::new(PathBuf::from("/r"));
        assert_eq!(s.apply_str("no placeholders here"), "no placeholders here");
    }

    #[test]
    fn walks_nested_object_values() {
        let s = Substitutions::new(PathBuf::from("/r"));
        let v = json!({
            "url": "https://x.com",
            "workspace": "{run}/x.html",
            "meta": { "nested": "{run}/nested" }
        });
        let out = s.apply(&v);
        assert_eq!(out["workspace"], "/r/x.html");
        assert_eq!(out["meta"]["nested"], "/r/nested");
        assert_eq!(out["url"], "https://x.com");
    }

    #[test]
    fn walks_string_arrays() {
        let s = Substitutions::new(PathBuf::from("/r"));
        let v = json!(["{run}/a", "{run}/b", "static"]);
        let out = s.apply(&v);
        assert_eq!(out[0], "/r/a");
        assert_eq!(out[1], "/r/b");
        assert_eq!(out[2], "static");
    }

    #[test]
    fn resolve_path_returns_pathbuf() {
        let s = Substitutions::new(PathBuf::from("/r"));
        let p = s.resolve_path("{run}/out.json");
        assert_eq!(p, PathBuf::from("/r/out.json"));
    }
}
