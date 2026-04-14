"""
Pattern Storage and Adaptive Planning for Bootstrap Agent Evolution
Python implementation using existing memory_store/memory_query capabilities
"""

import json
import re
import time
import uuid
from datetime import datetime
from typing import Dict, List, Optional, Any, Tuple
from dataclasses import dataclass, asdict
from enum import Enum

# Mock memory API functions (would be replaced with actual aaOS calls)
def memory_store(content: str, category: str, replaces: Optional[str] = None) -> str:
    """Mock memory_store function - in real implementation, calls aaOS memory_store"""
    # This would be replaced with actual memory_store call
    memory_id = f"memory_{int(time.time())}_{uuid.uuid4().hex[:8]}"
    print(f"[MOCK] Stored memory: {memory_id}, category: {category}")
    return memory_id

def memory_query(query: str, category: Optional[str] = None, limit: int = 5) -> List[Dict]:
    """Mock memory_query function - in real implementation, calls aaOS memory_query"""
    # This would be replaced with actual memory_query call
    print(f"[MOCK] Query: {query}, category: {category}, limit: {limit}")
    return []

# Data Models
@dataclass
class ExecutionStep:
    """Represents a single step in an execution plan"""
    action: str
    tool: Optional[str] = None
    parameters: Optional[Dict] = None
    result: Optional[Dict] = None
    timestamp: Optional[str] = None
    duration_ms: Optional[float] = None
    
    def to_dict(self) -> Dict:
        return asdict(self)

@dataclass
class ExecutionOutcome:
    """Represents the outcome of an execution"""
    success: bool
    partial_success: Optional[float] = None
    error: Optional[str] = None
    cost_units: Optional[float] = None
    total_duration_ms: Optional[float] = None
    
    def to_dict(self) -> Dict:
        return asdict(self)

@dataclass
class ExecutionTrace:
    """Complete execution trace for learning"""
    trace_id: str
    goal: str
    plan: List[ExecutionStep]
    outcome: ExecutionOutcome
    metadata: Optional[Dict] = None
    
    def to_dict(self) -> Dict:
        return {
            "trace_id": self.trace_id,
            "goal": self.goal,
            "plan": [step.to_dict() for step in self.plan],
            "outcome": self.outcome.to_dict(),
            "metadata": self.metadata or {}
        }
    
    @classmethod
    def from_dict(cls, data: Dict) -> 'ExecutionTrace':
        return cls(
            trace_id=data["trace_id"],
            goal=data["goal"],
            plan=[ExecutionStep(**step) for step in data["plan"]],
            outcome=ExecutionOutcome(**data["outcome"]),
            metadata=data.get("metadata")
        )

@dataclass
class PatternStep:
    """Step in a behavioral pattern"""
    step_type: str  # "action", "decision", "checkpoint", "parallel"
    action_template: str
    tool_preference: Optional[List[str]] = None
    conditions: Optional[Dict] = None
    fallbacks: Optional[List[Dict]] = None
    
    def to_dict(self) -> Dict:
        return asdict(self)

@dataclass
class PatternMetrics:
    """Metrics tracking pattern performance"""
    success_rate: float = 0.0
    average_cost: float = 0.0
    average_duration_ms: float = 0.0
    execution_count: int = 0
    last_used: Optional[str] = None
    adaptation_rate: float = 0.0
    
    def to_dict(self) -> Dict:
        return asdict(self)

@dataclass
class PatternConstraints:
    """Constraints for pattern application"""
    required_capabilities: Optional[List[str]] = None
    environment_requirements: Optional[List[str]] = None
    complexity_limit: Optional[str] = None  # "simple", "medium", "complex"
    
    def to_dict(self) -> Dict:
        return asdict(self)

