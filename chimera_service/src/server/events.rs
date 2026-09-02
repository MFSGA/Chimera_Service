use std::sync::Arc;

use chimera_ipc::api::ws::events::Event;
use nyanpasu_core_manager::LogFrame;
use tokio::sync::broadcast;

const EVENT_CHANNEL_CAPACITY: usize = 256;
const LOG_EVENT_CHANNEL_CAPACITY: usize = 1024;

/// Fan-out point for WebSocket events. Clones share one broadcast channel.
#[derive(Clone)]
pub struct EventHub {
    tx: broadcast::Sender<Event>,
    log_tx: broadcast::Sender<Arc<LogFrame>>,
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
            log_tx: broadcast::channel(LOG_EVENT_CHANNEL_CAPACITY).0,
        }
    }

    /// Sending is synchronous and unaffected by slow subscribers.
    pub fn send(&self, event: Event) {
        let _ = self.tx.send(event);
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.tx.subscribe()
    }

    pub fn send_log(&self, frame: Arc<LogFrame>) {
        let _ = self.log_tx.send(frame);
    }

    pub fn subscribe_logs(&self) -> broadcast::Receiver<Arc<LogFrame>> {
        self.log_tx.subscribe()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chimera_ipc::api::{status::CoreState, ws::events::Event};
    use nyanpasu_core_manager::{CoreKind, LogLevel, LogStream};
    use tokio::sync::broadcast::error::{RecvError, TryRecvError};

    fn frame(epoch: u64) -> Arc<LogFrame> {
        Arc::new(LogFrame {
            at: 1_700_000_000_000,
            epoch,
            kind: CoreKind::Mihomo,
            stream: LogStream::Stdout,
            level: LogLevel::Info,
            timestamp: None,
            target: None,
            message: "hello".into(),
            fields: Vec::new(),
            raw: "hello".into(),
            truncated: false,
        })
    }

    #[test]
    fn status_and_log_rings_are_independent() {
        let hub = EventHub::new();
        let mut events = hub.subscribe();
        let mut logs = hub.subscribe_logs();
        hub.send(Event::new_core_state_changed(CoreState::Running));
        assert!(matches!(events.try_recv(), Ok(Event::CoreStateChanged(CoreState::Running))));
        assert!(matches!(logs.try_recv(), Err(TryRecvError::Empty)));

        let sent = frame(1);
        hub.send_log(Arc::clone(&sent));
        let received = logs.try_recv().unwrap();
        assert!(Arc::ptr_eq(&sent, &received));
        assert!(matches!(events.try_recv(), Err(TryRecvError::Empty)));
    }

    #[tokio::test]
    async fn a_log_flood_cannot_lag_the_status_ring() {
        let hub = EventHub::new();
        let mut events = hub.subscribe();
        let mut logs = hub.subscribe_logs();
        hub.send(Event::new_core_state_changed(CoreState::Running));
        for epoch in 0..(LOG_EVENT_CHANNEL_CAPACITY as u64 + 32) {
            hub.send_log(frame(epoch));
        }

        assert!(matches!(events.recv().await, Ok(Event::CoreStateChanged(CoreState::Running))));
        assert!(matches!(logs.recv().await, Err(RecvError::Lagged(_))));
    }
}
