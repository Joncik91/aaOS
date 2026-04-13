# AAOS Security Hardening Plan

## Prioritized Fixes with Code Suggestions

### Phase 1: Critical Fixes (Week 1)

#### 1.1 Fix Path Traversal in Glob Matching (V1.1)
**File**: `/src/crates/aaos-core/src/capability.rs`

```rust
/// Secure glob matching with path normalization
fn glob_matches(pattern: &str, path: &str) -> bool {
    use std::path::Path;
    
    if pattern == "*" {
        return true;
    }
    
    // Normalize both pattern and path
    let normalized_path = Path::new(path)
        .canonicalize()
        .ok()
        .and_then(|p| p.to_str().map(|s| s.to_string()))
        .unwrap_or_else(|| path.to_string());
    
    let normalized_pattern = if let Some(prefix) = pattern.strip_suffix('*') {
        // For glob patterns, normalize the prefix
        Path::new(prefix)
            .canonicalize()
            .ok()
            .and_then(|p| p.to_str().map(|s| format!("{}*", s)))
            .unwrap_or_else(|| pattern.to_string())
    } else {
        pattern.to_string()
    };
    
    if let Some(prefix) = normalized_pattern.strip_suffix('*') {
        normalized_path.starts_with(prefix)
    } else {
        normalized_pattern == normalized_path
    }
}
```

#### 1.2 Fix Parent Capability Validation (V2.1)
**File**: `/src/crates/agentd/src/spawn_tool.rs`

```rust
// Replace lines 100-120 with:
// Find a parent token that permits this child capability AND parent actually has it
let parent_has_and_permits = parent_tokens.iter().any(|t| {
    // Parent must have the exact capability or broader
    match (&t.capability, &child_cap) {
        // For file operations, parent's glob must be superset of child's
        (Capability::FileRead { path_glob: parent_glob }, 
         Capability::FileRead { path_glob: child_glob }) => {
            glob_matches(parent_glob, child_glob)
        }
        (Capability::FileWrite { path_glob: parent_glob }, 
         Capability::FileWrite { path_glob: child_glob }) => {
            glob_matches(parent_glob, child_glob)
        }
        // For spawn child, parent's allowed agents must include child's
        (Capability::SpawnChild { allowed_agents: parent_agents }, 
         Capability::SpawnChild { allowed_agents: child_agents }) => {
            child_agents.iter().all(|c| 
                parent_agents.contains(c) || parent_agents.contains(&"*".to_string())
            )
        }
        // For exact matches
        _ => t.capability == child_cap || 
             (matches!(t.capability, Capability::ToolInvoke { tool_name: ref tn } 
                      if tn == "*") && 
              matches!(child_cap, Capability::ToolInvoke { .. }))
    }
});

if !parent_has_and_permits {
    return Err(CoreError::CapabilityDenied {
        agent_id: ctx.agent_id,
        capability: child_cap.clone(),
        reason: format!(
            "parent lacks {:?} or cannot delegate to child '{}'",
            child_cap, child_manifest.name
        ),
    });
}
```

#### 1.3 Fix Path Canonicalization in File Tools (V3.1)
**File**: `/src/crates/aaos-tools/src/file_read.rs` and `file_write.rs`

```rust
// Add helper function
fn normalize_and_validate_path(path_str: &str, capability_glob: &str) -> Result<std::path::PathBuf, CoreError> {
    use std::path::Path;
    
    let path = Path::new(path_str);
    
    // Canonicalize to resolve symlinks and normalize
    let canonical = path.canonicalize()
        .map_err(|e| CoreError::Ipc(format!("failed to canonicalize path: {e}")))?;
    
    // Convert to string for glob matching
    let canonical_str = canonical.to_str()
        .ok_or_else(|| CoreError::Ipc("path contains invalid UTF-8".into()))?;
    
    // Check against capability glob
    if !glob_matches(capability_glob, canonical_str) {
        return Err(CoreError::CapabilityDenied {
            agent_id: ctx.agent_id,
            capability: Capability::FileRead {
                path_glob: capability_glob.to_string(),
            },
            reason: format!("path not permitted: {canonical_str}"),
        });
    }
    
    Ok(canonical)
}

// In file_read.rs invoke method:
let canonical_path = normalize_and_validate_path(path_str, &capability_glob)?;
let metadata = tokio::fs::metadata(&canonical_path).await?;
```

### Phase 2: High Priority Fixes (Week 2)

#### 2.1 Fix Child Token Constraints (V2.2)
**File**: `/src/crates/agentd/src/spawn_tool.rs`

