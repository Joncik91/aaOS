# Pattern Storage Design: Enhanced Execution Tracing and Pattern Recognition

## Phase 1 Design Goals

1. **Enhanced Execution Tracing**: Capture detailed execution data
2. **Simple Pattern Recognition**: Store and retrieve execution patterns
3. **Basic Metrics**: Calculate success rates, costs, and timing
4. **Backward Compatibility**: Work with existing MemoryStore

## 1. Extended MemoryCategory Enum

```rust
// /src/crates/aaos-memory/src/types.rs

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MemoryCategory {
    Fact,
    Observation,
    Decision,
    Preference,
    // New categories for Phase 1
    ExecutionTrace,    // Raw execution data
    ExecutionPattern,  // Recognized patterns
    AgentProfile,      // Agent behavior profiles
    CapabilityUsage,   // Capability usage patterns
}

impl MemoryCategory {
    pub fn as_str(&self) -> &'static str {
        match self {
            MemoryCategory::Fact => "fact",
            MemoryCategory::Observation => "observation",
            MemoryCategory::Decision => "decision",
            MemoryCategory::Preference => "preference",
            MemoryCategory::ExecutionTrace => "execution_trace",
            MemoryCategory::ExecutionPattern => "execution_pattern",
            MemoryCategory::AgentProfile => "agent_profile",
            MemoryCategory::CapabilityUsage => "capability_usage",
        }
    }
}
```

## 2. Structured Execution Data Schema

```rust
// /src/crates/aaos-memory/src/execution.rs

use serde::{Deserialize, Serialize};
use std::time::{Duration, SystemTime};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutionTrace {
    pub trace_id: String,           // Unique identifier for this trace
    pub parent_trace_id: Option<String>, // For nested executions
    pub agent_id: String,           // ID of the executing agent
    pub task_description: String,   // What was being attempted
    pub start_time: SystemTime,
    pub end_time: Option<SystemTime>,
    pub duration: Option<Duration>,
    
    // Execution components
    pub spawned_agents: Vec<AgentSpawn>,
    pub capabilities_used: Vec<CapabilityUsage>,
    pub decisions_made: Vec<Decision>,
    pub observations_recorded: Vec<Observation>,
    
    // Cost and resource tracking
    pub token_cost: Option<u64>,
    pub memory_usage: Option<u64>,
    
    // Outcome
    pub success: bool,
    pub error_message: Option<String>,
    pub result_summary: String,
    
    // Metadata
    pub tags: Vec<String>,
    pub priority: u8,  // 0-255, higher is more important
    pub confidence: f32, // 0.0-1.0 confidence in the trace accuracy
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentSpawn {
    pub agent_id: String,
    pub agent_type: String,
    pub purpose: String,
    pub duration: Option<Duration>,
    pub success: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CapabilityUsage {
    pub capability_name: String,
    pub parameters: serde_json::Value,
    pub duration: Option<Duration>,
    pub success: bool,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Decision {
    pub decision_id: String,
    pub options_considered: Vec<String>,
    pub chosen_option: String,
    pub reasoning: String,
    pub confidence: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Observation {
    pub observation_id: String,
    pub content: String,
    pub source: String,  // e.g., "file_read", "memory_query"
    pub relevance: f32,
}
```

## 3. Execution Pattern Schema

