/**
 * Pattern Storage and Retrieval Functions for Bootstrap Agent Evolution
 * 
 * This module provides functions to store, retrieve, and manage behavioral patterns
 * using the existing memory_store and memory_query capabilities.
 */

// Pattern Storage Functions
class PatternStorage {
  constructor() {
    this.PATTERN_CATEGORY = 'decision'; // Using existing decision category
    this.TRACE_CATEGORY = 'decision';   // Execution traces also stored as decisions
    this.PATTERN_PREFIX = 'pattern:';
    this.TRACE_PREFIX = 'trace:';
  }

  /**
   * Store an execution trace
   * @param {Object} traceData - Trace data following execution_trace_schema
   * @returns {Promise<string>} - Memory UUID
   */
  async storeExecutionTrace(traceData) {
    const traceId = traceData.trace_id || `trace_${Date.now()}_${Math.random().toString(36).substr(2, 9)}`;
    const enhancedTrace = {
      ...traceData,
      trace_id: traceId,
      metadata: {
        ...traceData.metadata,
        stored_at: new Date().toISOString(),
        type: 'execution_trace'
      }
    };

    const content = JSON.stringify(enhancedTrace);
    const memoryKey = `${this.TRACE_PREFIX}${traceId}`;
    
    // Store with semantic content for retrieval
    const memoryContent = `${memoryKey}\nGoal: ${traceData.goal}\n${content}`;
    
    // Use memory_store with decision category (already available)
    // Note: In actual implementation, this would call memory_store API
    return this._storeMemory(memoryContent, this.TRACE_CATEGORY);
  }

  /**
   * Extract pattern from successful execution trace
   * @param {Object} traceData - Successful execution trace
   * @param {Object} options - Pattern extraction options
   * @returns {Object} - Pattern following pattern_template_schema
   */
  extractPatternFromTrace(traceData, options = {}) {
    const {
      patternName = `Pattern_${Date.now()}`,
      description = `Extracted from trace ${traceData.trace_id}`,
      tags = []
    } = options;

    // Analyze trace to create pattern
    const steps = traceData.plan.map(step => ({
      step_type: 'action',
      action_template: this._createActionTemplate(step),
      tool_preference: step.tool ? [step.tool] : [],
      conditions: {},
      fallbacks: []
    }));

    // Calculate initial metrics from trace
    const metrics = {
      success_rate: traceData.outcome.success ? 1.0 : 0.0,
      average_cost: traceData.outcome.cost_units || 0,
      average_duration_ms: traceData.outcome.total_duration_ms || 0,
      execution_count: 1,
      last_used: new Date().toISOString(),
      adaptation_rate: 0.0
    };

    const patternId = `pattern_${Date.now()}_${Math.random().toString(36).substr(2, 9)}`;

    return {
      pattern_id: patternId,
      name: patternName,
      description: description,
      goal_pattern: this._createGoalPattern(traceData.goal),
      goal_embeddings: [], // Would be populated by embedding service
      strategy: steps,
      metrics: metrics,
      constraints: {
        required_capabilities: this._extractRequiredCapabilities(traceData),
        environment_requirements: [],
        complexity_limit: this._assessComplexity(traceData)
      },
      metadata: {
        created_from: traceData.trace_id,
        created_at: new Date().toISOString(),
        last_updated: new Date().toISOString(),
        version: '1.0',
        tags: ['extracted', ...tags]
      }
    };
  }

  /**
   * Store a behavioral pattern
   * @param {Object} patternData - Pattern data following pattern_template_schema
   * @returns {Promise<string>} - Memory UUID
   */
  async storePattern(patternData) {
    const patternId = patternData.pattern_id;
    const enhancedPattern = {
      ...patternData,
      metadata: {
        ...patternData.metadata,
        stored_at: new Date().toISOString(),
        type: 'behavioral_pattern'
      }
    };

    const content = JSON.stringify(enhancedPattern);
    const memoryKey = `${this.PATTERN_PREFIX}${patternId}`;
    
    // Store with semantic content for retrieval
    const memoryContent = `${memoryKey}\nPattern: ${patternData.name}\nGoal Pattern: ${patternData.goal_pattern}\n${content}`;
    
    return this._storeMemory(memoryContent, this.PATTERN_CATEGORY);
  }

