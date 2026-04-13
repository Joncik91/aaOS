# Phase E3: Per-Agent Token Budget Enforcement - Design Document

## Overview

This document describes the architecture and implementation of per-agent token budget enforcement for aaOS (agent-first operating system). The system tracks cumulative token usage per agent and prevents agents from exceeding their allocated budget, providing cost control and resource management.

## Architecture

### Core Components

1. **BudgetConfig** - Configuration structure stored in AgentManifest
2. **BudgetTracker** - Thread-safe tracker with atomic operations
3. **AgentProcess** - Extended to include budget tracking
4. **AgentServices** - Updated report_usage() method with budget enforcement
5. **CoreError** - New BudgetExceeded error variant

### Data Flow

```
Agent Execution
     │
     ▼
Token Usage Generated
     │
     ▼
report_usage(agent_id, TokenUsage)
     │
     ▼
BudgetTracker.track(total_tokens) ──┐
     │                               │
     ▼                               │
Check budget                         │
     │                               │
     ▼                               │
If exceeded → BudgetError            │
     │                               │
     ▼                               │
If within budget → Continue          │
     │                               │
     ▼                               │
Update atomic counters ◄─────────────┘
     │
     ▼
Audit logging (existing Phase E2)
     │
     ▼
Return success/error
```

## Implementation Details

### 1. BudgetTracker (`budget.rs`)

The core budget tracking component with the following features:

#### Thread Safety
- Uses `AtomicU64` for all counters
- Compare-and-swap loops for atomic updates
- No locks for common operations
- Suitable for high-concurrency environments

#### Reset Periods
- Configurable reset periods (0 = no reset, one-time budget)
- Automatic reset checking throttled to once per second
- System time based on UNIX epoch seconds

#### Error Handling
- Detailed `BudgetError` with usage statistics
- Clear error messages showing used/limit/requested tokens

#### Performance Optimizations
- Reset checking limited to once per second
- Atomic operations minimize contention
- Minimal memory footprint (~32 bytes plus atomics)

### 2. AgentManifest Integration

#### Configuration Structure
```rust
pub struct BudgetConfig {
    pub max_tokens: u64,
    #[serde(default = "default_reset_period")]
    pub reset_period_seconds: u64,
}
```

#### Backward Compatibility
- Optional field with `#[serde(default)]`
- No budget enforcement if not specified
- Existing manifests continue to work unchanged

### 3. AgentProcess Extension

#### Storage
```rust
pub struct AgentProcess {
    // ... existing fields
    pub budget_tracker: Option<Arc<BudgetTracker>>,
}
```

#### Initialization
- Created in `AgentProcess::new()` from manifest configuration
- `Arc` wrapped for shared ownership
- `None` if no budget configuration specified

### 4. Services Layer Integration

#### report_usage() Enhancement
```rust
async fn report_usage(&self, agent_id: AgentId, usage: TokenUsage) -> Result<()> {
    // Phase E3: Budget enforcement
    self.registry.track_token_usage(agent_id, usage.clone())?;
    
    // Phase E2: Existing audit logging
    self.audit_log.record(AuditEvent::new(...));
    
    Ok(())
}
```

#### Error Propagation
- Budget errors converted to `CoreError::BudgetExceeded`
- Executor already checks these errors
- Natural integration with existing error handling

### 5. Error System Extension

#### New Error Variant
```rust
#[derive(Debug, Error)]
pub enum CoreError {
    // ... existing variants
    #[error("Token budget exceeded: {0}")]
    BudgetExceeded(String),
}
```

## Configuration Examples

### YAML Manifest with Budget
```yaml
name: research-agent
model: claude-3-5-sonnet-20241022
system_prompt: "You are a research assistant."
budget_config:
  max_tokens: 1000000      # 1 million tokens
  reset_period_seconds: 86400  # 24 hours (daily reset)
capabilities:
  - web_search
  - "file_read: /data/research/*"
lifecycle: on-demand
```

