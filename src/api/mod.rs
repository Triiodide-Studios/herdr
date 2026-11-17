pub mod client;
mod event_hub;
pub mod schema;
mod server;
mod status;
mod subscriptions;
mod wait;

pub use event_hub::EventHub;
pub use server::ServerHandle;
pub(crate) use server::{api_method_name, start_server_with_stop_control};
pub use status::{read_runtime_status_at, RuntimeStatus};

use std::path::PathBuf;

use tokio::sync::mpsc;

use crate::api::schema::{Method, Request};

pub const SOCKET_PATH_ENV_VAR: &str = "HERDR_SOCKET_PATH";

pub(crate) fn request_changes_ui(request: &Request) -> bool {
    matches!(
        &request.method,
        Method::ServerReloadConfig(_)
            | Method::ServerReloadAgentManifests(_)
            | Method::NotificationShow(_)
            | Method::ProductAnnouncementDismiss(_)
            | Method::ReleaseNotesDismiss(_)
            | Method::CommandInvoke(_)
            | Method::WorkspaceCreate(_)
            | Method::WorkspaceFocus(_)
            | Method::WorkspaceRename(_)
            | Method::WorkspaceMove(_)
            | Method::WorkspaceMoveBlock(_)
            | Method::WorkspaceReportMetadata(_)
            | Method::WorkspaceClose(_)
            | Method::WorktreeCreate(_)
            | Method::WorktreeOpen(_)
            | Method::WorktreeRemove(_)
            | Method::TabCreate(_)
            | Method::TabFocus(_)
            | Method::TabRename(_)
            | Method::TabMove(_)
            | Method::TabClose(_)
            | Method::LayoutApply(_)
            | Method::LayoutSetSplitRatio(_)
            | Method::AgentRename(_)
            | Method::AgentViewSet(_)
            | Method::AgentViewClear(_)
            | Method::AgentFocus(_)
            | Method::AgentStart(_)
            | Method::AgentPrompt(_)
            | Method::AgentSendKeys(_)
            | Method::PaneSplit(_)
            | Method::PaneSwap(_)
            | Method::PaneMove(_)
            | Method::PaneZoom(_)
            | Method::PaneFocusDirection(_)
            | Method::PaneResize(_)
            | Method::PaneScroll(_)
            | Method::PaneClear(_)
            | Method::PaneEditScrollback(_)
            | Method::PaneFocus(_)
            | Method::PaneInputSet(_)
            | Method::PaneRename(_)
            | Method::PaneReportAgent(_)
            | Method::PaneReportAgentSession(_)
            | Method::PaneReportMetadata(_)
            | Method::PaneClearAgentAuthority(_)
            | Method::PaneReleaseAgent(_)
            | Method::PaneClose(_)
            | Method::PopupClose(_)
            | Method::PluginUnlink(_)
            | Method::PluginDisable(_)
            | Method::PluginActionInvoke(_)
            | Method::PluginPaneOpen(_)
            | Method::PluginPaneFocus(_)
            | Method::PluginPaneClose(_)
    )
}

/// A request that only reads: it cannot change any pane's agent state, so the server need not compare every pane's state around it
/// for notifications (BET-9635: that comparison walked every pane, and for each one every tab, on the loop that serves keys, for each of
/// thousands of reads a second).
pub(crate) fn request_reads_only(request: &Request) -> bool {
    matches!(
        &request.method,
        Method::Ping(_)
            | Method::ServerAgentManifests(_)
            | Method::SessionSnapshot(_)
            | Method::WorkspaceList(_)
            | Method::WorkspaceGet(_)
            | Method::TabList(_)
            | Method::TabGet(_)
            | Method::AgentList(_)
            | Method::AgentGet(_)
            | Method::AgentRead(_)
            | Method::AgentExplain(_)
            | Method::PaneLayout(_)
            | Method::PaneProcessInfo(_)
            | Method::LayoutExport(_)
            | Method::PaneNeighbor(_)
            | Method::PaneEdges(_)
            | Method::PaneSelectionRead(_)
            | Method::PaneList(_)
            | Method::PaneCurrent(_)
            | Method::PaneGet(_)
            | Method::PaneRead(_)
            | Method::IntegrationList(_)
            | Method::PluginList(_)
            | Method::PluginActionList(_)
            | Method::PluginLogList(_)
    )
}

pub struct ApiRequestMessage {
    pub request: Request,
    pub respond_to: std::sync::mpsc::Sender<String>,
    pub response_write_complete: Option<std::sync::mpsc::Receiver<()>>,
}

pub type ApiRequestSender = mpsc::UnboundedSender<ApiRequestMessage>;

pub fn socket_path() -> PathBuf {
    crate::session::active_api_socket_path()
}

#[cfg(test)]
mod request_class_tests {
    use super::*;

    fn request(method: &str, params: serde_json::Value) -> Request {
        serde_json::from_value(serde_json::json!({"id": "t", "method": method, "params": params}))
            .unwrap()
    }

    /// The reads plugins and subscriptions make thousands of times a second skip the per-pane notification comparison; a request that
    /// can change the UI never does (BET-9635).
    #[test]
    fn polled_reads_skip_the_pane_comparison_and_writes_keep_it() {
        for (method, params) in [
            ("pane.get", serde_json::json!({"pane_id": "w1:p1"})),
            ("pane.read", serde_json::json!({"pane_id": "w1:p1", "source": "visible"})),
            ("agent.list", serde_json::json!({})),
            ("session.snapshot", serde_json::json!({})),
        ] {
            let read = request(method, params);
            assert!(request_reads_only(&read), "{method} should only read");
            assert!(!request_changes_ui(&read), "{method} is listed as changing the UI");
        }
        for (method, params) in [
            ("pane.focus", serde_json::json!({"pane_id": "w1:p1"})),
            ("tab.focus", serde_json::json!({"tab_id": "w1:t1"})),
        ] {
            let write = request(method, params);
            assert!(!request_reads_only(&write), "{method} changes state");
            assert!(request_changes_ui(&write), "{method}");
        }
    }
}
