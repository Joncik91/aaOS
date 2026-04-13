# AAOS Security Vulnerabilities - Prioritized List

## Critical Vulnerabilities (CVSS 8.6-9.1)

### V1.1: Path Traversal via Glob Matching
- **Location**: `/src/crates/aaos-core/src/capability.rs:146`
- **CVSS Score**: 9.1 (CVSS:3.1/AV:N/AC:L/PR:N/UI:N/S:U/C:H/I:H/A:N)
- **Description**: `glob_matches` function doesn't handle path normalization or directory traversal sequences
- **Impact**: Agents can bypass directory restrictions using `..` sequences
- **Example**: Permission for `/data/*` allows access to `/data/../etc/passwd`
- **Proof of Concept**:
  ```rust
  // Current vulnerable implementation
  fn glob_matches(pattern: &str, path: &str) -> bool {
      if pattern == "*" { return true; }
      if let Some(prefix) = pattern.strip_suffix('*') {
          path.starts_with(prefix)  // Vulnerable: "/data/../etc/passwd" starts with "/data/"
      } else {
          pattern == path
      }
  }
  ```

### V2.1: Missing Parent Capability Validation
- **Location**: `/src/crates/agentd/src/spawn_tool.rs:80-120`
- **CVSS Score**: 8.8 (CVSS:3.1/AV:N/AC:L/PR:L/UI:N/S:U/C:H/I:H/A:H)
- **Description**: Parent validation only checks if parent permits child capability, not if parent actually possesses it
- **Impact**: Child agents could receive capabilities parent doesn't have via wildcard permissions
- **Example**: Parent with `"tool:*"` could spawn child requesting `"file_write:/etc/*"`

### V3.1: Missing Path Canonicalization
- **Location**: `/src/crates/aaos-tools/src/file_read.rs:38-60` and `file_write.rs:58-80`
- **CVSS Score**: 8.6 (CVSS:3.1/AV:N/AC:L/PR:L/UI:N/S:U/C:H/I:H/A:N)
- **Description**: File operations use `Path::new()` without canonicalization
- **Impact**: Path traversal attacks bypass capability checks
- **Example**: `"/allowed/../secret"` matches `"/allowed/*"` permission

## High Severity Vulnerabilities (CVSS 7.0-7.5)

### V1.2: Missing Path Normalization
- **Location**: `/src/crates/aaos-core/src/capability.rs:146`
- **CVSS Score**: 7.5 (CVSS:3.1/AV:N/AC:L/PR:L/UI:N/S:U/C:H/I:N/A:N)
- **Description**: No canonicalization before glob matching, symlinks not resolved
- **Impact**: Bypass directory restrictions via symlinks

### V2.2: Child Token Ignores Parent Constraints
- **Location**: `/src/crates/agentd/src/spawn_tool.rs:120-130`
- **CVSS Score**: 7.3 (CVSS:3.1/AV:N/AC:L/PR:L/UI:N/S:U/C:L/I:L/A:L)
- **Description**: Child tokens issued with `Constraints::default()` ignoring parent constraints
- **Impact**: Child could have fewer constraints than parent (higher rate limits)

### V3.2: Symlink Attacks
- **Location**: `/src/crates/aaos-tools/src/file_read.rs` and `file_write.rs`
- **CVSS Score**: 7.1 (CVSS:3.1/AV:N/AC:L/PR:L/UI:N/S:U/C:L/I:H/A:N)
- **Description**: No check for symlinks before file operations
- **Impact**: Write to symlink pointing outside allowed area

### V6.1: Capability Checker Injection
- **Location**: `/src/crates/aaos-ipc/src/router.rs:25-35`
- **CVSS Score**: 7.0 (CVSS:3.1/AV:N/AC:L/PR:L/UI:N/S:C/C:L/I:L/A:N)
- **Description**: Router accepts arbitrary `capability_checker` function
- **Impact**: Single point of failure for all inter-agent communication

## Medium Severity Vulnerabilities (CVSS 5.4-6.5)

### V3.3: Directory Creation Without Validation
- **Location**: `/src/crates/aaos-tools/src/file_write.rs:80-85`
- **CVSS Score**: 6.5 (CVSS:3.1/AV:N/AC:L/PR:L/UI:N/S:U/C:N/I:L/A:L)
- **Description**: `create_dir_all()` creates parent dirs without checking glob boundaries
- **Impact**: Could create directory structure outside allowed area

### V4.2: Approval Service Bypass
- **Location**: `/src/crates/aaos-runtime/src/services.rs:38-70`
- **CVSS Score**: 6.3 (CVSS:3.1/AV:N/AC:L/PR:H/UI:N/S:U/C:H/I:N/A:N)
- **Description**: Human approval could bypass capability checks
- **Impact**: Approved actions might not respect capability boundaries

