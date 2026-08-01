use chimera_ipc::api::ws::events::Event;
use tokio::sync::broadcast;

const EVENT_CHANNEL_CAPACITY: usize = 256;

/// Lag warnings must not be forwarded back into the same event ring.
pub(crate) const WS_LAG_LOG_TARGET: &str = "chimera_service::ws::lag";

pub(crate) fn should_forward_to_hub(target: &str) -> bool {
    target != WS_LAG_LOG_TARGET
}

/// Fan-out point for WebSocket events. Clones share one broadcast channel.
#[derive(Clone)]
pub struct EventHub {
    tx: broadcast::Sender<Event>,
}

impl Default for EventHub {
    fn default() -> Self {
        Self::new()
    }
}

impl EventHub {
    pub fn new() -> Self {
        Self {
            tx: broadcast::channel(EVENT_CHANNEL_CAPACITY).0,
        }
    }

    /// Sending is synchronous and unaffected by slow subscribers.
    pub fn send(&self, event: Event) {
        let _ = self.tx.send(event);
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.tx.subscribe()
    }
}
