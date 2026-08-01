use axum::{
    Router,
    extract::{
        State, WebSocketUpgrade,
        ws::{Message, WebSocket},
    },
    response::Response,
    routing::any,
};
use chimera_ipc::api::ws::events::{EVENT_URI, Event};
use futures_util::{SinkExt, StreamExt, stream::SplitSink};
use tokio::sync::broadcast::error::RecvError;

use super::AppState;
use crate::server::{
    CoreManager,
    events::{EventHub, WS_LAG_LOG_TARGET},
};

pub fn setup() -> Router<AppState> {
    Router::new().route(EVENT_URI, any(ws_handler))
}

/// One protocol, no negotiation: service and consumer ship together. Query
/// strings are ignored by routing and do not affect the upgrade.
async fn ws_handler(State(state): State<AppState>, ws: WebSocketUpgrade) -> Response {
    ws.on_upgrade(move |socket| handle_socket(socket, state.hub, state.core_manager))
}

async fn handle_socket(socket: WebSocket, hub: EventHub, core_manager: CoreManager) {
    let mut events = hub.subscribe();
    let (mut sink, mut stream) = socket.split();

    let handler = async { while let Some(Ok(_)) = stream.next().await {} };

    let sender = async {
        if !send_snapshot(&mut sink, &core_manager).await {
            return;
        }
        loop {
            match events.recv().await {
                Ok(event) => {
                    if !send_event(&mut sink, &event).await {
                        break;
                    }
                }
                Err(RecvError::Lagged(skipped)) => {
                    tracing::warn!(
                        target: WS_LAG_LOG_TARGET,
                        "ws subscriber dropped {skipped} events"
                    );
                    events = events.resubscribe();
                    if !send_snapshot(&mut sink, &core_manager).await {
                        break;
                    }
                }
                Err(RecvError::Closed) => break,
            }
        }
    };

    tokio::select! {
        _ = handler => (),
        _ = sender => (),
    }
}

async fn send_snapshot(
    sink: &mut SplitSink<WebSocket, Message>,
    core_manager: &CoreManager,
) -> bool {
    let event = Event::new_core_status_changed(core_manager.status().await);
    send_event(sink, &event).await
}

async fn send_event(sink: &mut SplitSink<WebSocket, Message>, event: &Event) -> bool {
    let Ok(payload) = simd_json::to_vec(event) else {
        tracing::error!("Failed to serialize event: {:?}", event);
        return true;
    };
    match sink.send(Message::binary(payload)).await {
        Ok(()) => true,
        Err(error) => {
            tracing::error!("Failed to send event: {:?}", error);
            false
        }
    }
}
