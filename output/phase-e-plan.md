# Phase E: Inference Scheduling - Implementation Plan

## 1. Project Structure

### 1.1 New Crates
```
aaos-llm-backends/          # Ollama, vLLM client implementations
    src/
        lib.rs
        ollama.rs
        vllm.rs
        common.rs
        mod.rs
        
aaos-inference/            # Scheduler, cache manager, budget tracking
    src/
        lib.rs
        scheduler.rs
        cache.rs
        budget.rs
        types.rs
```

### 1.2 Modified Crates
```
aaos-llm/                  # Extended LlmClient trait, ModelRegistry
aaos-runtime/             # InferenceScheduler integration
aaos-tools/               # New inference monitoring tools
Cargo.toml                # Workspace dependencies
```

## 2. Core Types and Traits

### 2.1 Extended LlmClient Trait (aaos-llm)
```rust
// src/client.rs
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InferenceBudget {
    pub max_tokens: Option<u32>,
    pub max_cost_usd: Option<f64>,
    pub priority: Priority,
    pub deadline: Option<Instant>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Priority {
    Critical = 100,
    Normal = 50,
    Background = 10,
}

#[derive(Debug)]
pub struct ChatResponse {
    pub content: String,
    pub usage: UsageMetrics,
    pub cost_usd: f64,
    pub cache_hit: bool,
}

pub trait LlmClient: Send + Sync {
    // Existing
    async fn chat(&self, messages: Vec<Message>) -> Result<String, LlmError>;
    async fn max_context_tokens(&self) -> usize;
    
    // New Phase E
    async fn chat_with_budget(
        &self,
        messages: Vec<Message>,
        budget: InferenceBudget,
    ) -> Result<ChatResponse, LlmError>;
    
    fn supports_kv_cache(&self) -> bool;
    fn cache_identifier(&self) -> Option<CacheKey>;
    async fn estimate_cost(&self, messages: &[Message]) -> CostEstimate;
    fn backend_type(&self) -> BackendType;
}
```

### 2.2 Model Registry (aaos-llm)
```rust
// src/registry.rs
pub struct ModelRegistry {
    backends: HashMap<BackendType, Arc<dyn LlmClient>>,
    default_backend: BackendType,
}

impl ModelRegistry {
    pub fn new(config: &InferenceConfig) -> Result<Self, RegistryError>;
    pub fn get_client(&self, model_spec: &str) -> Result<Arc<dyn LlmClient>, RegistryError>;
    pub fn register_backend(&mut self, backend: BackendType, client: Arc<dyn LlmClient>);
    pub fn list_available_models(&self) -> Vec<ModelInfo>;
}

#[derive(Debug, Clone)]
pub struct ModelInfo {
    pub spec: String,           // "ollama:llama3.2:3b"
    pub backend: BackendType,
    pub context_size: usize,
    pub cost_per_token: f64,
    pub available: bool,
}
```

### 2.3 Inference Scheduler (aaos-inference)
```rust
// src/scheduler.rs
pub struct InferenceScheduler {
    queues: HashMap<Priority, VecDeque<InferenceRequest>>,
    active_slots: HashMap<AgentId, ActiveInference>,
    cache_manager: Option<Arc<dyn KvCacheManager>>,
    budget_tracker: BudgetTracker,
    config: SchedulerConfig,
}

#[derive(Debug)]
pub struct InferenceRequest {
    pub request_id: Uuid,
    pub agent_id: AgentId,
    pub messages: Vec<Message>,
    pub model_requirements: ModelRequirements,
    pub budget: InferenceBudget,
    pub cache_hint: Option<CacheHint>,
    pub created_at: Instant,
    pub callback: oneshot::Sender<InferenceResult>,
}

impl InferenceScheduler {
    pub fn new(config: SchedulerConfig) -> Self;
    
    pub async fn submit(
        &self,
        request: InferenceRequest,
    ) -> Result<InferenceTicket, SchedulerError>;
    
    pub async fn schedule_next(&mut self) -> Option<ScheduledInference>;
    
    pub fn check_agent_budget(&self, agent_id: &AgentId) -> BudgetStatus;
    
    pub fn cancel_request(&mut self, request_id: &Uuid) -> bool;
    
    pub fn get_queue_stats(&self) -> QueueStats;
}

// src/types.rs
#[derive(Debug, Clone)]
pub struct SchedulerConfig {
    pub max_concurrent_slots: usize,
    pub max_queue_depth: usize,
    pub enable_preemption: bool,
    pub starvation_timeout_ms: u64,
    pub default_priority: Priority,
}
```

