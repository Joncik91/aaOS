# Services Patch for Phase E3 Token Budget Enforcement

## File: `/src/crates/aaos-runtime/src/services.rs`

### Changes Required:

#### 1. Add BudgetTracker to AgentProcess
First, we need to store BudgetTracker in AgentProcess (in `/src/crates/aaos-runtime/src/process.rs`):

```rust
// In process.rs
use aaos_core::budget::{BudgetConfig, BudgetTracker};
use std::sync::Arc;

pub struct AgentProcess {
    // ... existing fields ...
    pub budget_tracker: Option<Arc<BudgetTracker>>,
}

impl AgentProcess {
    pub fn new(id: AgentId, manifest: AgentManifest, capabilities: Vec<CapabilityToken>) -> Self {
        // Create budget tracker from manifest config
        let budget_tracker = manifest.budget_config.as_ref()
            .map(|config| Arc::new(BudgetTracker::new(*config)));
        
        Self {
            // ... existing fields ...
            budget_tracker,
        }
    }
    
    pub fn track_token_usage(&self, usage: &TokenUsage) -> Result<(), CoreError> {
        if let Some(tracker) = &self.budget_tracker {
            let total_tokens = usage.input_tokens + usage.output_tokens;
            if total_tokens > 0 {
                tracker.track(total_tokens).map_err(|e| {
                    CoreError::BudgetExceeded(format!(
                        "Agent {}: {}",
                        self.id,
                        e
                    ))
                })?;
            }
        }
        Ok(())
    }
}
```

#### 2. Add Budget Tracking Method to AgentRegistry
Add a method to AgentRegistry to track token usage:

```rust
// In registry.rs
impl AgentRegistry {
    // ... existing methods ...
    
    pub fn track_token_usage(&self, agent_id: AgentId, usage: TokenUsage) -> Result<()> {
        let mut entry = self.agents
            .get_mut(&agent_id)
            .ok_or(CoreError::AgentNotFound(agent_id))?;
        
        entry.value_mut().track_token_usage(&usage)
    }
}
```

#### 3. Update InProcessAgentServices::report_usage()
Update the report_usage method to check budget:

```rust
// In services.rs, in the InProcessAgentServices implementation
#[async_trait]
impl AgentServices for InProcessAgentServices {
    async fn report_usage(&self, agent_id: AgentId, usage: TokenUsage) -> Result<()> {
        // Phase E3: Check token budget before logging
        self.registry.track_token_usage(agent_id, usage.clone())?;
        
        // Existing audit logging (from Phase E2)
        self.audit_log.record(AuditEvent::new(
            agent_id,
            AuditEventKind::UsageReported {
                input_tokens: usage.input_tokens,
                output_tokens: usage.output_tokens,
            },
        ));
        Ok(())
    }
    
    // ... other methods unchanged ...
}
```

#### 4. Add BudgetExceeded Error Variant
Add to `/src/crates/aaos-core/src/error.rs`:

```rust
#[derive(Debug, Error)]
pub enum CoreError {
    // ... existing variants ...
    
    #[error("Token budget exceeded: {0}")]
    BudgetExceeded(String),
}
```

#### 5. Import Budget Types
Add imports at the top of services.rs:

```rust
// Add to imports section
use aaos_core::budget::{BudgetConfig, BudgetTracker};
```

## Complete Updated report_usage Method:

Here's the complete updated `report_usage` method with budget enforcement:

```rust
async fn report_usage(&self, agent_id: AgentId, usage: TokenUsage) -> Result<()> {
    // Phase E3: Budget enforcement - check if agent exceeds token budget
    self.registry.track_token_usage(agent_id, usage.clone())?;
    
    // Phase E2: Audit logging (existing functionality)
    self.audit_log.record(AuditEvent::new(
        agent_id,
        AuditEventKind::UsageReported {
            input_tokens: usage.input_tokens,
            output_tokens: usage.output_tokens,
        },
    ));
    
    Ok(())
}
```

## Key Design Points:

1. **Backward Compatibility**: If no budget is configured in the manifest, `budget_tracker` will be `None` and budget checking is skipped.

2. **Error Propagation**: Budget errors are converted to `CoreError::BudgetExceeded` which the executor already checks.

3. **Thread Safety**: `BudgetTracker` uses atomic operations, and `AgentProcess` is accessed through `DashMap` which provides concurrent access.

4. **Integration Order**: Budget checking happens BEFORE audit logging, ensuring we don't log usage that exceeds the budget.

5. **Minimal Changes**: The change is focused on the `report_usage` method with supporting changes in `AgentProcess` and `AgentRegistry`.

## Testing Considerations:

The existing tests in `services.rs` should be updated to test budget enforcement:

```rust
#[tokio::test]
async fn report_usage_with_budget_enforcement() {
    // Create agent with small budget
    let manifest = AgentManifest::from_yaml(r#"
name: budget-agent
model: test-model
system_prompt: "test"
budget_config:
  max_tokens: 100
  reset_period_seconds: 3600
capabilities:
  - "tool: echo"
"#).unwrap();
    
    // Setup services with this agent
    // ... test setup code ...
    
    // First usage should succeed
    services.report_usage(agent_id, TokenUsage { input_tokens: 50, output_tokens: 25 }).await.unwrap();
    
    // Second usage that would exceed budget should fail
    let result = services.report_usage(agent_id, TokenUsage { input_tokens: 60, output_tokens: 30 }).await;
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("Budget exceeded"));
}
```