```rust
// /src/crates/aaos-memory/src/patterns.rs

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutionPattern {
    pub pattern_id: String,
    pub name: String,
    pub description: String,
    
    // Pattern definition
    pub sequence: Vec<PatternStep>,
    pub preconditions: Vec<PatternCondition>,
    pub postconditions: Vec<PatternCondition>,
    
    // Statistical data
    pub frequency: u32,
    pub success_rate: f32,
    pub avg_duration: Option<Duration>,
    pub avg_token_cost: Option<f64>,
    
    // Examples
    pub example_trace_ids: Vec<String>,
    
    // Metadata
    pub category: PatternCategory,
    pub complexity: PatternComplexity,
    pub last_observed: SystemTime,
    pub confidence: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PatternStep {
    pub step_type: StepType,
    pub agent_type: Option<String>,
    pub capability: Option<String>,
    pub parameters: Option<serde_json::Value>,
    pub expected_outcome: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum StepType {
    AgentSpawn,
    CapabilityUse,
    DecisionPoint,
    Observation,
    Wait,  // For timing patterns
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PatternCondition {
    pub condition_type: ConditionType,
    pub field: String,
    pub operator: Operator,
    pub value: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ConditionType {
    Precondition,
    Postcondition,
    Invariant,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Operator {
    Equals,
    NotEquals,
    GreaterThan,
    LessThan,
    Contains,
    StartsWith,
    EndsWith,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum PatternCategory {
    AgentCoordination,
    CapabilitySequence,
    DecisionFlow,
    ErrorRecovery,
    Optimization,
    CommonWorkflow,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum PatternComplexity {
    Simple,     // 1-3 steps
    Moderate,   // 4-7 steps
    Complex,    // 8+ steps
    Nested,     // Contains sub-patterns
}
```

## 4. Enhanced MemoryStore Trait

```rust
// /src/crates/aaos-memory/src/store.rs

pub trait MemoryStore {
    // Existing methods...
    
    // New methods for Phase 1
    
    /// Store an execution trace with structured data
    fn store_execution_trace(&self, trace: &ExecutionTrace) -> Result<String, StoreError>;
    
    /// Store an execution pattern
    fn store_execution_pattern(&self, pattern: &ExecutionPattern) -> Result<String, StoreError>;
    
    /// Retrieve execution traces by criteria
    fn get_execution_traces(
        &self,
        filters: &TraceFilters,
        limit: usize,
    ) -> Result<Vec<ExecutionTrace>, StoreError>;
    
    /// Find patterns matching a trace
    fn find_matching_patterns(
        &self,
        trace: &ExecutionTrace,
        similarity_threshold: f32,
    ) -> Result<Vec<ExecutionPattern>, StoreError>;
    
    /// Calculate basic metrics for a time period
    fn calculate_metrics(
        &self,
        start_time: SystemTime,
        end_time: SystemTime,
    ) -> Result<ExecutionMetrics, StoreError>;
    
    /// Update pattern statistics based on new trace
    fn update_pattern_from_trace(
        &self,
        pattern_id: &str,
        trace: &ExecutionTrace,
        success: bool,
    ) -> Result<(), StoreError>;
}

#[derive(Debug, Clone)]
pub struct TraceFilters {
    pub agent_id: Option<String>,
    pub success: Option<bool>,
    pub min_duration: Option<Duration>,
    pub max_duration: Option<Duration>,
    pub capabilities_used: Option<Vec<String>>,
    pub tags: Option<Vec<String>>,
    pub start_time_range: Option<(SystemTime, SystemTime)>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutionMetrics {
    pub total_executions: u64,
    pub successful_executions: u64,
    pub failed_executions: u64,
    pub success_rate: f32,
    pub avg_execution_time: Duration,
    pub total_token_cost: u64,
    pub avg_token_cost: f64,
    pub most_common_patterns: Vec<PatternFrequency>,
    pub capability_usage: Vec<CapabilityStats>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PatternFrequency {
    pub pattern_id: String,
    pub pattern_name: String,
    pub frequency: u32,
    pub success_rate: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CapabilityStats {
    pub capability_name: String,
    pub usage_count: u32,
    pub success_rate: f32,
    pub avg_duration: Duration,
}
```

## 5. Bootstrap Agent Integration