@dataclass
class BehavioralPattern:
    """Reusable behavioral pattern"""
    pattern_id: str
    name: str
    description: str
    goal_pattern: str
    strategy: List[PatternStep]
    metrics: PatternMetrics
    constraints: Optional[PatternConstraints] = None
    metadata: Optional[Dict] = None
    
    def to_dict(self) -> Dict:
        return {
            "pattern_id": self.pattern_id,
            "name": self.name,
            "description": self.description,
            "goal_pattern": self.goal_pattern,
            "strategy": [step.to_dict() for step in self.strategy],
            "metrics": self.metrics.to_dict(),
            "constraints": self.constraints.to_dict() if self.constraints else {},
            "metadata": self.metadata or {}
        }
    
    @classmethod
    def from_dict(cls, data: Dict) -> 'BehavioralPattern':
        return cls(
            pattern_id=data["pattern_id"],
            name=data["name"],
            description=data["description"],
            goal_pattern=data["goal_pattern"],
            strategy=[PatternStep(**step) for step in data["strategy"]],
            metrics=PatternMetrics(**data["metrics"]),
            constraints=PatternConstraints(**data["constraints"]) if data.get("constraints") else None,
            metadata=data.get("metadata")
        )

# Pattern Storage Class
class PatternStorage:
    """Manages storage and retrieval of patterns using memory system"""
    
    def __init__(self):
        self.pattern_category = "decision"
        self.trace_category = "decision"
        self.pattern_prefix = "pattern:"
        self.trace_prefix = "trace:"
    
    def store_execution_trace(self, trace: ExecutionTrace) -> str:
        """Store an execution trace in memory"""
        trace_dict = trace.to_dict()
        trace_dict["metadata"] = trace_dict.get("metadata", {})
        trace_dict["metadata"]["stored_at"] = datetime.now().isoformat()
        trace_dict["metadata"]["type"] = "execution_trace"
        
        content = json.dumps(trace_dict, indent=2)
        memory_key = f"{self.trace_prefix}{trace.trace_id}"
        
        # Format for semantic search
        memory_content = f"{memory_key}\nGoal: {trace.goal}\n{content}"
        
        # Store in memory
        memory_id = memory_store(
            content=memory_content,
            category=self.trace_category
        )
        
        return memory_id
    
    def extract_pattern_from_trace(self, trace: ExecutionTrace, 
                                   pattern_name: Optional[str] = None,
                                   description: Optional[str] = None,
                                   tags: Optional[List[str]] = None) -> BehavioralPattern:
        """Extract a behavioral pattern from a successful execution trace"""
        
        # Create pattern steps from execution steps
        strategy = []
        for step in trace.plan:
            pattern_step = PatternStep(
                step_type="action",
                action_template=self._create_action_template(step),
                tool_preference=[step.tool] if step.tool else None,
                conditions={},
                fallbacks=[]
            )
            strategy.append(pattern_step)
        
        # Create pattern metrics from trace outcome
        metrics = PatternMetrics(
            success_rate=1.0 if trace.outcome.success else 0.0,
            average_cost=trace.outcome.cost_units or 0.0,
            average_duration_ms=trace.outcome.total_duration_ms or 0.0,
            execution_count=1,
            last_used=datetime.now().isoformat(),
            adaptation_rate=0.0
        )
        
        # Create pattern constraints
        constraints = PatternConstraints(
            required_capabilities=self._extract_required_capabilities(trace),
            environment_requirements=[],
            complexity_limit=self._assess_complexity(trace)
        )
        
        # Generate pattern ID and metadata
        pattern_id = f"pattern_{int(time.time())}_{uuid.uuid4().hex[:8]}"
        
        metadata = {
            "created_from": trace.trace_id,
            "created_at": datetime.now().isoformat(),
            "last_updated": datetime.now().isoformat(),
            "version": "1.0",
            "tags": tags or ["extracted"]
        }
        
        return BehavioralPattern(
            pattern_id=pattern_id,
            name=pattern_name or f"Pattern_{int(time.time())}",
            description=description or f"Extracted from trace {trace.trace_id}",
            goal_pattern=self._create_goal_pattern(trace.goal),
            strategy=strategy,
            metrics=metrics,
            constraints=constraints,
            metadata=metadata
        )
    
    def store_pattern(self, pattern: BehavioralPattern) -> str:
        """Store a behavioral pattern in memory"""
        pattern_dict = pattern.to_dict()
        pattern_dict["metadata"] = pattern_dict.get("metadata", {})
        pattern_dict["metadata"]["stored_at"] = datetime.now().isoformat()
        pattern_dict["metadata"]["type"] = "behavioral_pattern"
        
        content = json.dumps(pattern_dict, indent=2)
        memory_key = f"{self.pattern_prefix}{pattern.pattern_id}"
        
        # Format for semantic search
        memory_content = f"{memory_key}\nPattern: {pattern.name}\nGoal Pattern: {pattern.goal_pattern}\n{content}"
        
        # Store in memory
        memory_id = memory_store(
            content=memory_content,
            category=self.pattern_category
        )
        
        return memory_id
    
    def find_matching_patterns(self, goal: str, 
                               limit: int = 5,
                               min_similarity: float = 0.3) -> List[Tuple[BehavioralPattern, float]]:
        """Find patterns matching a given goal"""
        
        # Use semantic search to find relevant patterns
        query = f"pattern goal strategy for: {goal}"
        
        # Query memory for patterns
        memories = memory_query(
            query=query,
            category=self.pattern_category,
            limit=limit * 2  # Get extra for filtering
        )
        
        matching_patterns = []
        
        for memory in memories:
            try:
                pattern = self._extract_pattern_from_memory(memory.get("content", ""))
                if not pattern:
                    continue
                
                # Calculate similarity score
                similarity_score = self._calculate_goal_similarity(goal, pattern.goal_pattern)
                
                if similarity_score >= min_similarity:
                    matching_patterns.append((pattern, similarity_score))
                    
            except (json.JSONDecodeError, KeyError) as e:
                print(f"Error parsing pattern from memory: {e}")
                continue
        
        # Sort by similarity score
        matching_patterns.sort(key=lambda x: x[1], reverse=True)
        
        return matching_patterns[:limit]
    
    def update_pattern_metrics(self, pattern_id: str, 
                               success: bool,
                               cost_units: float = 0.0,
                               duration_ms: float = 0.0,
                               adaptations_made: int = 0) -> bool:
        """Update pattern metrics after execution"""
        
        # Find the pattern (in real implementation, would query memory)
        # For now, this is a mock implementation
        print(f"[MOCK] Updating metrics for pattern {pattern_id}")
        print(f"  Success: {success}, Cost: {cost_units}, Duration: {duration_ms}ms")
        
        return True
    
    # Helper methods
    def _extract_pattern_from_memory(self, content: str) -> Optional[BehavioralPattern]:
        """Extract pattern JSON from memory content"""
        try:
            # Find JSON in content (after first newline with {)
            lines = content.split('\n')
            json_start = -1
            for i, line in enumerate(lines):
                if line.strip().startswith('{'):
                    json_start = i
                    break
            
            if json_start == -1:
                return None
            
            json_content = '\n'.join(lines[json_start:])
            pattern_dict = json.loads(json_content)
            return BehavioralPattern.from_dict(pattern_dict)
            
        except (json.JSONDecodeError, KeyError) as e:
            print(f"Error extracting pattern: {e}")
            return None
    
    def _calculate_goal_similarity(self, goal1: str, goal2: str) -> float:
        """Calculate similarity between two goals using word overlap"""
        
        # Simple word overlap calculation
        words1 = set(re.findall(r'\b\w{3,}\b', goal1.lower()))
        words2 = set(re.findall(r'\b\w{3,}\b', goal2.lower()))
        
        if not words1 or not words2:
            return 0.0
        
        intersection = words1.intersection(words2)
        union = words1.union(words2)
        
        return len(intersection) / len(union)
    
    def _create_action_template(self, step: ExecutionStep) -> str:
        """Create a parameterized action template from execution step"""
        action = step.action.lower()
        
        if 'file' in action:
            return f"Perform {step.action} operation on {{target}}"
        elif 'memory' in action:
            return f"Store or query memory for {{purpose}}"
        else:
            return f"Execute {step.action} with appropriate parameters"
    
    def _create_goal_pattern(self, goal: str) -> str:
        """Create a generalized goal pattern from specific goal"""
        # Extract significant words
        words = re.findall(r'\b\w{4,}\b', goal.lower())
        common_words = {'create', 'build', 'make', 'generate', 'write', 'read', 'store', 'query'}
        
        # Filter out common words
        significant_words = [w for w in words if w not in common_words]
        
        if significant_words:
            # Create pattern with wildcards
            return f"*{'* *'.join(significant_words[:3])}*"
        else:
            # Fallback to truncated goal
            return goal[:50] + ('...' if len(goal) > 50 else '')
    
    def _extract_required_capabilities(self, trace: ExecutionTrace) -> List[str]:
        """Extract required capabilities from execution trace"""
        capabilities = set()
        
        for step in trace.plan:
            if step.tool:
                capabilities.add(step.tool)
        
        return list(capabilities)
    
    def _assess_complexity(self, trace: ExecutionTrace) -> str:
        """Assess complexity level of execution trace"""
        step_count = len(trace.plan)
        
        if step_count <= 3:
            return "simple"
        elif step_count <= 7:
            return "medium"
        else:
            return "complex"

