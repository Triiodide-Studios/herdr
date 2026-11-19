use std::path::PathBuf;

use crate::api::schema::{
    EventData, EventEnvelope, EventKind, ResponseResult, TabCreateParams, TabListParams,
    TabMoveParams, TabRenameParams, TabTarget,
};
use crate::app::{App, Mode};

use super::responses::{encode_error, encode_success};

impl App {
    pub(super) fn handle_tab_list(&mut self, id: String, params: TabListParams) -> String {
        let tabs = if let Some(workspace_id) = params.workspace_id {
            let Some(ws_idx) = self.parse_workspace_id(&workspace_id) else {
                return workspace_not_found(id, &workspace_id);
            };
            let Some(_) = self.state.workspaces.get(ws_idx) else {
                return workspace_not_found(id, &workspace_id);
            };
            self.tab_list_info(ws_idx)
        } else {
            let mut tabs = Vec::new();
            for (ws_idx, ws) in self.state.workspaces.iter().enumerate() {
                for tab_idx in 0..ws.tabs.len() {
                    if let Some(tab) = self.tab_info(ws_idx, tab_idx) {
                        tabs.push(tab);
                    }
                }
            }
            tabs
        };

        encode_success(id, ResponseResult::TabList { tabs })
    }

    pub(super) fn handle_tab_get(&mut self, id: String, target: TabTarget) -> String {
        let Some((ws_idx, tab_idx)) = self.parse_tab_id(&target.tab_id) else {
            return tab_not_found(id, &target.tab_id);
        };
        let Some(tab) = self.tab_info(ws_idx, tab_idx) else {
            return tab_not_found(id, &target.tab_id);
        };

        encode_success(id, ResponseResult::TabInfo { tab })
    }

    pub(super) fn handle_tab_create(&mut self, id: String, params: TabCreateParams) -> String {
        let TabCreateParams {
            workspace_id,
            cwd,
            focus,
            label,
            env,
        } = params;
        let ws_idx = if let Some(workspace_id) = workspace_id {
            let Some(ws_idx) = self.parse_workspace_id(&workspace_id) else {
                return workspace_not_found(id, &workspace_id);
            };
            ws_idx
        } else if let Some(active) = self.state.active {
            active
        } else {
            return encode_error(id, "workspace_not_found", "no active workspace");
        };
        let cwd = cwd.map(PathBuf::from).unwrap_or_else(|| {
            self.resolve_new_terminal_cwd(self.focused_pane_cwd_in_workspace(ws_idx))
        });
        let (rows, cols) = self.state.new_pane_size(crate::ui::NewPanePlacement::Alone);
        let default_shell = self.state.default_shell.clone();
        let scrollback_limit_bytes = self.state.pane_scrollback_limit_bytes;
        let host_terminal_theme = self.state.host_terminal_theme;
        let host_terminal_appearance = self.pane_host_terminal_appearance();
        let extra_env = match super::env::normalize_launch_env(env) {
            Ok(env) => env,
            Err((code, message)) => return encode_error(id, &code, message),
        };
        let result = self
            .state
            .workspaces
            .get_mut(ws_idx)
            .ok_or_else(|| std::io::Error::other("workspace disappeared"))
            .and_then(|ws| {
                ws.create_tab(
                    rows,
                    cols,
                    cwd,
                    scrollback_limit_bytes,
                    host_terminal_theme,
                    host_terminal_appearance,
                    crate::pane::PaneShellConfig::new(&default_shell, self.state.shell_mode),
                    extra_env,
                )
            });
        match result {
            Ok((tab_idx, terminal, runtime)) => {
                self.finish_created_tab(id, ws_idx, tab_idx, terminal, runtime, label, focus)
            }
            Err(err) => encode_error(id, "tab_create_failed", err.to_string()),
        }
    }

