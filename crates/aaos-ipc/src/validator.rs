use aaos_core::{CoreError, Result};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::RwLock;

/// Validates message payloads against registered JSON schemas.
///
/// Every tool and method in aaOS has a declared schema. The validator
/// ensures that all messages conform to their schema before delivery.
pub struct SchemaValidator {
    schemas: RwLock<HashMap<String, Value>>,
}

impl SchemaValidator {
    pub fn new() -> Self {
        Self {
            schemas: RwLock::new(HashMap::new()),
        }
    }

    /// Register a JSON schema for a method.
    pub fn register_schema(&self, method: &str, schema: Value) {
        self.schemas
            .write()
            .unwrap()
            .insert(method.to_string(), schema);
    }

    /// Validate a payload against the schema for a method.
    /// If no schema is registered for the method, validation passes.
    pub fn validate(&self, method: &str, payload: &Value) -> Result<()> {
        let schemas = self.schemas.read().unwrap();
        let Some(schema) = schemas.get(method) else {
            return Ok(()); // No schema registered = accept anything
        };

        // Basic type-level validation using the schema
        // Full JSON Schema validation would use the `jsonschema` crate,
        // but for the initial scaffold we do structural matching.
        self.validate_against_schema(payload, schema)
    }

    fn validate_against_schema(&self, payload: &Value, schema: &Value) -> Result<()> {
        match schema.get("type").and_then(|t| t.as_str()) {
            Some("object") => {
                if !payload.is_object() {
                    return Err(CoreError::SchemaValidation("expected object".into()));
                }
                // Check required fields
                if let Some(required) = schema.get("required").and_then(|r| r.as_array()) {
                    let obj = payload.as_object().unwrap();
                    for field in required {
                        if let Some(name) = field.as_str() {
                            if !obj.contains_key(name) {
                                return Err(CoreError::SchemaValidation(format!(
                                    "missing required field: {name}"
                                )));
                            }
                        }
                    }
                }
            }
            Some("array") => {
                if !payload.is_array() {
                    return Err(CoreError::SchemaValidation("expected array".into()));
                }
            }
            Some("string") => {
                if !payload.is_string() {
                    return Err(CoreError::SchemaValidation("expected string".into()));
                }
            }
            Some("number") | Some("integer") => {
                if !payload.is_number() {
                    return Err(CoreError::SchemaValidation("expected number".into()));
                }
            }
            _ => {} // No type constraint or unknown type = accept
        }
        Ok(())
    }

    /// List all registered method schemas.
    pub fn methods(&self) -> Vec<String> {
        self.schemas.read().unwrap().keys().cloned().collect()
    }
}

impl Default for SchemaValidator {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn validates_required_fields() {
        let validator = SchemaValidator::new();
        validator.register_schema(
            "tools/call",
            json!({
                "type": "object",
                "required": ["name", "arguments"]
            }),
        );

        // Valid
        let result = validator.validate("tools/call", &json!({"name": "search", "arguments": {}}));
        assert!(result.is_ok());

        // Missing required field
        let result = validator.validate("tools/call", &json!({"name": "search"}));
        assert!(result.is_err());
    }

    #[test]
    fn validates_type() {
        let validator = SchemaValidator::new();
        validator.register_schema("test", json!({"type": "object"}));

        assert!(validator.validate("test", &json!({})).is_ok());
        assert!(validator.validate("test", &json!("string")).is_err());
    }

    #[test]
    fn unregistered_method_passes() {
        let validator = SchemaValidator::new();
        assert!(validator.validate("unknown", &json!("anything")).is_ok());
    }
}