```rust
// Replace token issuance (lines 120-130) with:
// Find parent token that grants this capability to inherit constraints
let parent_token = parent_tokens.iter()
    .find(|t| t.permits(&child_cap))
    .ok_or_else(|| CoreError::CapabilityDenied {
        agent_id: ctx.agent_id,
        capability: child_cap.clone(),
        reason: "no parent token found".into(),
    })?;

// Inherit parent constraints
let child_constraints = parent_token.constraints.clone();

child_tokens.push(CapabilityToken::issue(
    child_id,
    child_cap,
    child_constraints,
));
```

#### 2.2 Add Symlink Protection (V3.2)
**File**: `/src/crates/aaos-tools/src/file_read.rs` and `file_write.rs`

```rust
// Add to normalize_and_validate_path function:
fn check_symlink_safety(path: &std::path::Path) -> Result<(), CoreError> {
    use std::fs;
    
    // Check if any component in the path is a symlink
    let mut current = path;
    while let Some(parent) = current.parent() {
        if let Ok(metadata) = fs::symlink_metadata(current) {
            if metadata.file_type().is_symlink() {
                return Err(CoreError::Ipc(
                    format!("path contains symlink: {}", current.display())
                ));
            }
        }
        current = parent;
    }
    Ok(())
}

// Call in normalize_and_validate_path:
check_symlink_safety(&canonical)?;
```

#### 2.3 Secure Capability Checker in Router (V6.1)
**File**: `/src/crates/aaos-ipc/src/router.rs`

```rust
// Instead of accepting arbitrary function, use registry method
pub fn new_with_registry(
    audit_log: Arc<dyn AuditLog>,
    registry: Arc<AgentRegistry>,
) -> Self {
    let capability_checker = move |agent_id: AgentId, capability: &Capability| {
        registry.check_capability(agent_id, capability).unwrap_or(false)
    };
    
    Self::new(audit_log, capability_checker)
}

// Update MessageRouter::new to be crate-private or remove
// to force use of secure constructor
```

### Phase 3: Medium Priority Fixes (Week 3)

#### 3.1 Fix Directory Creation Validation (V3.3)
**File**: `/src/crates/aaos-tools/src/file_write.rs`

```rust
// Replace create_dir_all logic with:
if let Some(parent) = path.parent() {
    // Check each directory in the path is within allowed glob
    let mut current = Path::new("");
    for component in parent.components() {
        current = current.join(component);
        let current_str = current.to_str()
            .ok_or_else(|| CoreError::Ipc("invalid path component".into()))?;
        
        if !glob_matches(capability_glob, current_str) {
            return Err(CoreError::CapabilityDenied {
                agent_id: ctx.agent_id,
                capability: Capability::FileWrite {
                    path_glob: capability_glob.to_string(),
                },
                reason: format!("parent directory not permitted: {current_str}"),
            });
        }
    }
    
    tokio::fs::create_dir_all(parent)
        .await
        .map_err(|e| CoreError::Ipc(format!("failed to create directories: {e}")))?;
}
```

#### 3.2 Fix Approval Service Integration (V4.2)
**File**: `/src/crates/aaos-runtime/src/services.rs`

```rust
// Modify invoke_tool to check capabilities even after approval
async fn invoke_tool(&self, agent_id: AgentId, tool: &str, input: Value) -> Result<Value> {
    // First check capability
    let tokens = self.registry.get_tokens(agent_id)?;
    let required = Capability::ToolInvoke {
        tool_name: tool.to_string(),
    };
    let has_capability = tokens.iter().any(|t| t.permits(&required));
    
    if !has_capability {
        return Err(CoreError::CapabilityDenied {
            agent_id,
            capability: required,
            reason: "tool invocation not permitted".into(),
        });
    }
    
    // Then check approval if required
    if let Ok(manifest) = self.registry.get_manifest(agent_id) {
        if manifest.approval_required.contains(&tool.to_string()) {
            // ... approval logic ...
        }
    }
    
    // Invoke tool
    self.tool_invocation.invoke(agent_id, tool, input, &tokens).await
}
```

#### 3.3 Fix Token Filtering for Unknown Tools (V7.1)
**File**: `/src/crates/aaos-tools/src/invocation.rs`

```rust
fn matches_tool_capability(capability: &Capability, tool_name: &str) -> bool {
    match tool_name {
        "file_read" => matches!(capability, Capability::FileRead { .. }),
        "file_write" => matches!(capability, Capability::FileWrite { .. }),
        "web_fetch" => matches!(capability, Capability::WebSearch),
        "spawn_agent" => matches!(capability, Capability::SpawnChild { .. }),
        "memory_store" | "memory_query" | "memory_delete" => 
            matches!(capability, Capability::Custom { name, .. } if name == "memory_access"),
        _ => matches!(capability, Capability::ToolInvoke { tool_name: tn } 
                     if tn == "*" || tn == tool_name),
    }
}
```

#### 3.4 Add Message Validation (V6.2)
**File**: `/src/crates/aaos-ipc/src/router.rs`