# Pattern Matcher Class
class PatternMatcher:
    """Matches and selects patterns for given goals"""
    
    def __init__(self, storage: PatternStorage):
        self.storage = storage
    
    def select_pattern_for_goal(self, goal: str, 
                                context: Optional[Dict] = None) -> Dict:
        """Select the best pattern for a given goal"""
        
        context = context or {}
        
        # Find matching patterns
        matching_patterns = self.storage.find_matching_patterns(
            goal=goal,
            limit=10,
            min_similarity=0.2
        )
        
        if not matching_patterns:
            return {
                "selected": None,
                "reason": "No matching patterns found",
                "adaptation_required": True,
                "adaptation_type": "create_new"
            }
        
        # Score each pattern
        scored_patterns = []
        for pattern, similarity in matching_patterns:
            score = self._calculate_pattern_score(pattern, similarity, context)
            scored_patterns.append({
                "pattern": pattern,
                "similarity": similarity,
                "score": score,
                "confidence": self._calculate_confidence(pattern.metrics)
            })
        
        # Sort by score
        scored_patterns.sort(key=lambda x: x["score"], reverse=True)
        
        best_pattern = scored_patterns[0]
        adaptation_analysis = self._analyze_adaptation_requirements(
            goal, best_pattern["pattern"], context
        )
        
        # Prepare alternatives
        alternatives = []
        for i, p in enumerate(scored_patterns[1:4], 1):
            alternatives.append({
                "rank": i + 1,
                "pattern_id": p["pattern"].pattern_id,
                "name": p["pattern"].name,
                "score": p["score"]
            })
        
        return {
            "selected": best_pattern["pattern"],
            "selection_score": best_pattern["score"],
            "similarity_score": best_pattern["similarity"],
            "confidence": best_pattern["confidence"],
            "reason": self._generate_selection_reason(best_pattern),
            "adaptation_required": adaptation_analysis["required"],
            "adaptation_points": adaptation_analysis["points"],
            "alternative_patterns": alternatives
        }
    
    def _calculate_pattern_score(self, pattern: BehavioralPattern, 
                                 similarity: float,
                                 context: Dict) -> float:
        """Calculate overall score for pattern selection"""
        
        weights = {
            "similarity": 0.4,
            "success_rate": 0.3,
            "efficiency": 0.2,
            "freshness": 0.1
        }
        
        # Success rate score
        success_score = pattern.metrics.success_rate
        
        # Efficiency score
        max_cost = context.get("max_cost", 100)
        max_duration = context.get("max_duration", 60000)
        
        cost_score = max(0, 1 - (pattern.metrics.average_cost / max_cost))
        duration_score = max(0, 1 - (pattern.metrics.average_duration_ms / max_duration))
        efficiency_score = (cost_score + duration_score) / 2
        
        # Freshness score
        freshness_score = 1.0
        if pattern.metrics.last_used:
            last_used = datetime.fromisoformat(pattern.metrics.last_used)
            days_since = (datetime.now() - last_used).days
            freshness_score = max(0, 1 - (days_since / 30))
        
        # Calculate weighted score
        score = (
            similarity * weights["similarity"] +
            success_score * weights["success_rate"] +
            efficiency_score * weights["efficiency"] +
            freshness_score * weights["freshness"]
        )
        
        return score
    
    def _calculate_confidence(self, metrics: PatternMetrics) -> float:
        """Calculate confidence in pattern based on metrics"""
        execution_weight = min(metrics.execution_count / 10, 1.0)
        return (execution_weight * 0.4) + (metrics.success_rate * 0.6)
    
    def _analyze_adaptation_requirements(self, goal: str,
                                         pattern: BehavioralPattern,
                                         context: Dict) -> Dict:
        """Analyze what adaptations are needed for the pattern"""
        
        adaptation_points = []
        required = False
        
        # Check goal specificity
        if '*' in pattern.goal_pattern:
            pattern_keywords = pattern.goal_pattern.replace('*', '').lower()
            if pattern_keywords not in goal.lower():
                adaptation_points.append("Goal specificity mismatch")
                required = True
        
        # Check capability constraints
        if pattern.constraints and pattern.constraints.required_capabilities:
            available = context.get("capabilities", [])
            missing = [c for c in pattern.constraints.required_capabilities 
                      if c not in available]
            if missing:
                adaptation_points.append(f"Missing capabilities: {', '.join(missing)}")
                required = True
        
        return {
            "required": required,
            "points": adaptation_points
        }
    
    def _generate_selection_reason(self, pattern_info: Dict) -> str:
        """Generate human-readable reason for pattern selection"""
        
        reasons = []
        pattern = pattern_info["pattern"]
        
        if pattern_info["similarity"] >= 0.8:
            reasons.append("excellent goal match")
        elif pattern_info["similarity"] >= 0.6:
            reasons.append("good goal match")
        
        if pattern.metrics.success_rate >= 0.9:
            reasons.append("high success rate")
        elif pattern.metrics.success_rate >= 0.7:
            reasons.append("good success rate")
        
        if pattern.metrics.execution_count >= 5:
            reasons.append("well-tested pattern")
        
        if pattern_info["confidence"] >= 0.8:
            reasons.append("high confidence")
        
        if reasons:
            return f"Selected because of {', '.join(reasons)}"
        else:
            return "Selected as best available option"

