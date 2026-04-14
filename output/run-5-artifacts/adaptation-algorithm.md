# Adaptive Planning Algorithm for Bootstrap Agent Evolution

## Overview

This document describes the adaptive planning algorithm that enables the Bootstrap Agent to evolve its behavior by learning from execution traces and reusing successful patterns. The algorithm works within the constraints of the existing aaOS memory system, using `memory_store` and `memory_query` for pattern storage and retrieval.

## Core Principles

1. **Incremental Learning**: Learn from every execution, successful or failed
2. **Pattern Reuse**: Reuse successful strategies for similar goals
3. **Adaptive Selection**: Choose patterns based on context and historical performance
4. **Continuous Improvement**: Update pattern metrics with each execution

## Algorithm Components

### 1. Pattern Storage and Retrieval

**Storage Mechanism:**
- Patterns stored as JSON strings in memory content
- Use `memory_store` with category "decision" (existing category)
- Include semantic metadata for retrieval
- Pattern ID format: `pattern:{uuid}`

**Retrieval Mechanism:**
- Use `memory_query` with semantic search
- Search by goal keywords and pattern characteristics
- Filter by capability requirements
- Sort by relevance and confidence

### 2. Pattern Matching Algorithm

```
Algorithm: FindMatchingPatterns(goal, context)
Input: goal (string), context (object)
Output: List of matching patterns with scores

1. Query memory for patterns related to goal
   - Search query: "pattern goal strategy for: {goal}"
   - Category: "decision"
   - Limit: 10 (retrieve extra for filtering)

2. For each retrieved memory:
   a. Parse pattern JSON from memory content
   b. Calculate similarity score:
      similarity = calculateGoalSimilarity(goal, pattern.goal_pattern)
   c. Calculate confidence score:
      confidence = (execution_weight * 0.4) + (success_rate * 0.6)
      where execution_weight = min(execution_count / 10, 1.0)
   d. Check capability constraints:
      if pattern.requires_capabilities ⊆ context.available_capabilities
         include pattern
      else
         exclude pattern

3. Filter patterns with similarity ≥ threshold (default: 0.3)

4. Sort patterns by composite score:
   composite_score = (similarity * 0.7) + (confidence * 0.3)

5. Return top N patterns (default: 5)
```

### 3. Pattern Selection Algorithm

```
Algorithm: SelectBestPattern(goal, context, matching_patterns)
Input: goal, context, list of matching patterns
Output: Selected pattern with adaptation requirements

1. If no matching patterns:
   return {selected: null, adaptation_required: true, type: "create_new"}

2. For each pattern, calculate selection score:
   score = (similarity * w1) + (success_rate * w2) + (efficiency * w3) + (freshness * w4)
   where:
     similarity = pattern.similarity_score
     success_rate = pattern.metrics.success_rate
     efficiency = 1 - (avg_cost/max_cost + avg_duration/max_duration)/2
     freshness = 1 - min(days_since_last_use/30, 1)
     weights: w1=0.4, w2=0.3, w3=0.2, w4=0.1

3. Sort patterns by selection score (descending)

4. Select top pattern

5. Analyze adaptation requirements:
   a. Goal specificity check
   b. Capability compatibility check
   c. Environment requirement check
   d. Complexity level check

6. Return selection result with:
   - Selected pattern
   - Selection score and reason
   - Adaptation requirements
   - Alternative patterns
```

### 4. Pattern Execution and Adaptation

```
Algorithm: ExecuteWithAdaptation(goal, selected_pattern, context)
Input: goal, selected pattern, execution context
Output: Execution result with adaptation metadata

1. If adaptation_required:
   a. Create adapted plan = adaptPattern(selected_pattern, goal, context)
   b. adaptation_count = count of adaptations made
   else:
   a. Use pattern strategy directly
   b. adaptation_count = 0

2. Execute plan with monitoring:
   a. Record start time
   b. Track resource usage (API calls, etc.)
   c. Capture intermediate results

3. Evaluate outcome:
   success = goal_achieved(actual_result, expected_result)
   partial_success = calculate_partial_success(actual_result, goal)
   cost_units = calculate_cost(resource_usage)
   duration_ms = end_time - start_time

4. Create execution trace:
   trace = {
     trace_id: generate_uuid(),
     goal: goal,
     plan: executed_steps_with_results,
     outcome: {success, partial_success, cost_units, duration_ms},
     metadata: {pattern_used: pattern_id, adaptations: adaptation_count}
   }

5. Store execution trace

6. Update pattern metrics:
   if pattern_used:
      updatePatternMetrics(pattern_id, {
        success: success,
        cost_units: cost_units,
        duration_ms: duration_ms,
        adaptations_made: adaptation_count
      })

7. If successful and adaptation_count > 0:
      consider creating new pattern from adapted execution

8. Return execution result with trace_id
```

