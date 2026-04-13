# Phase E: Inference Scheduling - Technical Specification

## 1. Overview and Goals

**Problem:** Current aaOS relies exclusively on external LLM APIs (Anthropic) for inference, treating LLM calls as unbounded external resources rather than schedulable system resources. This prevents cost-effective agent fleets and efficient GPU/NPU utilization.

**Vision:** Treat LLM inference as a first-class schedulable resource, analogous to CPU time scheduling in traditional operating systems. Enable mixed-model agent fleets with local models for routine tasks and API models for complex reasoning.

**Primary Goals:**
1. Integrate local model backends (Ollama, vLLM) alongside Anthropic API
2. Implement inference scheduling with priority-based queue management  
3. Add KV cache management for efficient local model inference
4. Enable per-agent inference budget controls and cost tracking

**Success Metrics:**
- Support for ≥2 local model backends alongside Anthropic
- Scheduler throughput of ≥10 concurrent inference requests
- KV cache hit rate ≥60% for agents with overlapping context
- 50% reduction in inference costs for routine tasks via local models

## 2. Architecture Changes

### 2.1 Extended LlmClient Trait
The existing `LlmClient` trait in `aaos-llm` crate will be extended with backend-specific optimizations:

```rust
pub trait LlmClient: Send + Sync {
    /// Existing methods...
    async fn chat(&self, messages: Vec<Message>) -> Result<String, LlmError>;
    async fn max_context_tokens(&self) -> usize;
    
    /// New Phase E additions:
    async fn chat_with_budget(
        &self, 
        messages: Vec<Message>, 
        budget: InferenceBudget
    ) -> Result<ChatResponse, LlmError>;
    
    fn supports_kv_cache(&self) -> bool;
    fn cache_identifier(&self) -> Option<CacheKey>;
    async fn estimate_cost(&self, messages: &[Message]) -> CostEstimate;
}
```

### 2.2 Model Registry and Backend Selection
- New `ModelRegistry` component for runtime backend selection
- Manifest `model` field extended: `{provider}:{model}` syntax
- Backend resolution: `ollama:llama3.2:3b`, `vllm:mixtral-8x7b`, `anthropic:claude-3-5-sonnet`

### 2.3 Inference Scheduler Service
New `InferenceScheduler` service in `aaos-runtime`:

```rust
pub struct InferenceScheduler {
    /// Multiple priority queues (critical, normal, background)
    queues: HashMap<Priority, VecDeque<InferenceRequest>>,
    /// Active inference slots with budget tracking
    active_slots: HashMap<AgentId, InferenceSlot>,
    /// KV cache manager (for local backends)
    cache_manager: Option<Arc<dyn KvCacheManager>>,
    /// Budget enforcer
    budget_enforcer: BudgetEnforcer,
}

impl InferenceScheduler {
    pub async fn submit(
        &self, 
        request: InferenceRequest, 
        priority: Priority
    ) -> Result<InferenceTicket, SchedulerError>;
    
    pub async fn schedule_next(&mut self) -> Option<ScheduledInference>;
    pub fn check_budget(&self, agent_id: &AgentId) -> BudgetStatus;
}
```

## 3. Local Model Integration

### 3.1 OllamaClient Implementation
- Implements `LlmClient` trait for Ollama's REST API (`/api/generate`, `/api/chat`)
- Supports streaming and non-streaming inference
- Model management: pull, list, delete models via admin API
- Automatic model availability detection

### 3.2 VLLMClient Implementation  
- Integrates with vLLM's OpenAI-compatible API server
- Supports continuous batching and PagedAttention
- Exposes vLLM-specific parameters (temperature, top_p, repetition_penalty)
- Graceful fallback when vLLM server unavailable

### 3.3 Backend Configuration
```yaml
# config/local-models.yaml
backends:
  ollama:
    enabled: true
    url: "http://localhost:11434"
    default_model: "llama3.2:3b"
    cache_dir: "/var/cache/aaos/ollama"
    
  vllm:
    enabled: false  # Enable when GPU available
    url: "http://localhost:8000"
    api_key: null   # OpenAI-compatible format
    
  anthropic:
    enabled: true
    api_key: "${ANTHROPIC_API_KEY}"
```

## 4. Inference Scheduling System

### 4.1 Request Structure
```rust
pub struct InferenceRequest {
    pub agent_id: AgentId,
    pub messages: Vec<Message>,
    pub model_requirements: ModelRequirements,
    pub budget: InferenceBudget,
    pub cache_hint: Option<CacheHint>,
    pub callback: oneshot::Sender<InferenceResult>,
}

pub struct InferenceBudget {
    pub max_tokens: Option<u32>,
    pub max_cost_usd: Option<f64>,
    pub priority: Priority,  // Critical, Normal, Background
    pub deadline: Option<Instant>,
}
```

### 4.2 Scheduler Algorithm
1. **Priority-based dequeuing**: Critical > Normal > Background
2. **Budget-aware selection**: Skip requests exceeding agent's budget
3. **Deadline-aware scheduling**: Earliest deadline first within priority class
4. **Fairness guarantee**: Round-robin within same priority/agent class

