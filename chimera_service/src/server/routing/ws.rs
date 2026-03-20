use std::sync::Arc;

use chimera_ipc::api::ws::events::Event;
use dashmap::DashMap;
// use futures_util::{SinkExt, StreamExt};
use tokio::sync::mpsc::Sender as MpscSender;

type SocketId = usize;

#[derive(Default, Clone)]
pub struct WsState {
    pub events_subscribers: Arc<DashMap<SocketId, MpscSender<Event>>>,
}

impl WsState {
    pub async fn event_broadcast(&self, event: Event) {
        futures_util::future::join_all(self.events_subscribers.iter().map(|entry| {
            let tx = entry.value().clone();
            let event = event.clone();
            async move {
                if let Err(e) = tx.send(event).await {
                    tracing::error!("Failed to send event: {:?}", e);
                }
            }
        }))
        .await;
    }
}