### 5. Pattern Creation and Evolution

```
Algorithm: CreateOrUpdatePattern(execution_trace)
Input: Execution trace with outcome
Output: New or updated pattern

1. If trace.outcome.success:
   a. Extract pattern from trace:
      - Identify reusable strategy steps
      - Generalize goal to pattern
      - Extract required capabilities
      - Assess complexity level

   b. Check for existing similar patterns:
      similar_patterns = findMatchingPatterns(trace.goal, {minSimilarity: 0.6})

   c. If similar_patterns exists:
        Update most similar pattern with new metrics
        Optionally merge strategies if significantly different
      else:
        Create new pattern with:
          - Pattern ID
          - Generalized goal pattern
          - Extracted strategy
          - Initial metrics from trace
          - Constraints based on execution context

   d. Store pattern in memory

2. If trace.outcome.partial_success ≥ 0.7:
      Consider creating pattern with adaptation notes

3. If trace.outcome.success = false:
      Analyze failure for pattern anti-patterns
      Store as learning experience (not as reusable pattern)
```

## Scoring Functions

### Goal Similarity Calculation

```javascript
function calculateGoalSimilarity(goal1, goal2) {
  // Simple word overlap (production would use embeddings)
  const words1 = new Set(goal1.toLowerCase().split(/\W+/).filter(w => w.length > 2));
  const words2 = new Set(goal2.toLowerCase().split(/\W+/).filter(w => w.length > 2));
  
  if (words1.size === 0 || words2.size === 0) return 0;
  
  const intersection = new Set([...words1].filter(x => words2.has(x)));
  const union = new Set([...words1, ...words2]);
  
  return intersection.size / union.size;
}
```

### Pattern Confidence Calculation

```javascript
function calculatePatternConfidence(metrics) {
  // Confidence based on execution history
  const executionWeight = Math.min(metrics.execution_count / 10, 1.0);
  const successWeight = metrics.success_rate;
  
  return (executionWeight * 0.4) + (successWeight * 0.6);
}
```

### Efficiency Score Calculation

```javascript
function calculateEfficiencyScore(pattern, context) {
  const maxCost = context.max_cost || 100;
  const maxDuration = context.max_duration || 60000;
  
  const costScore = Math.max(0, 1 - (pattern.metrics.average_cost / maxCost));
  const durationScore = Math.max(0, 1 - (pattern.metrics.average_duration_ms / maxDuration));
  
  return (costScore + durationScore) / 2;
}
```

## Adaptation Strategies

### 1. Parameter Adaptation
- Replace template variables with actual values
- Adjust resource limits based on context
- Modify timeout values

### 2. Structural Adaptation
- Add or remove steps based on goal complexity
- Reorder steps for efficiency
- Parallelize independent steps

### 3. Tool Adaptation
- Substitute unavailable tools with alternatives
- Combine multiple tools for complex operations
- Add fallback mechanisms

### 4. Goal Generalization/Specialization
- Broaden pattern for wider applicability
- Specialize pattern for specific use cases
- Create variant patterns for different contexts

## Metrics and Evaluation

### Success Metrics
- **Success Rate**: Percentage of successful executions
- **Partial Success Rate**: Degree of goal achievement (0-1)
- **Adaptation Success Rate**: Success rate after adaptation

### Efficiency Metrics
- **Average Cost**: Resource consumption per execution
- **Average Duration**: Time to complete execution
- **Cost Reduction**: Improvement over time

### Quality Metrics
- **Pattern Reuse Rate**: How often patterns are reused
- **Adaptation Rate**: Frequency of pattern adaptation
- **Pattern Diversity**: Variety of patterns available