### V6.2: No Message Content Validation
- **Location**: `/src/crates/aaos-ipc/src/router.rs`
- **CVSS Score**: 5.9 (CVSS:3.1/AV:N/AC:L/PR:L/UI:N/S:U/C:N/I:N/A:L)
- **Description**: Routes any JSON content without validation
- **Impact**: Malformed messages could cause crashes

### V7.1: Token Filtering Flaw
- **Location**: `/src/crates/aaos-tools/src/invocation.rs:60-80`
- **CVSS Score**: 5.4 (CVSS:3.1/AV:N/AC:L/PR:L/UI:N/S:U/C:L/I:L/A:N)
- **Description**: `matches_tool_capability` returns `true` for unknown tools
- **Impact**: Unknown tools receive ALL tokens, potentially including sensitive file access
- **Vulnerable Code**:
  ```rust
  fn matches_tool_capability(capability: &Capability, tool_name: &str) -> bool {
      match tool_name {
          "file_read" => matches!(capability, Capability::FileRead { .. }),
          // ... other known tools ...
          _ => true, // unknown tools get all tokens! ← VULNERABILITY
      }
  }
  ```

## Low Severity Issues

### V5.1: No Direct Capability Validation in Executor
- **Location**: `/src/crates/aaos-llm/src/executor.rs:180-250`
- **CVSS Score**: 2.7 (CVSS:3.1/AV:N/AC:L/PR:L/UI:N/S:U/C:N/I:N/A:L)
- **Note**: This is actually correct design - executor properly delegates to services layer

### V8.2: Persistent Agent Self-Send Capability
- **Location**: `/src/crates/aaos-runtime/src/registry.rs:280-290`
- **CVSS Score**: 2.1 (CVSS:3.1/AV:N/AC:L/PR:L/UI:N/S:U/C:N/I:L/A:N)
- **Description**: Automatic grant of self-messaging capability to persistent agents
- **Impact**: Minor - necessary for API functionality

## Vulnerability Matrix

| ID | Severity | CVSS | Component | Status | Priority |
|----|----------|------|-----------|--------|----------|
| V1.1 | CRITICAL | 9.1 | Capability System | Confirmed | P0 |
| V2.1 | CRITICAL | 8.8 | Spawn Tool | Confirmed | P0 |
| V3.1 | CRITICAL | 8.6 | File Operations | Confirmed | P0 |
| V1.2 | HIGH | 7.5 | Capability System | Confirmed | P1 |
| V2.2 | HIGH | 7.3 | Spawn Tool | Confirmed | P1 |
| V3.2 | HIGH | 7.1 | File Operations | Confirmed | P1 |
| V6.1 | HIGH | 7.0 | Message Router | Confirmed | P1 |
| V3.3 | MEDIUM | 6.5 | File Operations | Confirmed | P2 |
| V4.2 | MEDIUM | 6.3 | Services Layer | Confirmed | P2 |
| V6.2 | MEDIUM | 5.9 | Message Router | Confirmed | P2 |
| V7.1 | MEDIUM | 5.4 | Tool Invocation | Confirmed | P2 |
| V5.1 | LOW | 2.7 | LLM Executor | Design Issue | P3 |
| V8.2 | LOW | 2.1 | Agent Registry | By Design | P3 |

## Remediation Timeline

### P0 (Critical) - Immediate Fix (Week 1)
- V1.1: Fix glob matching with path normalization
- V2.1: Complete parent capability validation
- V3.1: Add path canonicalization to file operations

### P1 (High) - Fix within 1 week
- V1.2: Add path normalization to capability system
- V2.2: Inherit parent constraints in child tokens
- V3.2: Add symlink protection
- V6.1: Secure capability checker injection

### P2 (Medium) - Fix within 2 weeks
- V3.3: Validate directory creation paths
- V4.2: Integrate approval service with capability checks
- V6.2: Add message content validation
- V7.1: Fix token filtering for unknown tools

### P3 (Low) - Address in next release
- V5.1: Review design (actually correct)
- V8.2: Document rationale for self-send capability

## Testing Requirements

For each vulnerability fix, implement:
1. Unit tests demonstrating the vulnerability
2. Unit tests verifying the fix
3. Integration tests for cross-component scenarios
4. Fuzz tests for path validation and capability parsing

## Verification Checklist

- [ ] Path traversal attacks no longer possible
- [ ] Parent cannot delegate capabilities it doesn't possess
- [ ] Child tokens inherit parent constraints
- [ ] Symlinks cannot bypass directory restrictions
- [ ] Unknown tools don't receive sensitive tokens
- [ ] Message router uses secure capability checker
- [ ] All fixes have comprehensive test coverage