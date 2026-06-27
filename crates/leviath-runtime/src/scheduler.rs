//! Task scheduler for managing agent execution priorities.

use std::collections::VecDeque;

/// Task scheduler for prioritizing agent work.
#[derive(Debug)]
pub struct TaskScheduler {
    /// Queue of pending tasks
    queue: VecDeque<Task>,
}

/// A task to be executed by an agent.
#[derive(Debug, Clone)]
pub struct Task {
    /// Unique task identifier
    pub id: String,

    /// Task description/prompt
    pub prompt: String,

    /// Priority (higher = more important)
    pub priority: i32,
}

impl TaskScheduler {
    /// Create a new task scheduler.
    pub fn new() -> Self {
        Self {
            queue: VecDeque::new(),
        }
    }

    /// Add a task to the scheduler.
    pub fn schedule(&mut self, task: Task) {
        // Insert task in priority order
        let pos = self
            .queue
            .iter()
            .position(|t| t.priority < task.priority)
            .unwrap_or(self.queue.len());
        self.queue.insert(pos, task);
    }

    /// Get the next task to execute.
    pub fn next_task(&mut self) -> Option<Task> {
        self.queue.pop_front()
    }

    /// Get the number of pending tasks.
    pub fn pending_count(&self) -> usize {
        self.queue.len()
    }
}

impl Default for TaskScheduler {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_priority_ordering() {
        let mut scheduler = TaskScheduler::new();

        scheduler.schedule(Task {
            id: "1".to_string(),
            prompt: "Low priority".to_string(),
            priority: 1,
        });

        scheduler.schedule(Task {
            id: "2".to_string(),
            prompt: "High priority".to_string(),
            priority: 10,
        });

        let next = scheduler.next_task().unwrap();
        assert_eq!(next.id, "2"); // High priority first
    }
}