  /**
   * Find patterns matching a goal
   * @param {string} goal - The goal to match patterns against
   * @param {Object} options - Search options
   * @returns {Promise<Array>} - Matching patterns with scores
   */
  async findMatchingPatterns(goal, options = {}) {
    const {
      limit = 5,
      minSimilarity = 0.3,
      requireCapabilities = true
    } = options;

    // Use semantic search to find relevant patterns
    const query = `pattern goal strategy for: ${goal}`;
    
    // Note: In actual implementation, this would call memory_query API
    const memories = await this._queryMemory(query, this.PATTERN_CATEGORY, limit * 2); // Get extra for filtering
    
    const patterns = [];
    
    for (const memory of memories) {
      try {
        const pattern = this._extractPatternFromMemory(memory.content);
        if (!pattern) continue;

        // Calculate similarity score
        const similarityScore = this._calculateGoalSimilarity(goal, pattern.goal_pattern);
        
        if (similarityScore >= minSimilarity) {
          // Check capability constraints if required
          if (requireCapabilities && pattern.constraints?.required_capabilities) {
            const hasCapabilities = await this._checkCapabilities(pattern.constraints.required_capabilities);
            if (!hasCapabilities) continue;
          }

          patterns.push({
            pattern: pattern,
            similarity_score: similarityScore,
            confidence: this._calculateConfidence(pattern.metrics),
            memory_id: memory.id
          });
        }
      } catch (error) {
        console.warn('Error parsing pattern from memory:', error);
      }
    }

    // Sort by similarity score and confidence
    patterns.sort((a, b) => {
      const scoreA = (a.similarity_score * 0.7) + (a.confidence * 0.3);
      const scoreB = (b.similarity_score * 0.7) + (b.confidence * 0.3);
      return scoreB - scoreA;
    });

    return patterns.slice(0, limit);
  }

  /**
   * Update pattern metrics after execution
   * @param {string} patternId - Pattern ID to update
   * @param {Object} executionResult - Result of pattern execution
   * @returns {Promise<boolean>} - Success status
   */
  async updatePatternMetrics(patternId, executionResult) {
    const {
      success,
      cost_units = 0,
      duration_ms = 0,
      adaptations_made = 0
    } = executionResult;

    // Find the pattern memory
    const query = `pattern_id: ${patternId}`;
    const memories = await this._queryMemory(query, this.PATTERN_CATEGORY, 1);
    
    if (memories.length === 0) {
      return false;
    }

    const memory = memories[0];
    const pattern = this._extractPatternFromMemory(memory.content);
    
    if (!pattern) {
      return false;
    }

    // Update metrics
    const oldMetrics = pattern.metrics;
    const executionCount = oldMetrics.execution_count + 1;
    
    // Update success rate (moving average)
    const newSuccessRate = ((oldMetrics.success_rate * oldMetrics.execution_count) + (success ? 1 : 0)) / executionCount;
    
    // Update average cost (moving average)
    const newAvgCost = ((oldMetrics.average_cost * oldMetrics.execution_count) + cost_units) / executionCount;
    
    // Update average duration (moving average)
    const newAvgDuration = ((oldMetrics.average_duration_ms * oldMetrics.execution_count) + duration_ms) / executionCount;
    
    // Update adaptation rate
    const adaptationRate = adaptations_made > 0 ? 1 : 0;
    const newAdaptationRate = ((oldMetrics.adaptation_rate * oldMetrics.execution_count) + adaptationRate) / executionCount;

    pattern.metrics = {
      success_rate: newSuccessRate,
      average_cost: newAvgCost,
      average_duration_ms: newAvgDuration,
      execution_count: executionCount,
      last_used: new Date().toISOString(),
      adaptation_rate: newAdaptationRate
    };

    pattern.metadata.last_updated = new Date().toISOString();

    // Store updated pattern (replaces old memory)
    const updatedContent = JSON.stringify(pattern);
    const memoryKey = `${this.PATTERN_PREFIX}${patternId}`;
    const memoryContent = `${memoryKey}\nPattern: ${pattern.name}\nGoal Pattern: ${pattern.goal_pattern}\n${updatedContent}`;
    
    await this._storeMemory(memoryContent, this.PATTERN_CATEGORY, memory.id);
    
    return true;
  }