### Different Reset Periods
```yaml
# Hourly reset
budget_config:
  max_tokens: 50000
  reset_period_seconds: 3600

# Weekly reset
budget_config:
  max_tokens: 1000000
  reset_period_seconds: 604800

# One-time budget (no reset)
budget_config:
  max_tokens: 10000
  reset_period_seconds: 0
```

## Performance Considerations

### Atomic Operations
- `AtomicU64` operations are lock-free
- Compare-and-swap for updates ensures consistency
- Suitable for high-frequency usage reporting

### Reset Checking
- Throttled to once per second
- Avoids excessive system calls
- Trade-off: up to 1-second delay in reset detection

### Memory Usage
- BudgetTracker: ~32 bytes + atomics
- AgentProcess: Additional Arc pointer
- Minimal impact on overall system

## Thread Safety

### Concurrent Access Patterns
1. **Multiple agents reporting usage simultaneously** - Atomic operations handle this
2. **Budget checking while tracking** - Atomic operations are thread-safe
3. **Reset period checking** - Atomic timestamps ensure consistency

### No Deadlocks
- No mutexes or locks in hot path
- Atomic operations cannot deadlock
- Reset checking uses atomic timestamps

## Testing Strategy

### Unit Tests
1. **BudgetTracker basics** - Within limit, exceeds limit
2. **Reset functionality** - Automatic and manual reset
3. **Thread safety** - Concurrent access from multiple threads
4. **Error messages** - Clear and informative

### Integration Tests
1. **Manifest parsing** - With and without budget config
2. **Services integration** - report_usage with budget enforcement
3. **Error propagation** - Budget errors through call chain

### System Tests
1. **Multiple agents** - Concurrent budget tracking
2. **Long running** - Reset period behavior over time
3. **Edge cases** - Zero tokens, very large budgets

## Deployment Plan

### Phase 1: Core Implementation
1. Add BudgetConfig and BudgetTracker to aaos-core
2. Update AgentManifest with optional budget_config field
3. Add BudgetExceeded error variant

### Phase 2: Runtime Integration
1. Update AgentProcess to include BudgetTracker
2. Add track_token_usage method to AgentRegistry
3. Update InProcessAgentServices::report_usage()

### Phase 3: Testing
1. Unit tests for all new components
2. Integration tests with existing system
3. Performance testing with concurrent agents

### Phase 4: Rollout
1. Optional feature - no breaking changes
2. Gradual adoption by teams
3. Monitoring and metrics collection

## Monitoring and Metrics

### Key Metrics to Track
1. **Budget usage percentage** - Per agent
2. **Budget violations** - Count and rate
3. **Reset events** - Frequency and timing
4. **Error rates** - Budget vs other errors

### Alerting
1. **Warning threshold** - e.g., 80% of budget used
2. **Critical alerts** - Budget exceeded
3. **Reset failures** - If automatic reset doesn't work

## Future Enhancements

### 1. Hierarchical Budgets
- Parent-child budget relationships
- Shared budgets across agent teams
- Budget delegation and redistribution

### 2. Dynamic Budget Adjustment
- Adjust budgets based on agent performance
- Learning from usage patterns
- Predictive budget allocation

### 3. Advanced Reset Strategies
- Rolling windows (last N hours)
- Business hours only
- Calendar-based (weekdays vs weekends)

### 4. Cost Attribution
- Per-capability token tracking
- Cost breakdown by operation type
- Budget forecasting and planning

### 5. Integration with Billing
- Real-time cost tracking
- Budget alerts to financial systems
- Chargeback and showback reporting

## Conclusion

The per-agent token budget enforcement system provides a robust, thread-safe mechanism for controlling agent resource usage. With its optional configuration, atomic operations, and clean integration with existing systems, it offers:

1. **Cost Control** - Prevent runaway token usage
2. **Resource Management** - Fair allocation across agents
3. **Observability** - Clear tracking and reporting
4. **Flexibility** - Configurable per agent needs
5. **Performance** - Minimal overhead for common operations

The design follows aaOS patterns and integrates seamlessly with the existing Phase E2 usage tracking system, providing a complete solution for agent resource management.