    /// The rest of `tab.create` once the tab exists: its runtime and terminal are registered, the label and the focus applied, the session
    /// saved and the events emitted. Answers with the new tab. The synchronous `tab.create` and the one that opens its pane off the app loop
    /// both end here.
    #[allow(clippy::too_many_arguments)]
    fn finish_created_tab(
        &mut self,
        id: String,
        ws_idx: usize,
        tab_idx: usize,
        terminal: crate::terminal::TerminalState,
        runtime: crate::terminal::TerminalRuntime,
        label: Option<String>,
        focus: bool,
    ) -> String {
        self.terminal_runtimes.insert(terminal.id.clone(), runtime);
        self.state.terminals.insert(terminal.id.clone(), terminal);
        self.state.remove_alias_shadowed_by_new_pane(
            self.state.workspaces[ws_idx].tabs[tab_idx].root_pane,
        );
        if let Some(label) = label {
            let workspace_id = self.state.workspaces[ws_idx].id.clone();
            let tab_id = self.public_tab_id(ws_idx, tab_idx).unwrap_or_else(|| {
                crate::workspace::public_tab_id_for_number(&workspace_id, tab_idx + 1)
            });
            if let Some(tab) = self
                .state
                .workspaces
                .get_mut(ws_idx)
                .and_then(|ws| ws.tabs.get_mut(tab_idx))
            {
                tab.set_custom_name(label);
                crate::logging::tab_renamed(&workspace_id, &tab_id);
            }
        }
        if focus {
            self.state.switch_workspace_tab(ws_idx, tab_idx);
            self.state.mode = Mode::Terminal;
        }
        self.schedule_session_save();
        self.emit_tab_created_events(ws_idx, tab_idx);
        encode_success(
            id,
            self.tab_created_result(ws_idx, tab_idx)
                .expect("new tab should produce a complete create response"),
        )
    }

    /// `tab.create` for a request that does not take focus, with the first pane opened on a blocking thread (BET-9635). On 11/16 opening a
    /// ConPTY and starting the child held the app loop 76 ms on an idle server and 1.4 s to 3.6 s while the machine was loaded; every key
    /// and every API request waited behind it. The numbers and the pane's id are taken here; the loop returns at once; the
    /// `AppEvent::TabSpawned` that follows is finished by `finish_deferred_tab_create`, which answers through `respond_to`.
    /// Needs a tokio runtime (it uses `spawn_blocking`). A request that takes focus keeps the synchronous `handle_tab_create`.
    pub(crate) fn start_deferred_tab_create(
        &mut self,
        id: String,
        params: TabCreateParams,
        respond_to: std::sync::mpsc::Sender<String>,
    ) {
        let TabCreateParams {
            workspace_id,
            cwd,
            focus: _,
            label,
            env,
        } = params;
        let ws_idx = if let Some(workspace_id) = workspace_id {
            let Some(ws_idx) = self.parse_workspace_id(&workspace_id) else {
                let _ = respond_to.send(workspace_not_found(id, &workspace_id));
                return;
            };
            ws_idx
        } else if let Some(active) = self.state.active {
            active
        } else {
            let _ = respond_to.send(encode_error(
                id,
                "workspace_not_found",
                "no active workspace",
            ));
            return;
        };
        let cwd = cwd.map(PathBuf::from).unwrap_or_else(|| {
            self.resolve_new_terminal_cwd(self.focused_pane_cwd_in_workspace(ws_idx))
        });
        let (rows, cols) = self.state.new_pane_size(crate::ui::NewPanePlacement::Alone);
        let default_shell = self.state.default_shell.clone();
        let shell_mode = self.state.shell_mode;
        let scrollback_limit_bytes = self.state.pane_scrollback_limit_bytes;
        let host_terminal_theme = self.state.host_terminal_theme;
        let host_terminal_appearance = self.state.host_terminal_appearance;
        let extra_env = match super::env::normalize_launch_env(env) {
            Ok(env) => env,
            Err((code, message)) => {
                let _ = respond_to.send(encode_error(id, &code, message));
                return;
            }
        };
        let Some(workspace) = self.state.workspaces.get_mut(ws_idx) else {
            let _ = respond_to.send(encode_error(
                id,
                "tab_create_failed",
                "workspace disappeared",
            ));
            return;
        };
        let workspace_id = workspace.id.clone();
        let (reservation, launch_env) = workspace.reserve_tab(extra_env);
        let root_id = reservation.root_id;
        let events = reservation.events.clone();
        let render_notify = reservation.render_notify.clone();
        let render_dirty = reservation.render_dirty.clone();

        let spawn_id = self.next_tab_spawn_id;
        self.next_tab_spawn_id += 1;
        self.pending_tab_spawns.insert(
            spawn_id,
            crate::app::PendingTabSpawn {
                request_id: id,
                workspace_id,
                reservation,
                cwd: cwd.clone(),
                label,
                respond_to,
                died: None,
            },
        );
        let result_tx = events.clone();
        tokio::task::spawn_blocking(move || {
            let result = crate::terminal::TerminalRuntime::spawn(
                root_id,
                rows,
                cols,
                cwd,
                scrollback_limit_bytes,
                host_terminal_theme,
                host_terminal_appearance,
                crate::pane::PaneShellConfig::new(&default_shell, shell_mode),
                &launch_env,
                events,
                render_notify,
                render_dirty,
            );
            let _ = result_tx.blocking_send(crate::events::AppEvent::TabSpawned(Box::new(
                crate::events::TabSpawnResult { spawn_id, result },
            )));
        });
    }