### 2.4 KV Cache Manager (aaos-inference)
```rust
// src/cache.rs
pub trait KvCacheManager: Send + Sync {
    async fn allocate(
        &self,
        agent_id: &AgentId,
        model: &str,
        context: &[Message],
    ) -> Result<CacheHandle, CacheError>;
    
    async fn evict(
        &self,
        policy: EvictionPolicy,
    ) -> EvictionResult;
    
    async fn share_context(
        &self,
        source_agent: &AgentId,
        target_agent: &AgentId,
        similarity_threshold: f32,
    ) -> Result<SharingResult, CacheError>;
    
    fn stats(&self) -> CacheStats;
}

#[derive(Debug)]
pub struct CacheStats {
    pub total_size_bytes: u64,
    pub used_bytes: u64,
    pub hit_count: u64,
    pub miss_count: u64,
    pub eviction_count: u64,
    pub sharing_hits: u64,
}

#[derive(Debug, Clone)]
pub enum EvictionPolicy {
    Lru,
    Lfu,
    SizeBased,
    BudgetAware,
    Hybrid(Lru, SizeBased),
}
```

### 2.5 Budget Tracker (aaos-inference)
```rust
// src/budget.rs
pub struct BudgetTracker {
    per_agent_budgets: DashMap<AgentId, AgentBudget>,
    global_budget: Option<GlobalBudget>,
    spending_history: Vec<SpendingRecord>,
}

#[derive(Debug, Clone)]
pub struct AgentBudget {
    pub tokens_per_minute: Option<u32>,
    pub cost_per_session_usd: Option<f64>,
    pub priority_boost: Option<Priority>,
    pub spent_this_minute: AtomicU32,
    pub spent_this_session: AtomicU64, // micro-USD
}

impl BudgetTracker {
    pub fn can_spend(&self, agent_id: &AgentId, cost: CostEstimate) -> BudgetCheck;
    
    pub fn record_spending(
        &self,
        agent_id: &AgentId,
        tokens: u32,
        cost_usd: f64,
    );
    
    pub fn reset_minute_counters(&self);
    
    pub fn get_agent_stats(&self, agent_id: &AgentId) -> Option<BudgetStats>;
}
```

## 3. Implementation Phases

### Phase E1: Local Backend Support (Weeks 1-3)
**Goal:** Basic Ollama integration
```rust
// 1. Create OllamaClient struct
// 2. Implement LlmClient trait for Ollama
// 3. Add model parsing: "ollama:llama3.2:3b"
// 4. Basic error handling and fallback
// 5. Unit tests with mock server
```

**Files:**
- `aaos-llm-backends/src/ollama.rs`
- `aaos-llm/src/registry.rs` (extended)
- `config/local-models.example.yaml`

### Phase E2: Basic Scheduler (Weeks 4-5)
**Goal:** FIFO scheduling without priority
```rust
// 1. InferenceScheduler with single queue
// 2. Basic request submission API
// 3. Concurrent slot management
// 4. Simple AgentBudget struct
// 5. Integration with AgentExecutor
```

**Files:**
- `aaos-inference/src/scheduler.rs` (basic)
- `aaos-inference/src/budget.rs` (basic)
- `aaos-runtime/src/inference.rs` (integration)

### Phase E3: Priority Scheduling (Weeks 6-7)
**Goal:** Priority queues and budget enforcement
```rust
// 1. Multiple priority queues (Critical/Normal/Background)
// 2. BudgetChecker for per-agent limits
// 3. Deadline-aware scheduling
// 4. Audit events for budget violations
// 5. Load testing with simulated agents
```

**Files:**
- `aaos-inference/src/scheduler.rs` (enhanced)
- `aaos-inference/src/budget.rs` (enhanced)
- `aaos-tools/src/inference_monitor.rs`

### Phase E4: KV Cache Management (Weeks 8-10)
**Goal:** Basic cache sharing and eviction
```rust
// 1. KvCacheManager trait and InMemoryCache
// 2. LRU eviction policy
// 3. Exact context matching for sharing
// 4. Cache statistics and monitoring
// 5. Integration with OllamaClient
```

**Files:**
- `aaos-inference/src/cache.rs`
- `aaos-llm-backends/src/cache_integration.rs`
- `examples/cache_demo.rs`

### Phase E5: Advanced Features (Weeks 11-12)
**Goal:** vLLM support and optimizations
```rust
// 1. VLLMClient implementation
// 2. Semantic cache sharing (embedding-based)
// 3. Predictive scheduling
// 4. Cost estimation improvements
// 5. Performance benchmarking
```

**Files:**
- `aaos-llm-backends/src/vllm.rs`
- `aaos-inference/src/optimization.rs`
- `benchmarks/inference_throughput.rs`

## 4. Integration Points

### 4.1 AgentExecutor Modification
```rust
// aaos-llm/src/executor.rs
impl AgentExecutor {
    pub async fn run_with_inference(
        &self,
        agent_id: &AgentId,
        messages: Vec<Message>,
        budget: Option<InferenceBudget>,
    ) -> Result<ExecutionResult, ExecutionError> {
        // 1. Get agent's inference capability
        // 2. Check budget via BudgetTracker
        // 3. Build InferenceRequest
        // 4. Submit to InferenceScheduler
        // 5. Await result with timeout
        // 6. Record cost and update budget
    }
}
```

