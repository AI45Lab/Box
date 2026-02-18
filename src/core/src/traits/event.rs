//! Event bus abstraction.
//!
//! Decouples event emission and subscription from the Tokio broadcast
//! channel implementation in `event.rs`. Implementations can use any
//! pub/sub mechanism: in-process channels, NATS, Redis Pub/Sub, etc.
//!
//! The `BoxEvent` data type and event key constants remain in `event.rs`
//! as pure data — this trait only abstracts the transport.

use crate::event::BoxEvent;

/// Abstraction over event emission.
///
/// The runtime emits events at key lifecycle points (box ready, exec
/// completed, cache hit, etc.). Implementations decide how to deliver
/// these events to subscribers.
///
/// # Thread Safety
///
/// Implementations must be `Send + Sync + Clone`. The runtime clones
/// the bus and shares it across async tasks.
pub trait EventBus: Send + Sync + Clone {
    /// Emit an event to all subscribers.
    ///
    /// Fire-and-forget semantics — if no subscribers are listening,
    /// the event is silently dropped.
    fn emit(&self, event: BoxEvent);
}

/// Blanket impl: the existing `EventEmitter` already satisfies `EventBus`.
impl EventBus for crate::event::EventEmitter {
    fn emit(&self, event: BoxEvent) {
        self.emit(event);
    }
}
