use std::any::{Any, TypeId};
use std::collections::HashMap;

/// Marker trait for events that can be sent through the event bus.
pub trait Event: Send + Sync + 'static {}

type HandlerFn = Box<dyn Fn(&dyn Any) + Send + Sync>;

/// Type-safe event bus for decoupled communication between plugins.
pub struct EventBus {
    handlers: HashMap<TypeId, Vec<HandlerFn>>,
}

impl EventBus {
    pub fn new() -> Self {
        Self {
            handlers: HashMap::new(),
        }
    }

    /// Register a handler for events of type `E`.
    pub fn subscribe<E: Event, F>(&mut self, handler: F)
    where
        F: Fn(&E) + Send + Sync + 'static,
    {
        let type_id = TypeId::of::<E>();
        let wrapped: HandlerFn = Box::new(move |any| {
            if let Some(event) = any.downcast_ref::<E>() {
                handler(event);
            }
        });
        self.handlers.entry(type_id).or_default().push(wrapped);
    }

    /// Emit an event, notifying all registered handlers.
    pub fn emit<E: Event>(&self, event: &E) {
        let type_id = TypeId::of::<E>();
        if let Some(handlers) = self.handlers.get(&type_id) {
            for handler in handlers {
                handler(event);
            }
        }
    }
}

impl Default for EventBus {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};

    // Two independent test event types.
    struct PingEvent;
    impl Event for PingEvent {}

    struct PongEvent;
    impl Event for PongEvent {}

    #[test]
    fn subscribe_then_emit_invokes_handler() {
        let mut bus = EventBus::new();
        let count = Arc::new(AtomicU32::new(0));

        let count_clone = Arc::clone(&count);
        bus.subscribe::<PingEvent, _>(move |_| {
            count_clone.fetch_add(1, Ordering::SeqCst);
        });

        bus.emit(&PingEvent);
        assert_eq!(count.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn multiple_handlers_all_called() {
        let mut bus = EventBus::new();
        let a = Arc::new(AtomicU32::new(0));
        let b = Arc::new(AtomicU32::new(0));

        let a_clone = Arc::clone(&a);
        bus.subscribe::<PingEvent, _>(move |_| {
            a_clone.fetch_add(1, Ordering::SeqCst);
        });

        let b_clone = Arc::clone(&b);
        bus.subscribe::<PingEvent, _>(move |_| {
            b_clone.fetch_add(10, Ordering::SeqCst);
        });

        bus.emit(&PingEvent);
        assert_eq!(a.load(Ordering::SeqCst), 1);
        assert_eq!(b.load(Ordering::SeqCst), 10);
    }

    #[test]
    fn emit_with_no_subscribers_does_not_panic() {
        let bus = EventBus::new();
        // No subscribers registered — must not panic.
        bus.emit(&PingEvent);
    }

    #[test]
    fn different_event_types_do_not_cross_talk() {
        let mut bus = EventBus::new();
        let ping_count = Arc::new(AtomicU32::new(0));
        let pong_count = Arc::new(AtomicU32::new(0));

        let ping_clone = Arc::clone(&ping_count);
        bus.subscribe::<PingEvent, _>(move |_| {
            ping_clone.fetch_add(1, Ordering::SeqCst);
        });

        let pong_clone = Arc::clone(&pong_count);
        bus.subscribe::<PongEvent, _>(move |_| {
            pong_clone.fetch_add(1, Ordering::SeqCst);
        });

        bus.emit(&PingEvent);
        // Only PingEvent handler fires; PongEvent counter must remain zero.
        assert_eq!(ping_count.load(Ordering::SeqCst), 1);
        assert_eq!(pong_count.load(Ordering::SeqCst), 0);

        bus.emit(&PongEvent);
        // Now PongEvent handler fires; PingEvent counter stays at 1.
        assert_eq!(ping_count.load(Ordering::SeqCst), 1);
        assert_eq!(pong_count.load(Ordering::SeqCst), 1);
    }
}