```rust
const MAX_MESSAGE_SIZE: usize = 1_048_576; // 1MB

pub async fn route(&self, message: McpMessage) -> Result<()> {
    // Validate message size
    let msg_size = serde_json::to_vec(&message)?.len();
    if msg_size > MAX_MESSAGE_SIZE {
        return Err(CoreError::Ipc(format!(
            "message too large: {} bytes (max {})",
            msg_size, MAX_MESSAGE_SIZE
        )));
    }
    
    // Validate JSON-RPC structure
    if message.jsonrpc != "2.0" {
        return Err(CoreError::Ipc("invalid JSON-RPC version".into()));
    }
    
    // ... existing capability checking and routing ...
}
```

### Phase 4: Security Enhancements (Week 4)

#### 4.1 Add Comprehensive Security Tests

```rust
// tests/security.rs
#[cfg(test)]
mod security_tests {
    use super::*;
    
    #[test]
    fn path_traversal_prevention() {
        assert!(!glob_matches("/data/*", "/data/../etc/passwd"));
        assert!(!glob_matches("/tmp/*", "/tmp//../etc/shadow"));
        assert!(!glob_matches("/home/user/*", "/home/user/./../other"));
    }
    
    #[test]
    fn symlink_protection() {
        // Create symlink test
        let temp_dir = tempfile::tempdir().unwrap();
        let allowed = temp_dir.path().join("allowed");
        std::fs::create_dir(&allowed).unwrap();
        
        let symlink = temp_dir.path().join("link");
        std::os::unix::fs::symlink("/etc", &symlink).unwrap();
        
        let target = symlink.join("passwd");
        assert!(!glob_matches(
            allowed.to_str().unwrap(),
            target.to_str().unwrap()
        ));
    }
    
    #[test]
    fn capability_escalation_prevention() {
        // Test parent can't delegate capabilities it doesn't have
        // even with wildcard permissions
    }
}
```

#### 4.2 Add Security Headers and Configuration

```rust
// src/security/config.rs
pub struct SecurityConfig {
    pub max_path_depth: usize,
    pub allow_symlinks: bool,
    pub require_path_canonicalization: bool,
    pub max_spawn_depth: u32,
    pub enable_audit_logging: bool,
    pub strict_capability_delegation: bool,
}

impl Default for SecurityConfig {
    fn default() -> Self {
        Self {
            max_path_depth: 64,
            allow_symlinks: false,
            require_path_canonicalization: true,
            max_spawn_depth: 5,
            enable_audit_logging: true,
            strict_capability_delegation: true,
        }
    }
}
```

#### 4.3 Implement Rate Limiting Enforcement

```rust
// src/capability/enforcement.rs
pub struct RateLimiter {
    limits: HashMap<Uuid, TokenBucket>,
}

impl RateLimiter {
    pub fn check(&mut self, token: &CapabilityToken) -> bool {
        if let Some(rate_limit) = &token.constraints.rate_limit {
            let bucket = self.limits.entry(token.id)
                .or_insert_with(|| TokenBucket::new(rate_limit.max_per_minute));
            bucket.try_consume(1)
        } else {
            true
        }
    }
}

// Integrate into ToolInvocation::invoke
let mut rate_limiter = self.rate_limiter.lock().unwrap();
if !rate_limiter.check(token) {
    return Err(CoreError::RateLimitExceeded);
}
```

## Implementation Timeline

### Week 1: Critical Vulnerability Fixes
- Fix path traversal in glob matching
- Fix parent capability validation
- Fix path canonicalization

### Week 2: High Priority Fixes
- Fix child token constraints
- Add symlink protection
- Secure capability checker

### Week 3: Medium Priority Fixes
- Fix directory creation validation
- Fix approval service integration
- Fix token filtering
- Add message validation

### Week 4: Security Enhancements
- Add comprehensive security tests
- Implement security configuration
- Add rate limiting enforcement
- Security documentation

## Testing Strategy

1. **Unit Tests**: Each fix includes comprehensive unit tests
2. **Integration Tests**: Test cross-component security boundaries
3. **Fuzz Testing**: Path validation, capability parsing, message routing
4. **Penetration Testing**: Simulated attack scenarios
5. **Regression Testing**: Ensure fixes don't break existing functionality

## Monitoring and Validation

1. **Audit Log Analysis**: Regular review of security events
2. **Capability Usage Monitoring**: Track unusual patterns
3. **Path Validation Logging**: Log all path normalization attempts
4. **Security Metrics**: Track security incidents and false positives

## Rollout Plan

1. **Development**: Implement fixes in feature branches
2. **Testing**: Security team review and penetration testing
3. **Staging**: Deploy to staging environment for validation
4. **Production**: Gradual rollout with feature flags
5. **Monitoring**: Enhanced monitoring during rollout