```rust
// Example of how Bootstrap Agent captures execution data

pub struct BootstrapAgent {
    memory_store: Arc<dyn MemoryStore>,
    current_trace: Option<ExecutionTrace>,
    trace_stack: Vec<String>,  // For nested traces
}

impl BootstrapAgent {
    pub fn start_execution_trace(&mut self, task_description: &str) -> String {
        let trace_id = generate_uuid();
        let trace = ExecutionTrace {
            trace_id: trace_id.clone(),
            parent_trace_id: self.trace_stack.last().cloned(),
            agent_id: self.agent_id.clone(),
            task_description: task_description.to_string(),
            start_time: SystemTime::now(),
            end_time: None,
            duration: None,
            spawned_agents: Vec::new(),
            capabilities_used: Vec::new(),
            decisions_made: Vec::new(),
            observations_recorded: Vec::new(),
            token_cost: None,
            memory_usage: None,
            success: false,
            error_message: None,
            result_summary: String::new(),
            tags: Vec::new(),
            priority: 50,
            confidence: 1.0,
        };
        
        self.current_trace = Some(trace);
        self.trace_stack.push(trace_id.clone());
        
        trace_id
    }
    
    pub fn record_capability_use(
        &mut self,
        capability_name: &str,
        parameters: serde_json::Value,
        success: bool,
        error: Option<String>,
        duration: Option<Duration>,
    ) {
        if let Some(ref mut trace) = self.current_trace {
            let usage = CapabilityUsage {
                capability_name: capability_name.to_string(),
                parameters,
                duration,
                success,
                error,
            };
            trace.capabilities_used.push(usage);
        }
    }
    
    pub fn record_agent_spawn(
        &mut self,
        agent_id: &str,
        agent_type: &str,
        purpose: &str,
        success: bool,
        duration: Option<Duration>,
    ) {
        if let Some(ref mut trace) = self.current_trace {
            let spawn = AgentSpawn {
                agent_id: agent_id.to_string(),
                agent_type: agent_type.to_string(),
                purpose: purpose.to_string(),
                duration,
                success,
            };
            trace.spawned_agents.push(spawn);
        }
    }
    
    pub fn end_execution_trace(
        &mut self,
        success: bool,
        result_summary: &str,
        error_message: Option<String>,
    ) -> Result<(), StoreError> {
        if let Some(mut trace) = self.current_trace.take() {
            trace.end_time = Some(SystemTime::now());
            trace.duration = trace.end_time.unwrap().duration_since(trace.start_time).ok();
            trace.success = success;
            trace.result_summary = result_summary.to_string();
            trace.error_message = error_message;
            
            // Store the trace
            self.memory_store.store_execution_trace(&trace)?;
            
            // Try to match patterns
            let patterns = self.memory_store.find_matching_patterns(&trace, 0.7)?;
            
            // Update pattern statistics
            for pattern in patterns {
                self.memory_store.update_pattern_from_trace(
                    &pattern.pattern_id,
                    &trace,
                    success,
                )?;
            }
            
            // Pop from trace stack
            self.trace_stack.pop();
        }
        
        Ok(())
    }
    
    pub fn analyze_patterns(&self) -> Result<Vec<ExecutionPattern>, StoreError> {
        // Simple pattern recognition algorithm
        let filters = TraceFilters {
            agent_id: Some(self.agent_id.clone()),
            success: Some(true),
            min_duration: None,
            max_duration: None,
            capabilities_used: None,
            tags: None,
            start_time_range: None,
        };
        
        let traces = self.memory_store.get_execution_traces(&filters, 100)?;
        
        // Group similar traces and create patterns
        let patterns = self.identify_common_sequences(&traces);
        
        Ok(patterns)
    }
    
    fn identify_common_sequences(&self, traces: &[ExecutionTrace]) -> Vec<ExecutionPattern> {
        // Simplified pattern identification
        // In Phase 1, we look for exact sequence matches
        let mut sequence_counts: HashMap<String, (Vec<PatternStep>, u32)> = HashMap::new();
        
        for trace in traces {
            let sequence = self.extract_sequence(trace);
            let sequence_key = self.hash_sequence(&sequence);
            
            sequence_counts
                .entry(sequence_key)
                .and_modify(|(_, count)| *count += 1)
                .or_insert((sequence, 1));
        }
        
        // Convert to patterns
        sequence_counts
            .into_iter()
            .filter(|(_, (_, count))| *count >= 3)  // Minimum frequency
            .map(|(key, (sequence, frequency))| {
                ExecutionPattern {
                    pattern_id: generate_uuid(),
                    name: format!("Pattern_{}", &key[..8]),
                    description: format!("Common sequence observed {} times", frequency),
                    sequence,
                    preconditions: Vec::new(),
                    postconditions: Vec::new(),
                    frequency,
                    success_rate: 0.8,  // Would calculate from actual data
                    avg_duration: None,
                    avg_token_cost: None,
                    example_trace_ids: Vec::new(),  // Would populate
                    category: PatternCategory::CommonWorkflow,
                    complexity: self.assess_complexity(&sequence),
                    last_observed: SystemTime::now(),
                    confidence: 0.7,
                }
            })
            .collect()
    }
}
```