  /**
   * Get pattern statistics
   * @returns {Promise<Object>} - Pattern storage statistics
   */
  async getPatternStatistics() {
    const query = 'behavioral pattern metrics';
    const memories = await this._queryMemory(query, this.PATTERN_CATEGORY, 100);
    
    const stats = {
      total_patterns: 0,
      by_success_rate: { high: 0, medium: 0, low: 0 },
      by_complexity: { simple: 0, medium: 0, complex: 0 },
      average_metrics: {
        success_rate: 0,
        execution_count: 0,
        adaptation_rate: 0
      }
    };

    let totalSuccessRate = 0;
    let totalExecutionCount = 0;
    let totalAdaptationRate = 0;

    for (const memory of memories) {
      try {
        const pattern = this._extractPatternFromMemory(memory.content);
        if (!pattern) continue;

        stats.total_patterns++;
        
        // Categorize by success rate
        if (pattern.metrics.success_rate >= 0.8) {
          stats.by_success_rate.high++;
        } else if (pattern.metrics.success_rate >= 0.5) {
          stats.by_success_rate.medium++;
        } else {
          stats.by_success_rate.low++;
        }

        // Categorize by complexity
        const complexity = pattern.constraints?.complexity_limit || 'medium';
        stats.by_complexity[complexity] = (stats.by_complexity[complexity] || 0) + 1;

        // Accumulate for averages
        totalSuccessRate += pattern.metrics.success_rate;
        totalExecutionCount += pattern.metrics.execution_count;
        totalAdaptationRate += pattern.metrics.adaptation_rate;
      } catch (error) {
        continue;
      }
    }

    if (stats.total_patterns > 0) {
      stats.average_metrics.success_rate = totalSuccessRate / stats.total_patterns;
      stats.average_metrics.execution_count = totalExecutionCount / stats.total_patterns;
      stats.average_metrics.adaptation_rate = totalAdaptationRate / stats.total_patterns;
    }

    return stats;
  }

  // Helper methods (would be implemented based on actual environment)
  
  async _storeMemory(content, category, replaces = null) {
    // This is a placeholder for the actual memory_store API call
    // In the Bootstrap Agent, this would be:
    // return memory_store({ content, category, replaces });
    
    // For demonstration, return a mock UUID
    return `memory_${Date.now()}_${Math.random().toString(36).substr(2, 9)}`;
  }

  async _queryMemory(query, category, limit) {
    // This is a placeholder for the actual memory_query API call
    // In the Bootstrap Agent, this would be:
    // return memory_query({ query, category, limit });
    
    // For demonstration, return empty array
    return [];
  }

  _extractPatternFromMemory(content) {
    try {
      // Extract JSON from memory content (pattern is after first newline)
      const lines = content.split('\n');
      const jsonStart = lines.findIndex(line => line.startsWith('{'));
      if (jsonStart === -1) return null;
      
      const jsonContent = lines.slice(jsonStart).join('\n');
      return JSON.parse(jsonContent);
    } catch (error) {
      return null;
    }
  }

  _calculateGoalSimilarity(goal, goalPattern) {
    // Simple similarity calculation based on word overlap
    // In production, this would use embeddings or more sophisticated NLP
    
    const goalWords = new Set(goal.toLowerCase().split(/\W+/).filter(w => w.length > 2));
    const patternWords = new Set(goalPattern.toLowerCase().split(/\W+/).filter(w => w.length > 2));
    
    if (goalWords.size === 0 || patternWords.size === 0) return 0;
    
    const intersection = new Set([...goalWords].filter(x => patternWords.has(x)));
    const union = new Set([...goalWords, ...patternWords]);
    
    return intersection.size / union.size;
  }

  _calculateConfidence(metrics) {
    // Confidence based on execution count and success rate
    const executionWeight = Math.min(metrics.execution_count / 10, 1.0);
    const successWeight = metrics.success_rate;
    
    return (executionWeight * 0.4) + (successWeight * 0.6);
  }

  _createActionTemplate(step) {
    // Create a parameterized action template from a step
    if (step.action.includes('file')) {
      return `Perform ${step.action} operation on {target}`;
    } else if (step.action.includes('memory')) {
      return `Store or query memory for {purpose}`;
    } else {
      return `Execute ${step.action} with appropriate parameters`;
    }
  }