    /// The end of `start_deferred_tab_create`, on the app loop: adds the tab whose first pane is open, or answers that it could not be, and
    /// replays the `PaneDied` of a shell that exited before the tab was here.
    pub(crate) fn finish_deferred_tab_create(&mut self, spawned: crate::events::TabSpawnResult) {
        let Some(pending) = self.pending_tab_spawns.remove(&spawned.spawn_id) else {
            if let Ok(runtime) = spawned.result {
                runtime.shutdown_in_background();
            }
            return;
        };
        let crate::app::PendingTabSpawn {
            request_id,
            workspace_id,
            reservation,
            cwd,
            label,
            respond_to,
            died,
        } = pending;
        let root_id = reservation.root_id;
        let response = match spawned.result {
            Err(err) => encode_error(request_id, "tab_create_failed", err.to_string()),
            Ok(runtime) => {
                match self
                    .state
                    .workspaces
                    .iter()
                    .position(|workspace| workspace.id == workspace_id)
                {
                    None => {
                        runtime.shutdown_in_background();
                        workspace_not_found(request_id, &workspace_id)
                    }
                    Some(ws_idx) => {
                        let (tab_idx, terminal, runtime) = self.state.workspaces[ws_idx]
                            .add_reserved_tab(reservation, cwd, runtime);
                        let response = self.finish_created_tab(
                            request_id, ws_idx, tab_idx, terminal, runtime, label, false,
                        );
                        if let Some(exit_reason) = died {
                            self.handle_internal_event(crate::events::AppEvent::PaneDied {
                                pane_id: root_id,
                                exit_reason,
                            });
                        }
                        response
                    }
                }
            }
        };
        let _ = respond_to.send(response);
    }

    pub(super) fn handle_tab_focus(&mut self, id: String, target: TabTarget) -> String {
        let Some((ws_idx, tab_idx)) = self.parse_tab_id(&target.tab_id) else {
            return tab_not_found(id, &target.tab_id);
        };
        self.state.switch_workspace_tab(ws_idx, tab_idx);
        let tab = self.tab_info(ws_idx, tab_idx).unwrap();

        encode_success(id, ResponseResult::TabInfo { tab })
    }