### Learning Metrics
- **New Patterns Created**: Patterns added over time
- **Pattern Evolution Rate**: How often patterns are updated
- **Anti-pattern Detection**: Failed strategies identified

## Integration with Bootstrap Agent

### Behavioral Integration Points

1. **Pre-execution Phase**:
   ```javascript
   async function planExecution(goal, context) {
     const patternMatcher = new PatternMatcher(storage);
     const selection = await patternMatcher.selectPatternForGoal(goal, context);
     
     if (selection.selected) {
       return {
         plan: selection.selected.strategy,
         pattern_id: selection.selected.pattern_id,
         adaptation_required: selection.adaptation_required,
         adaptation_points: selection.adaptation_points
       };
     } else {
       return { plan: createNewPlan(goal), pattern_id: null };
     }
   }
   ```

2. **Execution Phase**:
   ```javascript
   async function executeWithLearning(goal, plan, context) {
     const startTime = Date.now();
     const result = await executePlan(plan);
     const duration = Date.now() - startTime;
     
     const trace = createExecutionTrace(goal, plan, result, duration);
     await storage.storeExecutionTrace(trace);
     
     if (context.pattern_id) {
       await storage.updatePatternMetrics(context.pattern_id, {
         success: result.success,
         cost_units: calculateCost(result),
         duration_ms: duration,
         adaptations_made: context.adaptations || 0
       });
     }
     
     return result;
   }
   ```

3. **Post-execution Phase**:
   ```javascript
   async function learnFromExecution(trace) {
     if (trace.outcome.success || trace.outcome.partial_success >= 0.7) {
       const pattern = storage.extractPatternFromTrace(trace);
       await storage.storePattern(pattern);
     }
     
     updateLearningMetrics(trace);
   }
   ```

### Memory Usage Optimization

1. **Pattern Pruning**:
   - Remove low-success patterns (success_rate < 0.3)
   - Archive rarely used patterns (execution_count < 3, last_used > 90 days)
   - Merge similar patterns with high overlap

2. **Trace Management**:
   - Keep recent traces (last 100 executions)
   - Archive old traces with pattern references
   - Compress trace data for storage efficiency

3. **Query Optimization**:
   - Cache frequently accessed patterns
   - Pre-compute similarity scores for common goals
   - Use metadata indexing for faster retrieval

## Implementation Guidelines

### Phase 1: Basic Pattern Storage
1. Implement execution trace storage
2. Implement pattern extraction from successful traces
3. Implement basic pattern retrieval by goal keywords

### Phase 2: Adaptive Selection
1. Implement similarity scoring
2. Implement pattern selection algorithm
3. Add basic adaptation capabilities

### Phase 3: Advanced Learning
1. Implement pattern evolution (merging, splitting)
2. Add anti-pattern detection
3. Implement automated pattern refinement

### Phase 4: Optimization
1. Add caching layer for frequent patterns
2. Implement pattern pruning and archiving
3. Add performance monitoring and tuning

## Testing and Validation

### Test Scenarios
1. **Pattern Creation**: Verify patterns are correctly extracted from traces
2. **Pattern Retrieval**: Test semantic search accuracy
3. **Pattern Selection**: Validate scoring and selection logic
4. **Adaptation**: Test various adaptation strategies
5. **Learning Loop**: Verify continuous improvement over time

### Success Criteria
- Pattern reuse rate increases over time
- Success rate improves with pattern usage
- Execution cost decreases with learning
- Adaptation success rate > 70%

## Limitations and Future Enhancements

### Current Limitations
- Simple similarity scoring (word overlap)
- No true semantic understanding
- Limited adaptation strategies
- Manual pattern review may be needed

### Future Enhancements
- Embedding-based similarity (semantic search)
- Reinforcement learning for pattern selection
- Automated pattern refinement
- Multi-agent pattern sharing
- Context-aware adaptation
- Predictive pattern pre-loading

## Conclusion

This adaptive planning algorithm enables the Bootstrap Agent to evolve from simple rule-based execution to learned behavioral patterns. By storing, retrieving, and adapting successful strategies, the agent can improve its performance over time while working within the constraints of the existing aaOS memory system.