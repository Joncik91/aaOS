use aaos_core::AgentId;

/// Priority level for agent scheduling.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord)]
pub enum Priority {
    Low = 0,
    #[default]
    Normal = 1,
    High = 2,
    Critical = 3,
}

/// A unit of work to be scheduled.
#[derive(Debug)]
pub struct ScheduleEntry {
    pub agent_id: AgentId,
    pub priority: Priority,
}

/// Trait for agent schedulers.
///
/// The scheduler determines which agent gets inference time next.
/// Initial implementation is round-robin with priority support.
pub trait Scheduler: Send + Sync {
    /// Add an agent to the schedule.
    fn enqueue(&self, entry: ScheduleEntry);

    /// Remove an agent from the schedule.
    fn dequeue(&self, agent_id: &AgentId);

    /// Get the next agent that should run.
    fn next(&self) -> Option<AgentId>;
}

/// Simple round-robin scheduler with priority support.
///
/// Higher-priority agents get more frequent turns.
pub struct RoundRobinScheduler {
    queue: std::sync::Mutex<Vec<ScheduleEntry>>,
}

impl RoundRobinScheduler {
    pub fn new() -> Self {
        Self {
            queue: std::sync::Mutex::new(Vec::new()),
        }
    }
}

impl Default for RoundRobinScheduler {
    fn default() -> Self {
        Self::new()
    }
}

impl Scheduler for RoundRobinScheduler {
    fn enqueue(&self, entry: ScheduleEntry) {
        let mut queue = self.queue.lock().unwrap();
        queue.push(entry);
        // Sort by priority (highest first)
        queue.sort_by(|a, b| b.priority.cmp(&a.priority));
    }

    fn dequeue(&self, agent_id: &AgentId) {
        let mut queue = self.queue.lock().unwrap();
        queue.retain(|e| e.agent_id != *agent_id);
    }

    fn next(&self) -> Option<AgentId> {
        let mut queue = self.queue.lock().unwrap();
        if queue.is_empty() {
            return None;
        }
        // Take the first (highest priority) and rotate to back
        let entry = queue.remove(0);
        let id = entry.agent_id;
        queue.push(entry);
        Some(id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_robin_basic() {
        let scheduler = RoundRobinScheduler::new();
        let a = AgentId::new();
        let b = AgentId::new();

        scheduler.enqueue(ScheduleEntry {
            agent_id: a,
            priority: Priority::Normal,
        });
        scheduler.enqueue(ScheduleEntry {
            agent_id: b,
            priority: Priority::Normal,
        });

        let first = scheduler.next().unwrap();
        let second = scheduler.next().unwrap();
        assert_ne!(first, second);

        // After full rotation, first should come back
        let third = scheduler.next().unwrap();
        assert_eq!(first, third);
    }

    #[test]
    fn priority_ordering() {
        let scheduler = RoundRobinScheduler::new();
        let low = AgentId::new();
        let high = AgentId::new();

        scheduler.enqueue(ScheduleEntry {
            agent_id: low,
            priority: Priority::Low,
        });
        scheduler.enqueue(ScheduleEntry {
            agent_id: high,
            priority: Priority::High,
        });

        assert_eq!(scheduler.next().unwrap(), high);
    }

    #[test]
    fn dequeue_removes() {
        let scheduler = RoundRobinScheduler::new();
        let a = AgentId::new();
        scheduler.enqueue(ScheduleEntry {
            agent_id: a,
            priority: Priority::Normal,
        });
        scheduler.dequeue(&a);
        assert!(scheduler.next().is_none());
    }
}