    pub(super) fn handle_tab_rename(&mut self, id: String, params: TabRenameParams) -> String {
        let Some((ws_idx, tab_idx)) = self.parse_tab_id(&params.tab_id) else {
            return tab_not_found(id, &params.tab_id);
        };
        let workspace_id = self.state.workspaces[ws_idx].id.clone();
        let tab_id = self.public_tab_id(ws_idx, tab_idx).unwrap_or_else(|| {
            crate::workspace::public_tab_id_for_number(&workspace_id, tab_idx + 1)
        });
        let Some(tab) = self
            .state
            .workspaces
            .get_mut(ws_idx)
            .and_then(|ws| ws.tabs.get_mut(tab_idx))
        else {
            return tab_not_found(id, &params.tab_id);
        };
        tab.set_custom_name(params.label.clone());
        crate::logging::tab_renamed(&workspace_id, &tab_id);
        self.schedule_session_save();
        self.emit_event(EventEnvelope {
            event: EventKind::TabRenamed,
            data: EventData::TabRenamed {
                tab_id: self.public_tab_id(ws_idx, tab_idx).unwrap(),
                workspace_id: self.public_workspace_id(ws_idx),
                label: params.label,
            },
        });
        let tab = self.tab_info(ws_idx, tab_idx).unwrap();

        encode_success(id, ResponseResult::TabInfo { tab })
    }

    pub(super) fn handle_tab_move(&mut self, id: String, params: TabMoveParams) -> String {
        let Some((ws_idx, tab_idx)) = self.parse_tab_id(&params.tab_id) else {
            return tab_not_found(id, &params.tab_id);
        };
        let Some(ws) = self.state.workspaces.get(ws_idx) else {
            return tab_not_found(id, &params.tab_id);
        };
        if params.insert_index > ws.tabs.len() {
            return encode_error(
                id,
                "tab_move_failed",
                format!("insert_index {} is out of bounds", params.insert_index),
            );
        }

        let tab_id = self
            .public_tab_id(ws_idx, tab_idx)
            .unwrap_or_else(|| crate::workspace::public_tab_id_for_number(&ws.id, tab_idx + 1));
        let workspace_id = self.public_workspace_id(ws_idx);
        let insert_index = params.insert_index;
        let moved = self
            .state
            .workspaces
            .get_mut(ws_idx)
            .is_some_and(|ws| ws.move_tab(tab_idx, insert_index));
        let tabs = self.tab_list_info(ws_idx);
        if moved {
            self.schedule_session_save();
            self.emit_event(EventEnvelope {
                event: EventKind::TabMoved,
                data: EventData::TabMoved {
                    tab_id,
                    workspace_id,
                    insert_index,
                    tabs: tabs.clone(),
                },
            });
        }

        encode_success(id, ResponseResult::TabList { tabs })
    }

