//! Task scheduling and assignment.
//!
//! The scheduler manages a queue of tasks and assigns them to available agents
//! based on priority and agent availability.

use crate::components::{AgentStatus, TaskAssignment};
use std::collections::{BinaryHeap, HashMap};
use std::cmp::Ordering;

/// Task to be assigned to an agent.
#[derive(Debug, Clone)]
pub struct Task {
    /// Unique task identifier
    pub task_id: String,

    /// Task description or prompt
    pub prompt: String,

    /// Task priority (higher = more important)
    pub priority: i32,

    /// Timestamp when task was created
    pub created_at: i64,
}

impl Task {
    /// Create a new task.
    pub fn new(task_id: String, prompt: String, priority: i32) -> Self {
        Self {
            task_id,
            prompt,
            priority,
            created_at: chrono::Utc::now().timestamp(),
        }
    }
}

// Implement ordering for priority queue
impl Ord for Task {
    fn cmp(&self, other: &Self) -> Ordering {
        self.priority.cmp(&other.priority)
            .then_with(|| other.created_at.cmp(&self.created_at))
    }
}

impl PartialOrd for Task {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Eq for Task {}

impl PartialEq for Task {
    fn eq(&self, other: &Self) -> bool {
        self.task_id == other.task_id
    }
}

/// Scheduler for managing task assignment to agents.
pub struct TaskScheduler {
    /// Priority queue of pending tasks
    task_queue: BinaryHeap<Task>,

    /// Map of agent IDs to their current task
    assignments: HashMap<String, String>,
}

impl TaskScheduler {
    /// Create a new task scheduler.
    pub fn new() -> Self {
        Self {
            task_queue: BinaryHeap::new(),
            assignments: HashMap::new(),
        }
    }

    /// Add a task to the queue.
    pub fn enqueue_task(&mut self, task: Task) {
        tracing::info!(task_id = %task.task_id, priority = task.priority, "Enqueueing task");
        self.task_queue.push(task);
    }

    /// Assign the next task to an agent.
    ///
    /// Returns the task assignment if a task is available.
    pub fn assign_task(&mut self, agent_id: String) -> Option<TaskAssignment> {
        let task = self.task_queue.pop()?;
        
        let assignment = TaskAssignment {
            task_id: task.task_id.clone(),
            prompt: task.prompt,
            priority: task.priority,
            assigned_at: chrono::Utc::now().timestamp(),
        };

        self.assignments.insert(agent_id.clone(), task.task_id);
        tracing::info!(agent_id = %agent_id, task_id = %assignment.task_id, "Assigned task");

        Some(assignment)
    }

    /// Mark a task as complete and free the agent.
    pub fn complete_task(&mut self, agent_id: &str) {
        if let Some(task_id) = self.assignments.remove(agent_id) {
            tracing::info!(agent_id = %agent_id, task_id = %task_id, "Task completed");
        }
    }

    /// Get the number of pending tasks.
    pub fn pending_count(&self) -> usize {
        self.task_queue.len()
    }

    /// Get the number of active assignments.
    pub fn active_count(&self) -> usize {
        self.assignments.len()
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
    fn test_scheduler_creation() {
        let scheduler = TaskScheduler::new();
        assert_eq!(scheduler.pending_count(), 0);
        assert_eq!(scheduler.active_count(), 0);
    }

    #[test]
    fn test_task_queue() {
        let mut scheduler = TaskScheduler::new();
        
        scheduler.enqueue_task(Task::new("task1".to_string(), "prompt1".to_string(), 1));
        scheduler.enqueue_task(Task::new("task2".to_string(), "prompt2".to_string(), 5));
        scheduler.enqueue_task(Task::new("task3".to_string(), "prompt3".to_string(), 3));
        
        assert_eq!(scheduler.pending_count(), 3);

        // Should assign highest priority task first
        let assignment = scheduler.assign_task("agent1".to_string()).unwrap();
        assert_eq!(assignment.task_id, "task2");
        assert_eq!(scheduler.active_count(), 1);
    }

    #[test]
    fn test_task_completion() {
        let mut scheduler = TaskScheduler::new();
        scheduler.enqueue_task(Task::new("task1".to_string(), "prompt".to_string(), 1));
        
        scheduler.assign_task("agent1".to_string());
        assert_eq!(scheduler.active_count(), 1);
        
        scheduler.complete_task("agent1");
        assert_eq!(scheduler.active_count(), 0);
    }
}