## 6. Serialization and Storage Strategy

```rust
// Memory content serialization for new categories

impl Memory {
    pub fn from_execution_trace(trace: ExecutionTrace) -> Self {
        let content = serde_json::to_string(&trace)
            .expect("Failed to serialize execution trace");
        
        Memory {
            id: generate_uuid(),
            timestamp: SystemTime::now(),
            category: MemoryCategory::ExecutionTrace,
            scope: MemoryScope::Private,
            content,
            metadata: HashMap::new(),
        }
    }
    
    pub fn from_execution_pattern(pattern: ExecutionPattern) -> Self {
        let content = serde_json::to_string(&pattern)
            .expect("Failed to serialize execution pattern");
        
        let mut metadata = HashMap::new();
        metadata.insert("frequency".to_string(), pattern.frequency.to_string());
        metadata.insert("complexity".to_string(), format!("{:?}", pattern.complexity));
        
        Memory {
            id: generate_uuid(),
            timestamp: SystemTime::now(),
            category: MemoryCategory::ExecutionPattern,
            scope: MemoryScope::Private,
            content,
            metadata,
        }
    }
    
    pub fn to_execution_trace(&self) -> Result<ExecutionTrace, serde_json::Error> {
        serde_json::from_str(&self.content)
    }
    
    pub fn to_execution_pattern(&self) -> Result<ExecutionPattern, serde_json::Error> {
        serde_json::from_str(&self.content)
    }
}
```

## 7. Query Enhancements

```rust
// Enhanced query capabilities for patterns

pub struct PatternQuery {
    pub min_frequency: Option<u32>,
    pub min_success_rate: Option<f32>,
    pub categories: Option<Vec<PatternCategory>>,
    pub max_complexity: Option<PatternComplexity>,
    pub contains_step_types: Option<Vec<StepType>>,
    pub contains_capabilities: Option<Vec<String>>,
}

impl MemoryStore {
    pub fn query_patterns(
        &self,
        query: &PatternQuery,
        limit: usize,
    ) -> Result<Vec<ExecutionPattern>, StoreError> {
        // Implementation would filter patterns based on query criteria
        // This can be built on top of existing query infrastructure
        unimplemented!()
    }
    
    pub fn suggest_pattern_for_task(
        &self,
        task_description: &str,
        available_capabilities: &[String],
    ) -> Result<Option<ExecutionPattern>, StoreError> {
        // Simple pattern suggestion based on task similarity
        // and capability availability
        unimplemented!()
    }
}
```

## 8. Metrics Collection Endpoint

```rust
// API for accessing execution metrics

pub struct MetricsCollector {
    memory_store: Arc<dyn MemoryStore>,
}

impl MetricsCollector {
    pub async fn get_dashboard_metrics(
        &self,
        time_range: TimeRange,
    ) -> Result<DashboardMetrics, StoreError> {
        let execution_metrics = self.memory_store.calculate_metrics(
            time_range.start,
            time_range.end,
        )?;
        
        let patterns = self.memory_store.query_patterns(
            &PatternQuery {
                min_frequency: Some(5),
                min_success_rate: Some(0.7),
                categories: None,
                max_complexity: None,
                contains_step_types: None,
                contains_capabilities: None,
            },
            10,
        )?;
        
        Ok(DashboardMetrics {
            execution_metrics,
            top_patterns: patterns,
            recent_traces: self.get_recent_traces(20)?,
            capability_heatmap: self.generate_capability_heatmap(time_range)?,
        })
    }
}
```

## Summary

This design provides:
1. **Structured execution data capture** with rich metadata
2. **Pattern storage and retrieval** with statistical tracking
3. **Backward compatibility** through enum extensions
4. **Incremental implementation** path
5. **Basic metrics calculation** for monitoring and optimization

Phase 1 focuses on capturing data and simple pattern recognition, laying the foundation for more advanced AI-driven pattern analysis in later phases.