    pub(super) fn handle_tab_close(&mut self, id: String, target: TabTarget) -> String {
        let Some((ws_idx, tab_idx)) = self.parse_tab_id(&target.tab_id) else {
            return tab_not_found(id, &target.tab_id);
        };
        let Some(tab_id) = self.public_tab_id(ws_idx, tab_idx) else {
            return tab_not_found(id, &target.tab_id);
        };
        let workspace_id = self.public_workspace_id(ws_idx);
        let Some(ws) = self.state.workspaces.get(ws_idx) else {
            return tab_not_found(id, &target.tab_id);
        };
        let closes_workspace = ws.tabs.len() <= 1;
        let terminal_ids = self.state.terminal_ids_for_tab(ws_idx, tab_idx);
        let pane_ids = ws
            .tabs
            .get(tab_idx)
            .map(|tab| tab.layout.pane_ids())
            .unwrap_or_default();

        if closes_workspace {
            if let Err(response) = self.require_restored_group_close_ready(
                &id,
                &self.state.workspace_close_indices(ws_idx),
            ) {
                return response;
            }
            if self.state.confirm_implicit_worktree_group_close(ws_idx) {
                return encode_error(
                    id,
                    "confirmation_required",
                    "closing this tab would close a worktree group",
                );
            }
            let workspace = self.workspace_info(ws_idx);
            self.state.selected = ws_idx;
            self.state.close_selected_workspace();
            self.state.remove_plugin_pane_records(pane_ids);
            self.shutdown_detached_terminal_runtimes_in_background();
            self.emit_event(EventEnvelope {
                event: EventKind::TabClosed,
                data: EventData::TabClosed {
                    tab_id,
                    workspace_id: workspace_id.clone(),
                },
            });
            self.emit_event(EventEnvelope {
                event: EventKind::WorkspaceClosed,
                data: EventData::WorkspaceClosed {
                    workspace_id,
                    workspace: Some(workspace),
                },
            });
            return encode_success(id, ResponseResult::Ok {});
        }

        let Some(ws) = self.state.workspaces.get_mut(ws_idx) else {
            return tab_not_found(id, &target.tab_id);
        };
        if !ws.close_tab(tab_idx) {
            return encode_error(
                id,
                "tab_close_failed",
                format!("tab {} could not be closed", target.tab_id),
            );
        }
        self.state.remove_plugin_pane_records(pane_ids);
        self.state.remove_unattached_terminal_ids(terminal_ids);
        self.shutdown_detached_terminal_runtimes_in_background();
        self.schedule_session_save();
        self.emit_event(EventEnvelope {
            event: EventKind::TabClosed,
            data: EventData::TabClosed {
                tab_id,
                workspace_id,
            },
        });

        encode_success(id, ResponseResult::Ok {})
    }

    fn tab_list_info(&self, ws_idx: usize) -> Vec<crate::api::schema::TabInfo> {
        self.state
            .workspaces
            .get(ws_idx)
            .map(|ws| {
                (0..ws.tabs.len())
                    .filter_map(|idx| self.tab_info(ws_idx, idx))
                    .collect()
            })
            .unwrap_or_default()
    }
}

fn workspace_not_found(id: String, workspace_id: &str) -> String {
    encode_error(
        id,
        "workspace_not_found",
        format!("workspace {workspace_id} not found"),
    )
}

fn tab_not_found(id: String, tab_id: &str) -> String {
    encode_error(id, "tab_not_found", format!("tab {tab_id} not found"))
}

#[cfg(test)]
mod tests {
    use super::super::test_support::{exiting_test_command, shutdown_test_runtimes};
    use super::*;
    use crate::{
        api::schema::SuccessResponse,
        config::{Config, ShellModeConfig},
        workspace::Workspace,
    };

    #[test]
    fn api_tab_close_last_tab_closes_workspace_and_emits_both_events() {
        let event_hub = crate::api::EventHub::default();
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &Config::default(),
            crate::app::AppPolicy::TEST,
            None,
            api_rx,
            event_hub.clone(),
        );
        app.state.workspaces = vec![Workspace::test_new("tabs")];
        app.state.active = Some(0);
        app.state.selected = 0;
        let tab_id = app.public_tab_id(0, 0).unwrap();
        let workspace_id = app.public_workspace_id(0);

        let response = app.handle_tab_close(
            "req".into(),
            TabTarget {
                tab_id: tab_id.clone(),
            },
        );

