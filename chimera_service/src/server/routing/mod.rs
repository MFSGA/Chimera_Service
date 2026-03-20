use super::CoreManager;
use ws::WsState;

pub mod ws;

#[derive(Clone)]
pub struct AppState {
    pub core_manager: CoreManager,
    pub ws_state: WsState,
}
