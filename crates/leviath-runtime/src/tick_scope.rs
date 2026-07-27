//! Which agent the pipeline is currently working on, so a panic inside a
//! schedule system can be blamed on the run that caused it (issue #109).
//!
//! Every agent lives in one shared [`World`](bevy_ecs::world::World) driven by
//! one [`Schedule`](bevy_ecs::schedule::Schedule), so a caught panic carries no
//! hint of *whose* data tripped it. Without attribution the daemon can only log
//! "something panicked" and re-tick the same unchanged state on the next wake —
//! panicking again, forever, while every other agent stalls behind it.
//!
//! Each per-agent loop in the pipeline therefore calls [`enter`] before touching
//! an entity and [`clear`] once the loop is done. If a system panics mid-loop,
//! the slot still holds the offending entity when
//! [`PipelineWorld::tick`](crate::world::PipelineWorld::tick) catches the
//! unwind, which then fails that one agent and lets the world carry on.
//!
//! ## Why a thread-local, and why no RAII guard
//!
//! Thread-local rather than a global: parallel tests each drive their own world
//! on their own thread and would otherwise trample a shared slot. This is sound
//! because the schedule runs single-threaded (see
//! [`PipelineWorld::new`](crate::world::PipelineWorld::new)) — systems run on
//! the same thread that catches the panic.
//!
//! And plainly set/cleared rather than a `Drop` guard: a guard would clear the
//! slot *while unwinding*, destroying the very evidence the catch needs.

use std::cell::Cell;

use bevy_ecs::entity::Entity;

thread_local! {
    /// The agent this thread's schedule is currently processing, if any.
    static CURRENT: Cell<Option<Entity>> = const { Cell::new(None) };
}

/// Record `entity` as the agent being processed right now.
pub fn enter(entity: Entity) {
    CURRENT.with(|c| c.set(Some(entity)));
}

/// Forget the current agent — call this when a per-agent loop finishes, so a
/// later panic in agent-independent code isn't blamed on the last agent seen.
pub fn clear() {
    CURRENT.with(|c| c.set(None));
}

/// The agent recorded by the last [`enter`] that has not been [`clear`]ed.
pub fn current() -> Option<Entity> {
    CURRENT.with(Cell::get)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enter_records_and_clear_forgets() {
        clear();
        assert_eq!(current(), None);
        let e = Entity::from_raw(7);
        enter(e);
        assert_eq!(current(), Some(e));
        // A second enter replaces rather than nests.
        let f = Entity::from_raw(9);
        enter(f);
        assert_eq!(current(), Some(f));
        clear();
        assert_eq!(current(), None);
    }

    #[test]
    fn a_panic_leaves_the_entity_recorded_for_the_catcher() {
        // The whole point: the slot must survive unwinding, which is why there
        // is no `Drop` guard.
        clear();
        let e = Entity::from_raw(3);
        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        let caught = std::panic::catch_unwind(|| {
            enter(e);
            panic!("mid-loop");
        });
        std::panic::set_hook(prev);
        assert!(caught.is_err());
        assert_eq!(current(), Some(e));
        clear();
    }
}