  _createGoalPattern(goal) {
    // Create a generalized goal pattern
    const words = goal.toLowerCase().split(/\W+/).filter(w => w.length > 3);
    const commonWords = ['create', 'build', 'make', 'generate', 'write', 'read', 'store', 'query'];
    
    // Remove very common words
    const significantWords = words.filter(w => !commonWords.includes(w));
    
    if (significantWords.length > 0) {
      return `*${significantWords.slice(0, 3).join('* *')}*`;
    } else {
      return goal.substring(0, 50) + '...';
    }
  }

  _extractRequiredCapabilities(traceData) {
    const capabilities = new Set();
    
    for (const step of traceData.plan) {
      if (step.tool) {
        capabilities.add(step.tool);
      }
    }
    
    return Array.from(capabilities);
  }

  _assessComplexity(traceData) {
    const stepCount = traceData.plan.length;
    
    if (stepCount <= 3) return 'simple';
    if (stepCount <= 7) return 'medium';
    return 'complex';
  }

  async _checkCapabilities(requiredCapabilities) {
    // This would check if the agent has the required capabilities
    // For now, assume all capabilities are available
    return true;
  }
}

// Pattern Matching and Selection Functions
class PatternMatcher {
  constructor(storage) {
    this.storage = storage;
  }

  /**
   * Select the best pattern for a goal
   * @param {string} goal - The goal to achieve
   * @param {Object} context - Execution context
   * @returns {Promise<Object>} - Selected pattern and adaptation plan
   */
  async selectPatternForGoal(goal, context = {}) {
    // Find matching patterns
    const matchingPatterns = await this.storage.findMatchingPatterns(goal, {
      limit: 10,
      minSimilarity: 0.2,
      requireCapabilities: true
    });

    if (matchingPatterns.length === 0) {
      return {
        selected: null,
        reason: 'No matching patterns found',
        adaptation_required: true,
        adaptation_type: 'create_new'
      };
    }

    // Score each pattern
    const scoredPatterns = matchingPatterns.map(match => {
      const score = this._calculatePatternScore(match, context);
      return {
        ...match,
        selection_score: score
      };
    });

    // Sort by score
    scoredPatterns.sort((a, b) => b.selection_score - a.selection_score);

    const bestPattern = scoredPatterns[0];
    const adaptationAnalysis = this._analyzeAdaptationRequirements(goal, bestPattern.pattern, context);

    return {
      selected: bestPattern.pattern,
      selection_score: bestPattern.selection_score,
      similarity_score: bestPattern.similarity_score,
      confidence: bestPattern.confidence,
      reason: this._generateSelectionReason(bestPattern),
      adaptation_required: adaptationAnalysis.required,
      adaptation_points: adaptationAnalysis.points,
      alternative_patterns: scoredPatterns.slice(1, 3).map(p => ({
        pattern_id: p.pattern.pattern_id,
        name: p.pattern.name,
        score: p.selection_score
      }))
    };
  }

  _calculatePatternScore(match, context) {
    const pattern = match.pattern;
    const weights = {
      similarity: 0.4,
      success_rate: 0.3,
      efficiency: 0.2,
      freshness: 0.1
    };

    // Similarity score (already calculated)
    const similarityScore = match.similarity_score;

    // Success rate score
    const successScore = pattern.metrics.success_rate;

    // Efficiency score (inverse of cost and duration)
    const maxExpectedCost = context.max_cost || 100;
    const maxExpectedDuration = context.max_duration || 60000;
    
    const costScore = Math.max(0, 1 - (pattern.metrics.average_cost / maxExpectedCost));
    const durationScore = Math.max(0, 1 - (pattern.metrics.average_duration_ms / maxExpectedDuration));
    const efficiencyScore = (costScore + durationScore) / 2;

    // Freshness score (prefer recently used patterns)
    const lastUsed = new Date(pattern.metrics.last_used);
    const daysSinceUse = (Date.now() - lastUsed.getTime()) / (1000 * 60 * 60 * 24);
    const freshnessScore = Math.max(0, 1 - (daysSinceUse / 30)); // Decay over 30 days

    // Calculate weighted score
    return (
      similarityScore * weights.similarity +
      successScore * weights.success_rate +
      efficiencyScore * weights.efficiency +
      freshnessScore * weights.freshness
    );
  }

