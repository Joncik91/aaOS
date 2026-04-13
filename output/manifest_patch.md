# Manifest Patch for Phase E3 Token Budget Enforcement

## File: `/src/crates/aaos-core/src/manifest.rs`

### Changes Required:

#### 1. Add BudgetConfig to imports and define it
Add near the top of the file (after the other imports):

```rust
// Add this import if needed for BudgetConfig
use crate::budget::BudgetConfig;
```

Or define BudgetConfig directly in manifest.rs. Since BudgetConfig is defined in `budget.rs`, we need to import it. The better approach is to define it in `budget.rs` and import it here.

#### 2. Add budget_config field to AgentManifest struct
Add to the AgentManifest struct:

```rust
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
    // Phase E3: Token budget configuration
    #[serde(default)]
    pub budget_config: Option<BudgetConfig>,
}
```

#### 3. Update the manifest parsing tests
Add tests for parsing manifests with budget configuration:

```rust
#[test]
fn parse_manifest_with_budget_config() {
    let yaml = r#"
name: budget-agent
model: claude-haiku-4-5-20251001
system_prompt: "test"
budget_config:
  max_tokens: 10000
  reset_period_seconds: 3600
capabilities:
  - web_search
"#;
    let manifest = AgentManifest::from_yaml(yaml).unwrap();
    assert_eq!(manifest.name, "budget-agent");
    assert!(manifest.budget_config.is_some());
    let budget_config = manifest.budget_config.unwrap();
    assert_eq!(budget_config.max_tokens, 10000);
    assert_eq!(budget_config.reset_period_seconds, 3600);
}

#[test]
fn parse_manifest_without_budget_config() {
    let yaml = r#"
name: no-budget-agent
model: claude-haiku-4-5-20251001
system_prompt: "test"
"#;
    let manifest = AgentManifest::from_yaml(yaml).unwrap();
    assert_eq!(manifest.name, "no-budget-agent");
    assert!(manifest.budget_config.is_none());
}

#[test]
fn manifest_roundtrips_yaml_with_budget() {
    let yaml = r#"
name: budget-roundtrip
model: test-model
system_prompt: "test"
budget_config:
  max_tokens: 5000
  reset_period_seconds: 1800
"#;
    let manifest = AgentManifest::from_yaml(yaml).unwrap();
    let serialized = serde_yaml::to_string(&manifest).unwrap();
    let reparsed = AgentManifest::from_yaml(&serialized).unwrap();
    assert_eq!(manifest.name, reparsed.name);
    assert!(reparsed.budget_config.is_some());
    assert_eq!(reparsed.budget_config.unwrap().max_tokens, 5000);
}
```

### Complete Updated AgentManifest Struct:

Here's the complete AgentManifest struct with the new budget_config field:

```rust
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
    /// Phase E3: Token budget configuration for this agent.
    /// If not specified, no budget enforcement is applied.
    #[serde(default)]
    pub budget_config: Option<BudgetConfig>,
}
```

### Key Design Points:

1. **Optional Field**: The `budget_config` is an `Option<BudgetConfig>` with `#[serde(default)]`, making it completely optional for backward compatibility.

2. **Backward Compatibility**: Existing manifests without `budget_config` will continue to work unchanged.

3. **Default Behavior**: When `budget_config` is `None`, no budget enforcement occurs.

4. **Serialization**: The field uses standard serde serialization, making it work with both YAML and JSON formats.

5. **Validation**: No special validation needed beyond what `BudgetConfig` already provides (e.g., `max_tokens > 0`).

### Example YAML Manifests:

#### With Budget Configuration:
```yaml
name: research-agent
model: claude-3-5-sonnet-20241022
system_prompt: "You are a research assistant."
budget_config:
  max_tokens: 1000000  # 1 million tokens
  reset_period_seconds: 86400  # 24 hours (daily reset)
capabilities:
  - web_search
  - "file_read: /data/research/*"
lifecycle: on-demand
```

#### Without Budget Configuration (backward compatible):
```yaml
name: simple-agent
model: claude-haiku-4-5-20251001
system_prompt: "You are a helpful assistant."
capabilities:
  - "tool: echo"
```

### Integration with Existing System:

The `budget_config` field integrates naturally with the existing manifest system:

1. **Parsing**: Automatically parsed from YAML by serde
2. **Validation**: Included in `AgentManifest::validate()` if needed
3. **Storage**: Stored as part of the agent's manifest
4. **Retrieval**: Accessible via `agent.manifest.budget_config`
5. **Usage**: Used to create `BudgetTracker` instance in `AgentProcess::new()`

### Migration Path:

1. **Phase 1**: Add the field with `Option` and `#[serde(default)]`
2. **Phase 2**: Update `AgentProcess` to use the budget config
3. **Phase 3**: Update `InProcessAgentServices::report_usage()` to check budget
4. **Phase 4**: Gradually add budget configurations to manifests as needed

This approach ensures a smooth transition and allows teams to adopt budget enforcement at their own pace.