# Example Usage
def example_usage():
    """Demonstrate the pattern storage and matching system"""
    
    print("=== Bootstrap Agent Pattern System Example ===\n")
    
    # Initialize storage and matcher
    storage = PatternStorage()
    matcher = PatternMatcher(storage)
    
    # Create an example execution trace
    trace = ExecutionTrace(
        trace_id=f"trace_{int(time.time())}",
        goal="Read configuration file and parse settings",
        plan=[
            ExecutionStep(
                action="file_read",
                tool="file_read",
                parameters={"path": "/config/settings.json"},
                result={"success": True, "content": "..."},
                timestamp=datetime.now().isoformat(),
                duration_ms=150.5
            ),
            ExecutionStep(
                action="parse_json",
                tool=None,
                parameters={"content": "..."},
                result={"success": True, "parsed": {...}},
                timestamp=datetime.now().isoformat(),
                duration_ms=50.2
            )
        ],
        outcome=ExecutionOutcome(
            success=True,
            cost_units=2.0,
            total_duration_ms=200.7
        ),
        metadata={
            "environment": "development",
            "agent_version": "1.0.0"
        }
    )
    
    print("1. Storing execution trace...")
    trace_id = storage.store_execution_trace(trace)
    print(f"   Trace stored with ID: {trace_id}\n")
    
    print("2. Extracting pattern from trace...")
    pattern = storage.extract_pattern_from_trace(
        trace,
        pattern_name="Config File Reading Pattern",
        description="Pattern for reading and parsing configuration files",
        tags=["file_operations", "parsing", "configuration"]
    )
    print(f"   Pattern extracted: {pattern.name}\n")
    
    print("3. Storing pattern...")
    pattern_id = storage.store_pattern(pattern)
    print(f"   Pattern stored with ID: {pattern_id}\n")
    
    print("4. Finding patterns for a similar goal...")
    test_goal = "Read settings file and extract configuration"
    matches = matcher.select_pattern_for_goal(test_goal)
    
    if matches["selected"]:
        print(f"   Selected pattern: {matches['selected'].name}")
        print(f"   Similarity score: {matches['similarity_score']:.2f}")
        print(f"   Confidence: {matches['confidence']:.2f}")
        print(f"   Adaptation required: {matches['adaptation_required']}")
        if matches['adaptation_required']:
            print(f"   Adaptation points: {matches['adaptation_points']}")
    else:
        print("   No suitable pattern found")
    
    print("\n5. Updating pattern metrics...")
    storage.update_pattern_metrics(
        pattern_id=pattern.pattern_id,
        success=True,
        cost_units=1.5,
        duration_ms=180.3,
        adaptations_made=0
    )
    
    print("\n=== Example Complete ===")

if __name__ == "__main__":
    example_usage()