  _analyzeAdaptationRequirements(goal, pattern, context) {
    const adaptationPoints = [];
    let required = false;

    // Check goal specificity
    if (pattern.goal_pattern.includes('*') && !goal.toLowerCase().includes(pattern.goal_pattern.replace(/\*/g, '').toLowerCase())) {
      adaptationPoints.push('Goal specificity mismatch');
      required = true;
    }

    // Check capability constraints
    if (pattern.constraints?.required_capabilities) {
      const missingCapabilities = this._checkMissingCapabilities(pattern.constraints.required_capabilities, context);
      if (missingCapabilities.length > 0) {
        adaptationPoints.push(`Missing capabilities: ${missingCapabilities.join(', ')}`);
        required = true;
      }
    }

    // Check environment requirements
    if (pattern.constraints?.environment_requirements) {
      const missingEnv = this._checkEnvironmentRequirements(pattern.constraints.environment_requirements, context);
      if (missingEnv.length > 0) {
        adaptationPoints.push(`Missing environment features: ${missingEnv.join(', ')}`);
        required = true;
      }
    }

    // Check complexity match
    if (pattern.constraints?.complexity_limit) {
      const goalComplexity = this._assessGoalComplexity(goal, context);
      const patternComplexity = pattern.constraints.complexity_limit;
      
      if (this._complexityMismatch(goalComplexity, patternComplexity)) {
        adaptationPoints.push(`Complexity mismatch: goal is ${goalComplexity}, pattern handles ${patternComplexity}`);
        required = true;
      }
    }

    return {
      required,
      points: adaptationPoints
    };
  }

  _generateSelectionReason(bestPattern) {
    const reasons = [];
    
    if (bestPattern.similarity_score >= 0.8) {
      reasons.push('excellent goal match');
    } else if (bestPattern.similarity_score >= 0.6) {
      reasons.push('good goal match');
    }
    
    if (bestPattern.pattern.metrics.success_rate >= 0.9) {
      reasons.push('high success rate');
    } else if (bestPattern.pattern.metrics.success_rate >= 0.7) {
      reasons.push('good success rate');
    }
    
    if (bestPattern.pattern.metrics.execution_count >= 5) {
      reasons.push('well-tested pattern');
    }
    
    if (bestPattern.confidence >= 0.8) {
      reasons.push('high confidence');
    }
    
    return `Selected because of ${reasons.join(', ')}`;
  }

  _checkMissingCapabilities(requiredCapabilities, context) {
    const availableCapabilities = context.capabilities || ['file_read', 'file_write', 'memory_store', 'memory_query'];
    return requiredCapabilities.filter(cap => !availableCapabilities.includes(cap));
  }

  _checkEnvironmentRequirements(requirements, context) {
    const environment = context.environment || {};
    return requirements.filter(req => !environment[req]);
  }

  _assessGoalComplexity(goal, context) {
    const goalLength = goal.length;
    const hasMultipleSteps = goal.includes(' and ') || goal.includes(' then ') || goal.includes(', ');
    
    if (goalLength < 50 && !hasMultipleSteps) return 'simple';
    if (goalLength < 150 && !hasMultipleSteps) return 'medium';
    return 'complex';
  }

  _complexityMismatch(goalComplexity, patternComplexity) {
    const complexityOrder = { simple: 1, medium: 2, complex: 3 };
    return complexityOrder[goalComplexity] > complexityOrder[patternComplexity];
  }
}

// Export the classes for use
module.exports = {
  PatternStorage,
  PatternMatcher
};

// Example usage:
/*
const { PatternStorage, PatternMatcher } = require('./pattern-storage.js');

async function example() {
  const storage = new PatternStorage();
  const matcher = new PatternMatcher(storage);
  
  // Store an execution trace
  const trace = {
    goal: "Read a file and store its content in memory",
    plan: [...],
    outcome: { success: true, cost_units: 2, total_duration_ms: 1500 }
  };
  
  const traceId = await storage.storeExecutionTrace(trace);
  
  // Extract and store pattern
  const pattern = storage.extractPatternFromTrace(trace, {
    patternName: "File-to-Memory Pattern",
    tags: ["file_operations", "memory_storage"]
  });
  
  await storage.storePattern(pattern);
  
  // Find patterns for a new goal
  const goal = "Read configuration file and cache in memory";
  const matches = await matcher.selectPatternForGoal(goal);
  
  console.log('Selected pattern:', matches.selected?.name);
  console.log('Adaptation required:', matches.adaptation_required);
}
*/