        let success: SuccessResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(success.result, ResponseResult::Ok {});
        assert!(app.state.workspaces.is_empty());
        assert!(app.state.active.is_none());
        let events = event_hub.events_after(0);
        assert_eq!(
            events
                .iter()
                .map(|(_, event)| event.event)
                .collect::<Vec<_>>(),
            [EventKind::TabClosed, EventKind::WorkspaceClosed]
        );
        assert!(matches!(
            &events[0].1.data,
            EventData::TabClosed {
                tab_id: closed_tab_id,
                workspace_id: closed_workspace_id,
            } if closed_tab_id == &tab_id && closed_workspace_id == &workspace_id
        ));
        assert!(matches!(
            &events[1].1.data,
            EventData::WorkspaceClosed {
                workspace_id: closed_workspace_id,
                workspace: Some(workspace),
            } if closed_workspace_id == &workspace_id
                && workspace.workspace_id == workspace_id
        ));
    }

    #[test]
    fn api_tab_move_reorders_tabs_in_target_workspace() {
        let event_hub = crate::api::EventHub::default();
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &Config::default(),
            crate::app::AppPolicy::TEST,
            None,
            api_rx,
            event_hub.clone(),
        );
        let mut workspace = Workspace::test_new("tabs");
        workspace.test_add_tab(Some("two"));
        workspace.test_add_tab(Some("three"));
        app.state.workspaces = vec![workspace];
        app.state.active = Some(0);
        app.state.selected = 0;
        let moved_root = app.state.workspaces[0].tabs[0].root_pane;
        let moved_id = app.public_tab_id(0, 0).unwrap();

        let response = app.handle_tab_move(
            "req".into(),
            TabMoveParams {
                tab_id: moved_id.clone(),
                insert_index: 3,
            },
        );

        let success: SuccessResponse = serde_json::from_str(&response).unwrap();
        let ResponseResult::TabList { tabs } = success.result else {
            panic!("expected tab list");
        };
        assert_eq!(app.state.workspaces[0].tabs[2].root_pane, moved_root);
        assert_eq!(tabs[2].tab_id, app.public_tab_id(0, 2).unwrap());
        let events = event_hub.events_after(0);
        assert!(events.iter().any(|(_, event)| {
            matches!(
                &event.data,
                EventData::TabMoved {
                    tab_id,
                    workspace_id,
                    insert_index: 3,
                    tabs,
                } if tab_id == &moved_id
                    && workspace_id == &app.public_workspace_id(0)
                    && tabs[2].tab_id == moved_id
            )
        }));
    }

    #[tokio::test]
    async fn tab_create_follows_cached_focused_pane_cwd_without_runtime() {
        let event_hub = crate::api::EventHub::default();
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &Config::default(),
            crate::app::AppPolicy::TEST,
            None,
            api_rx,
            event_hub,
        );
        app.state.default_shell = exiting_test_command().into();
        app.state.shell_mode = ShellModeConfig::NonLogin;
        let workspace = Workspace::test_new("tabs");
        let focused_pane = workspace.tabs[0].root_pane;
        app.state.workspaces = vec![workspace];
        app.state.active = Some(0);
        app.state.selected = 0;
        app.state.ensure_test_terminals();
        let cached_cwd = std::env::temp_dir();
        let terminal_id = app.state.workspaces[0]
            .terminal_id(focused_pane)
            .cloned()
            .unwrap();
        app.state.terminals.get_mut(&terminal_id).unwrap().cwd = cached_cwd.clone();

        let response = app.handle_tab_create(
            "req".into(),
            TabCreateParams {
                workspace_id: None,
                cwd: None,
                focus: false,
                label: None,
                env: Default::default(),
            },
        );

        let success: SuccessResponse = serde_json::from_str(&response).unwrap();
        assert!(matches!(success.result, ResponseResult::TabCreated { .. }));
        let created = &app.state.workspaces[0].tabs[1];
        let created_terminal_id = created.terminal_id(created.root_pane).unwrap();
        let created_cwd = &app.state.terminals.get(created_terminal_id).unwrap().cwd;
        assert_eq!(
            crate::worktree::canonical_or_original(created_cwd),
            crate::worktree::canonical_or_original(&cached_cwd)
        );
        shutdown_test_runtimes(&mut app);
    }

    // The deferred `tab.create` (BET-9635): the first pane is opened on a blocking thread and the tab is added when its event arrives.

    /// A shell that stays up for a few seconds and then ends by itself. A test must not leave a child for its runtime to wait for: on
    /// Windows `Terminate` and `Kill` open the process with `PROCESS_QUERY_LIMITED_INFORMATION` only and so end nothing, and a `cmd.exe`
    /// left behind kept the test's runtime from shutting down for as long as the machine stayed up (BET-9635).
    fn lingering_test_shell() -> String {
        #[cfg(windows)]
        {
            let path =
                std::env::temp_dir().join(format!("herdr-lingering-{}.cmd", std::process::id()));
            std::fs::write(&path, "@ping -n 6 127.0.0.1 >nul\r\n").unwrap();
            path.display().to_string()
        }
        #[cfg(not(windows))]
        {
            "/bin/sh".to_string()
        }
    }

    fn deferred_app(shell: &str) -> App {
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &Config::default(),
            crate::app::AppPolicy::TEST,
            None,
            api_rx,
            crate::api::EventHub::default(),
        );
        app.state.default_shell = shell.into();
        app.state.shell_mode = ShellModeConfig::NonLogin;
        let mut workspace = Workspace::test_new("tabs");
        workspace.tabs[0].events = app.event_tx.clone();
        app.state.workspaces = vec![workspace];
        app.state.active = Some(0);
        app.state.selected = 0;
        app.state.ensure_test_terminals();
        app
    }

    fn deferred_params(label: Option<&str>) -> TabCreateParams {
        TabCreateParams {
            workspace_id: None,
            cwd: Some(std::env::temp_dir().display().to_string()),
            focus: false,
            label: label.map(str::to_string),
            env: Default::default(),
        }
    }

    /// Handles the app's pending events until the request has its answer; `None` after 10 s. A test shuts its runtimes down before it
    /// asserts, so that a failure is reported and does not leave a shell alive for the test runtime to wait for.
    async fn deferred_answer(
        app: &mut App,
        rx: &std::sync::mpsc::Receiver<String>,
    ) -> Option<String> {
        for _ in 0..500 {
            while let Ok(event) = app.event_rx.try_recv() {
                app.handle_internal_event(event);
            }
            if let Ok(answer) = rx.try_recv() {
                return Some(answer);
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        None
    }

    #[tokio::test]
    async fn deferred_tab_create_returns_at_once_and_adds_the_tab_when_its_pane_is_open() {
        let mut app = deferred_app(&lingering_test_shell());
        let (tx, rx) = std::sync::mpsc::channel();

        app.start_deferred_tab_create("req".into(), deferred_params(Some("lane")), tx);

        assert!(
            rx.try_recv().is_err(),
            "nothing is answered before the pane is open"
        );
        assert_eq!(app.pending_tab_spawns.len(), 1);
        assert_eq!(
            app.state.workspaces[0].tabs.len(),
            1,
            "no tab until the event"
        );
        let answer = deferred_answer(&mut app, &rx).await;

        let pending_left = app.pending_tab_spawns.len();
        let tabs = app.state.workspaces[0].tabs.len();
        let created = app.state.workspaces[0].tabs.get(1).map(|created| {
            let terminal_id = created.terminal_id(created.root_pane).unwrap();
            (
                created.custom_name.clone(),
                app.terminal_runtimes.get(terminal_id).is_some(),
                app.state.terminals.contains_key(terminal_id),
            )
        });
        shutdown_test_runtimes(&mut app);

        let answer = answer.expect("the deferred tab.create was not answered within 10 s");
        let success: SuccessResponse = serde_json::from_str(&answer).unwrap();
        assert!(
            matches!(success.result, ResponseResult::TabCreated { .. }),
            "{answer}"
        );
        assert_eq!(pending_left, 0);
        assert_eq!(tabs, 2, "{answer}");
        let (name, runtime_registered, terminal_registered) = created.expect("the new tab");
        assert_eq!(name.as_deref(), Some("lane"));
        assert!(runtime_registered, "its runtime is registered");
        assert!(terminal_registered, "its terminal state is registered");
    }

    #[tokio::test]
    async fn two_deferred_tab_creates_in_flight_take_different_tab_and_pane_numbers() {
        let mut app = deferred_app(&lingering_test_shell());
        let (tx_a, rx_a) = std::sync::mpsc::channel();
        let (tx_b, rx_b) = std::sync::mpsc::channel();

        app.start_deferred_tab_create("a".into(), deferred_params(Some("a")), tx_a);
        app.start_deferred_tab_create("b".into(), deferred_params(Some("b")), tx_b);
        let answer_a = deferred_answer(&mut app, &rx_a).await;
        let answer_b = deferred_answer(&mut app, &rx_b).await;

        let tabs = &app.state.workspaces[0].tabs;
        let numbers = (tabs.len() == 3).then(|| {
            let panes = &app.state.workspaces[0].public_pane_numbers;
            (
                tabs[1].number,
                tabs[2].number,
                panes[&tabs[1].root_pane],
                panes[&tabs[2].root_pane],
            )
        });
        shutdown_test_runtimes(&mut app);

        assert!(answer_a.is_some() && answer_b.is_some(), "both answered");
        let (tab_a, tab_b, pane_a, pane_b) = numbers.expect("three tabs");
        assert_ne!(tab_a, tab_b, "{answer_a:?} {answer_b:?}");
        assert_ne!(pane_a, pane_b);
    }

    #[tokio::test]
    async fn deferred_tab_create_for_a_workspace_that_is_gone_answers_with_an_error() {
        let mut app = deferred_app(&lingering_test_shell());
        let mut second = Workspace::test_new("second");
        second.tabs[0].events = app.event_tx.clone();
        app.state.workspaces.push(second);
        app.state.ensure_test_terminals();
        let (tx, rx) = std::sync::mpsc::channel();
        let mut params = deferred_params(None);
        params.workspace_id = Some(app.state.workspaces[1].id.clone());

        app.start_deferred_tab_create("req".into(), params, tx);
        let pending_at_start = app.pending_tab_spawns.len();
        app.state.workspaces.remove(1);
        let answer = deferred_answer(&mut app, &rx).await;

        let pending_left = app.pending_tab_spawns.len();
        let workspaces = app.state.workspaces.len();
        let tabs = app.state.workspaces[0].tabs.len();
        let runtimes = app.terminal_runtimes.len();
        shutdown_test_runtimes(&mut app);

        let answer = answer.expect("the deferred tab.create was not answered within 10 s");
        let value: serde_json::Value = serde_json::from_str(&answer).unwrap();
        assert_eq!(pending_at_start, 1, "the create was pending: {answer}");
        assert_eq!(value["error"]["code"], "workspace_not_found", "{answer}");
        assert_eq!(pending_left, 0);
        assert_eq!((workspaces, tabs), (1, 1));
        assert_eq!(runtimes, 0, "the orphan runtime was not kept");
    }

    #[tokio::test]
    async fn a_shell_that_exits_before_its_tab_is_here_is_held_and_replayed_not_lost() {
        let mut app = deferred_app(exiting_test_command());
        let (tx, rx) = std::sync::mpsc::channel();
        app.start_deferred_tab_create("req".into(), deferred_params(None), tx);
        let root = app
            .pending_tab_spawns
            .values()
            .next()
            .unwrap()
            .reservation
            .root_id;

        // The shell's exit reaches the loop before the tab does.
        app.handle_internal_event(crate::events::AppEvent::PaneDied {
            pane_id: root,
            exit_reason: crate::platform::ChildExitReason::Exited,
        });

        assert!(
            app.pending_tab_spawns
                .values()
                .all(|pending| pending.died.is_some()),
            "the exit of a pane the loop does not have yet is held on the pending create"
        );
        let answer = deferred_answer(&mut app, &rx).await;
        let pending_left = app.pending_tab_spawns.len();
        shutdown_test_runtimes(&mut app);

        let answer = answer.expect("the deferred tab.create was not answered within 10 s");
        let success: SuccessResponse = serde_json::from_str(&answer).unwrap();
        assert!(
            matches!(success.result, ResponseResult::TabCreated { .. }),
            "{answer}"
        );
        assert_eq!(pending_left, 0);
    }
}