### 4.2 Runtime Integration
```rust
// aaos-runtime/src/lib.rs
pub struct Runtime {
    // Existing fields...
    inference_scheduler: Arc<InferenceScheduler>,
    model_registry: Arc<ModelRegistry>,
}

impl Runtime {
    pub fn with_inference_config(config: InferenceConfig) -> Result<Self, RuntimeError>;
    
    pub fn inference_scheduler(&self) -> Arc<InferenceScheduler>;
    
    pub fn model_registry(&self) -> Arc<ModelRegistry>;
}
```

### 4.3 Capability System Extension
```rust
// aaos-runtime/src/capability.rs
#[derive(Debug, Clone)]
pub enum Capability {
    // Existing variants...
    Inference {
        backend: BackendType,
        budget: InferenceBudget,
    },
}

impl Capability {
    pub fn parse_inference(s: &str) -> Result<Self, ParseError> {
        // Parse "inference:ollama:1000-tokens-per-minute"
        // Parse "inference:anthropic:0.05-usd-per-session"
    }
}
```

## 5. Testing Strategy

### 5.1 Unit Tests
```
aaos-llm-backends/
    tests/
        ollama_client.rs      # Mock server tests
        vllm_client.rs       # Integration tests
        
aaos-inference/
    tests/
        scheduler_test.rs    # Priority queue tests
        budget_test.rs       # Budget math tests
        cache_test.rs        # Eviction policy tests
```

### 5.2 Integration Tests
```rust
// tests/inference_integration.rs
#[tokio::test]
async fn test_mixed_backend_fleet() {
    // 1. Start Ollama locally
    // 2. Create 5 agents with Ollama, 2 with Anthropic
    // 3. Submit concurrent requests
    // 4. Verify scheduling and budget enforcement
    // 5. Check cache behavior
}

#[tokio::test]
async fn test_budget_enforcement() {
    // 1. Agent with 100-token budget
    // 2. Submit requests exceeding budget
    // 3. Verify rejection/queuing
    // 4. Check audit events
}
```

### 5.3 Load Testing
```rust
// benches/concurrent_agents.rs
#[tokio::test]
async fn benchmark_100_concurrent_agents() {
    // Simulate 100 agents making requests
    // Measure: throughput, latency, cache hit rate
    // Resource usage: memory, CPU, network
}
```

## 6. Configuration

### 6.1 Runtime Config Additions
```toml
# aaos.toml
[inference]
enabled = true
default_backend = "ollama"
concurrent_slots = 4

[inference.ollama]
url = "http://localhost:11434"
default_model = "llama3.2:3b"
timeout_seconds = 30

[inference.scheduler]
max_queue_depth = 1000
enable_preemption = true
starvation_timeout_ms = 5000

[inference.cache]
enabled = true
max_size_mb = 1024
eviction_policy = "lru"
```

### 6.2 Environment Variables
```bash
AAOS_INFERENCE_ENABLED=1
AAOS_OLLAMA_URL=http://localhost:11434
AAOS_DEFAULT_MODEL=ollama:llama3.2:3b
AAOS_INFERENCE_SLOTS=4
AAOS_CACHE_SIZE_MB=1024
```

## 7. Deployment Checklist

### 7.1 Prerequisites
- [ ] Ollama installed and running (`ollama pull llama3.2:3b`)
- [ ] GPU drivers (optional, for vLLM)
- [ ] Sufficient RAM for KV cache
- [ ] Network access to Anthropic API (fallback)

### 7.2 Runtime Dependencies
```toml
[dependencies]
# New
aaos-inference = { path = "../aaos-inference" }
aaos-llm-backends = { path = "../aaos-llm-backends" }

# Updated
aaos-llm = { path = "../aaos-llm", features = ["backends"] }
aaos-runtime = { path = "../aaos-runtime", features = ["inference"] }
```

### 7.3 Migration Steps
1. Update `Cargo.toml` with new dependencies
2. Add inference config to existing `aaos.toml`
3. Update runtime initialization to include `InferenceScheduler`
4. Optional: Update agent manifests to specify local models
5. Start with monitoring enabled, budget enforcement disabled
6. Gradually enable features based on stability

## 8. Success Criteria

### 8.1 Functional Requirements
- [ ] Ollama backend handles ≥90% of requests successfully
- [ ] Scheduler maintains FIFO ordering within priority classes
- [ ] Budget enforcement rejects/queues exceeding requests
- [ ] KV cache reduces inference latency by ≥20% for repeated context
- [ ] Fallback to Anthropic works when local backend unavailable

### 8.2 Performance Requirements  
- [ ] Scheduler adds <5ms overhead per request
- [ ] Cache lookup <1ms for cache hits
- [ ] Supports ≥100 concurrent agent inference requests
- [ ] Memory overhead <50MB plus configured cache size

### 8.3 Operational Requirements
- [ ] Clear audit trail for budget violations
- [ ] Runtime configuration without restart
- [ ] Metrics export for monitoring
- [ ] Graceful degradation under load

---

*This implementation plan provides concrete Rust types and trait signatures for building Phase E incrementally while maintaining compatibility with existing aaOS components.*