### 4.3 Concurrency Model
- Configurable concurrent inference slots (default: GPU-dependent)
- Slot allocation: 1:1 with hardware contexts (GPU streams)
- Oversubscription allowed for CPU-only inference
- Dynamic slot adjustment based on backend capabilities

## 5. KV Cache Management

### 5.1 Cache Hierarchy
```
Per-backend cache pool
    ├── Per-model cache partitions  
    ├── Per-agent cache reservations
    └── Shared cache for overlapping context
```

### 5.2 Cache Manager Interface
```rust
pub trait KvCacheManager: Send + Sync {
    async fn allocate(
        &self,
        agent_id: &AgentId,
        model: &str,
        context_size: usize
    ) -> Result<CacheHandle, CacheError>;
    
    async fn evict(&self, policy: EvictionPolicy) -> EvictionResult;
    
    async fn share_context(
        &self,
        source_agent: &AgentId,
        target_agent: &AgentId
    ) -> Result<SharingResult, CacheError>;
    
    fn hit_rate(&self) -> f64;
}
```

### 5.3 Cache Sharing Strategies
1. **Exact context matching**: Agents analyzing same document
2. **Prefix sharing**: Agents with common conversation prefix  
3. **Semantic similarity**: Embedding-based similarity threshold
4. **Manual sharing**: Explicit `share_cache_with` capability

### 5.4 Eviction Policies
- **LRU**: Least Recently Used (default)
- **LFU**: Least Frequently Used  
- **Budget-aware**: Evict lowest priority/budget agents first
- **Size-based**: Evict largest cache entries first

## 6. Integration with Existing aaOS Components

### 6.1 Agent Execution Pipeline
```
Agent Executor
    ↓
Inference Request Builder
    ↓
Budget Checker (per-agent limits)
    ↓
Inference Scheduler (priority queue)
    ↓
Backend Selector (local vs API)
    ↓
LlmClient with KV cache
    ↓
Response with cost tracking
```

### 6.2 Capability Model Extension
- New capability: `inference:{backend}:{budget}`
- Example: `inference:ollama:1000-tokens-per-minute`
- Budget enforcement via capability tokens
- Capability narrowing: parent can restrict child's inference budget

### 6.3 Audit Trail Additions
New audit events:
- `InferenceRequested`: Request submitted to scheduler
- `InferenceScheduled`: Request dequeued for execution  
- `InferenceCompleted`: Success with token count and cost
- `InferenceBudgetExceeded`: Budget enforcement triggered
- `CacheHit` / `CacheMiss`: KV cache performance
- `ModelBackendChanged`: Fallback between backends

## 7. Configuration and Deployment

### 7.1 Runtime Configuration
```toml
# Runtime config section
[inference]
concurrent_slots = 4
default_priority = "normal"
enable_budget_enforcement = true

[scheduler]
max_queue_depth = 1000
starvation_prevention = true  
preempt_background = true

[cache]
total_size_mb = 4096
eviction_policy = "lru"
enable_sharing = true
similarity_threshold = 0.7
```

### 7.2 Agent Manifest Extensions
```yaml
model: "ollama:llama3.2:3b"  # Backend:model syntax
inference_budget:
  tokens_per_minute: 1000
  max_cost_per_session: 0.10
  priority: "background"
  
capabilities:
  - "inference:ollama:500-tokens-per-minute"
  - "inference:anthropic:0.05-usd-per-session"
```

### 7.3 Deployment Scenarios
1. **Development laptop**: Ollama only, 2 concurrent slots
2. **GPU server**: vLLM + Ollama, 8+ slots, 16GB KV cache  
3. **Hybrid cloud**: Local models for routine + Anthropic for complex
4. **Cost-optimized**: Automatic backend selection based on task complexity

## 8. Migration and Compatibility

### 8.1 Backward Compatibility
- Existing manifests with `model: "claude-3-5-sonnet"` continue working
- No changes required for agent code
- Automatic fallback to Anthropic if local backend unavailable

### 8.2 Gradual Rollout
1. Phase E1: Local backend support (Ollama)
2. Phase E2: Basic scheduler (FIFO)
3. Phase E3: Priority scheduling + budget enforcement  
4. Phase E4: KV cache management
5. Phase E5: Advanced sharing and optimization

### 8.3 Testing Strategy
- Unit tests: Scheduler algorithms, budget math
- Integration: End-to-end with real Ollama/vLLM
- Load testing: Concurrent agent simulation
- Cost validation: USD estimation accuracy
- Fallback testing: Backend failure scenarios

## 9. Future Extensions

### 9.1 Advanced Scheduling
- Predictive scheduling based on agent patterns
- Work-conserving backfilling
- Multi-GPU scheduling
- Federated scheduling across nodes

### 9.2 Optimization
- Speculative decoding for common agent responses
- Batch merging for similar requests
- Model quantization awareness
- Warm/cold start optimization

### 9.3 Observability
- Inference latency histograms
- Cost attribution per goal/workspace
- Cache efficiency dashboards
- Budget forecasting

---

*This specification provides the foundation for treating LLM inference as a first-class schedulable resource within aaOS, enabling cost-effective agent fleets and efficient hardware utilization.*