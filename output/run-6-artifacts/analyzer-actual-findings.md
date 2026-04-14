Now let me provide a comprehensive analysis of improvements for the aaOS codebase:
# aaOS Source Code Analysis and Improvement Recommendations
## 1. Core Capability System (`/src/crates/aaos-core/src/capability.rs`)
### **Code Quality Issues:**
**Problem 1: Incomplete glob pattern matching**
```rust
fn glob_matches(pattern: &str, path: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    let normalized = normalize_path(path);
    if let Some(prefix) = pattern.strip_suffix('*') {
        let norm_prefix = normalize_path(prefix);
        normalized.starts_with(&norm_prefix)
    } else {
        let norm_pattern = normalize_path(pattern);
        norm_pattern == normalized
    }
}
```
**Issue:** Only supports exact matches and prefix wildcards (`/data/*`), but not:
- Suffix wildcards (`*.txt`)
- Middle wildcards (`/data/*/file.txt`)
- Multiple wildcards (`/data/*/*.txt`)
- Character classes (`[a-z]`)
- Ranges (`{a,b,c}`)
**Fix:** Use a proper glob library like `globset` or implement full glob matching.
**Problem 2: Missing capability validation**
```rust
pub fn issue(agent_id: AgentId, capability: Capability, constraints: Constraints) -> Self {
    // No validation of capability parameters
}
```
**Issue:** No validation of:
- Path globs for validity
- Host patterns for network access
- Agent names for spawn/message capabilities
**Fix:** Add validation methods for each capability type.
### **Error Handling Gaps:**
**Problem 3: Silent failures in `record_use()`**
```rust
pub fn record_use(&mut self) -> bool {
    self.invocation_count += 1;
    if let Some(max) = self.constraints.max_invocations {
        self.invocation_count <= max
    } else {
        true
    }
}
```
**Issue:** Returns `bool` but doesn't indicate why it failed. Callers might need to know if it's exhausted vs. other issues.
**Fix:** Return a `Result<(), CapabilityError>` with specific error variants.
### **Performance Concerns:**
**Problem 4: Repeated path normalization**
```rust
fn glob_matches(pattern: &str, path: &str) -> bool {
    let normalized = normalize_path(path);  // Called every time
    if let Some(prefix) = pattern.strip_suffix('*') {
        let norm_prefix = normalize_path(prefix);  // Called again
        normalized.starts_with(&norm_prefix)
    } else {
        let norm_pattern = normalize_path(pattern);  // Called again
        norm_pattern == normalized
    }
}
```
**Issue:** `normalize_path` is called multiple times per match operation.
**Fix:** Cache normalized paths or pre-normalize patterns.
### **Architectural Improvements:**
**Problem 5: Missing capability delegation**
**Issue:** No way for agents to delegate capabilities to child agents with reduced privileges.
**Fix:** Add `delegate()` method that creates new tokens with narrowed constraints for specific agents.
### **Security Considerations:**
**Problem 6: Path traversal incomplete protection**
```rust
fn normalize_path(path: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for component in path.split('/') {
        match component {
            "" | "." => {}
            ".." => { parts.pop(); }
            other => parts.push(other),
        }
    }
    // ...
}
```
**Issue:** Doesn't handle symlinks or absolute symlink targets. Also doesn't prevent `//etc/passwd` style attacks.
**Fix:** Use `std::path::Path` methods and consider filesystem checks for production.
---
## 2. Context Management (`/src/crates/aaos-runtime/src/context.rs`)
### **Code Quality Issues:**
**Problem 1: Complex summarization logic**
```rust
fn select_summarization_boundary(history: &[Message], target_tokens: u32) -> usize {
    // 40+ lines of complex logic
}
```
**Issue:** Function is too complex with multiple responsibilities:
- Token counting
- Tool pair detection
- Boundary adjustment
**Fix:** Break into smaller functions with single responsibilities.
**Problem 2: Hard-coded summarization prompt**
```rust
let request = CompletionRequest {
    system: "Compress this conversation into a dense factual summary...".to_string(),
    // ...
};
```
**Issue:** Prompt is hard-coded, not configurable per agent or use case.
**Fix:** Make summarization prompt configurable in `MemoryConfig`.
### **Error Handling Gaps:**
**Problem 3: Silent fallback on LLM failure**
```rust
Err(e) => {
    tracing::warn!("Summarization failed ({e}), falling back to no-op");
    Ok(Self::fold_summaries_into_prompt(history, system_prompt))
}
```
**Issue:** Silent fallback could lead to context window overflow if summarization keeps failing.
**Fix:** Implement progressive fallback strategies:
1. Try smaller segments
2. Use simpler summarization (first/last sentences)
3. Hard truncation as last resort
### **Performance Concerns:**
**Problem 4: Repeated token estimation**
```rust
pub fn estimate_tokens(messages: &[Message]) -> u32 {
    let total_chars: usize = messages.iter().map(|m| message_chars(m)).sum();
    (total_chars / 4) as u32
}
```
**Issue:** Char/4 heuristic is crude. Different models have different tokenization.
**Fix:** Use proper tokenizer or cache token counts per message.
**Problem 5: No caching of formatted summaries**
```rust
fn format_messages_for_summary(messages: &[Message]) -> String {
    // Formatting happens every time
}
```
**Issue:** Formatting the same messages repeatedly for retries or different strategies.
**Fix:** Cache formatted text or compute incrementally.
### **Architectural Improvements:**
**Problem 6: Tight coupling with LLM client**
```rust
pub struct ContextManager {
    llm_client: Arc<dyn LlmClient>,
    // ...
}
```
**Issue:** Hard dependency on LLM for summarization prevents alternative strategies.
**Fix:** Use strategy pattern with `SummarizationStrategy` trait.
### **Missing Tests:**
**Problem 7: No tests for edge cases**
**Missing tests for:**
- Empty history
- Single huge message exceeding context
- Mixed content types (text + tool use + images)
- Nested tool call chains
- Concurrent summarization requests
---
## 3. Persistent Agent Loop (`/src/crates/aaos-runtime/src/persistent.rs`)
### **Code Quality Issues:**
**Problem 1: Long function with multiple responsibilities**
```rust
pub async fn persistent_agent_loop(...) {
    // 200+ lines handling:
    // - Message reception
    // - Context preparation
    // - Execution
    // - Session storage
    // - Audit logging
    // - Command handling
}
```
**Issue:** Violates Single Responsibility Principle.
**Fix:** Extract into smaller components:
- `MessageProcessor`
- `ContextPreparer`
- `ExecutionOrchestrator`
- `SessionManager`
**Problem 2: Complex error recovery**
```rust
match result.stop_reason {
    ExecutionStopReason::Error(ref err_msg) => {
        // Error handling
    }
    _ => {
        // Success handling (50+ lines)
    }
}
```
**Issue:** Success path is too long and complex.
**Fix:** Extract success handling to separate function.
### **Error Handling Gaps:**
**Problem 3: Ignored session store errors**
```rust
let _ = session_store.archive_segment(&agent_id, &segment);
let _ = session_store.clear(&agent_id);
let _ = session_store.append(&agent_id, &history);
```
**Issue:** Silent discarding of storage errors could lead to data loss.
**Fix:** Proper error handling with retries or at least logging.
**Problem 4: No backpressure for message processing**
```rust
let Some(msg) = msg else { break; };
// Immediately process without checking load
```
**Issue:** Could overwhelm system under high load.
**Fix:** Implement backpressure with circuit breaker pattern.
### **Performance Concerns:**
**Problem 5: Blocking file I/O in async context**
```rust
let system_prompt_str = match &manifest.system_prompt {
    PromptSource::Inline(s) => s.clone(),
    PromptSource::File(path) => {
        std::fs::read_to_string(path)  // BLOCKING
            .unwrap_or_else(|_| format!("Failed to read prompt from {}", path.display()))
    }
};
```
**Issue:** Blocking I/O in async context can stall the event loop.
**Fix:** Use `tokio::fs` for async file operations.
**Problem 6: No batching of session writes**
```rust
let _ = session_store.append(&agent_id, &result.transcript_delta);
```
**Issue:** Writing after every message creates I/O overhead.
**Fix:** Batch writes with periodic flushing.
### **Architectural Improvements:**
**Problem 7: Missing health checks**
**Issue:** No way to monitor agent health or detect stuck loops.
**Fix:** Add health check endpoint and watchdog timer.
**Problem 8: No graceful shutdown**
```rust
Some(AgentCommand::Stop) => break,
```
**Issue:** Immediate break doesn't allow finishing current operation.
**Fix:** Implement graceful shutdown with drain mode.
### **Security Considerations:**
**Problem 9: No input validation**
```rust
fn extract_user_message(msg: &McpMessage) -> String {
    msg.params
        .get("message")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| msg.method.clone())
}
```
**Issue:** No validation or sanitization of user input.
**Fix:** Add input validation and size limits.
---
## 4. Session Storage (`/src/crates/aaos-runtime/src/session.rs`)
### **Code Quality Issues:**
**Problem 1: File-based locking issues**
```rust
fn append(&self, agent_id: &AgentId, messages: &[Message]) -> Result<()> {
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)  // No file locking
        .map_err(|e| CoreError::Ipc(...))?;
}
```
**Issue:** Concurrent appends from multiple threads/processes could corrupt file.
**Fix:** Implement file locking or use database.
**Problem 2: Inefficient archive loading**
```rust
fn load_archives(&self, agent_id: &AgentId) -> Result<Vec<ArchiveSegment>> {
    let prefix = format!("{}.archive.", agent_id.as_uuid());
    let mut archives = Vec::new();
    let entries = std::fs::read_dir(&self.data_dir).map_err(...)?;
    for entry in entries {
        // Read and parse each file individually
    }
}
```
**Issue:** Reads all archive files every time, even if only need recent ones.
**Fix:** Add metadata index file or database.
### **Error Handling Gaps:**
**Problem 3: Silent file corruption**
```rust
let msg: Message = serde_json::from_str(line)?;
```
**Issue:** If one line is corrupted, entire load fails.
**Fix:** Implement line-by-line error recovery with logging.
### **Performance Concerns:**
**Problem 4: No compression for archives**
```rust
let json = serde_json::to_string(segment)?;
std::fs::write(&path, json).map_err(...)?;
```
**Issue:** Archives stored as plain JSON, no compression.
**Fix:** Add gzip compression for archives.
**Problem 5: Linear search for archives**
```rust
if name.starts_with(&prefix) && name.ends_with(".json") {
```
**Issue:** O(n) search through directory entries.
**Fix:** Maintain index or use subdirectories by date.
### **Architectural Improvements:**
**Problem 6: Missing migration support**
**Issue:** No versioning or migration for session format changes.
**Fix:** Add version header to files and migration utilities.
**Problem 7: No encryption at rest**
**Issue:** Sensitive conversation data stored in plain text.
**Fix:** Add optional encryption for session files.
### **Missing Tests:**
**Problem 8: No concurrency tests**
**Missing tests for:**
- Concurrent reads/writes
- File locking behavior
- Recovery from partial writes
- Disk full scenarios
---
## 5. Web Fetch Tool (`/src/crates/aaos-tools/src/web_fetch.rs`)
### **Code Quality Issues:**
**Problem 1: Panic in constructor**
```rust
impl WebFetchTool {
    pub fn new() -> Self {
        let http = Client::builder()
            .timeout(Duration::from_secs(TIMEOUT_SECS))
            .redirect(reqwest::redirect::Policy::limited(MAX_REDIRECTS))
            .build()
            .expect("failed to build HTTP client");  // PANIC
        Self { http }
    }
}
```
**Issue:** Constructor can panic if HTTP client fails to build.
**Fix:** Return `Result<Self, CoreError>` instead.
**Problem 2: Hard-coded limits**
```rust
const DEFAULT_MAX_BYTES: usize = 50_000;
const TIMEOUT_SECS: u64 = 30;
const MAX_REDIRECTS: usize = 5;
```
**Issue:** Not configurable per-agent or per-invocation.
**Fix:** Make configurable via manifest or invocation context.
### **Error Handling Gaps:**
**Problem 3: Missing URL validation**
```rust
let url = input
    .get("url")
    .and_then(|v| v.as_str())
    .ok_or_else(|| CoreError::InvalidManifest("missing 'url' parameter".into()))?;
// No validation of URL format or allowed domains
```
**Issue:** Could fetch malicious or internal URLs.
**Fix:** Add URL validation against allowed domains/patterns.
**Problem 4: No retry logic**
```rust
let response = self
    .http
    .get(url)
    .send()
    .await
    .map_err(|e| CoreError::Ipc(format!("fetch failed: {e}")))?;
```
**Issue:** No retry for transient failures.
**Fix:** Add configurable retry with exponential backoff.
### **Security Considerations:**
**Problem 5: No request size limits**
```rust
let bytes = response
    .bytes()
    .await
    .map_err(|e| CoreError::Ipc(format!("failed to read body: {e}")))?;
```
**Issue:** Reads entire response into memory before checking size.
**Fix:** Stream response with incremental size checking.
**Problem 6: No content-type validation**
```rust
let content_type = response
    .headers()
    .get("content-type")
    .and_then(|v| v.to_str().ok())
    .unwrap_or("unknown")
    .to_string();
```
**Issue:** Could fetch binary/executable content.
**Fix:** Validate content-type against allowed types.
**Problem 7: No request headers control**
**Issue:** Can't set User-Agent, Accept headers, or authentication.
**Fix:** Add configurable headers capability.
### **Performance Concerns:**
**Problem 8: No connection pooling reuse**
```rust
impl Default for WebFetchTool {
    fn default() -> Self {
        Self::new()  // Creates new client each time
    }
}
```
**Issue:** If tool is recreated frequently, loses connection pooling benefits.
**Fix:** Use shared HTTP client singleton.
### **Missing Tests:**
