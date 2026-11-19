use std::time::Duration;

use bytes::Bytes;

use crate::api::schema::{
    AgentPromptParams, AgentRenameParams, AgentSendKeysParams, AgentStartParams, AgentTarget,
    PaneReadResult, ResponseResult,
};
use crate::app::App;

use super::responses::{encode_error, encode_error_body, encode_success};

const AGENT_PROMPT_SUBMIT_DELAY: Duration = Duration::from_millis(300);

// Codex's Windows input reader does not surface bracketed paste. It detects the prompt as a
// "paste burst" and, while that burst is buffered, rewrites a following Enter into a newline
// instead of submitting. The burst only flushes after an idle timeout, so any size-based delay is
// a timing guess that fails when ConPTY delivery lags it. Codex flushes a buffered burst
// synchronously when it receives a non-character key, so appending one after the paste gives the
// submission a deterministic paste boundary regardless of prompt size or delivery speed.
#[cfg(windows)]
fn append_codex_paste_boundary(runtime: &crate::terminal::TerminalRuntime, text: &mut Vec<u8>) {
    let keys = match crate::app::api_helpers::encode_api_keys(runtime, &["right".to_string()]) {
        Ok(keys) => keys,
        Err(key) => {
            tracing::warn!(key = %key, "failed to encode Codex paste boundary key");
            return;
        }
    };
    if let Some(key) = keys.into_iter().find(|bytes| !bytes.is_empty()) {
        text.extend_from_slice(&key);
    }
}

impl App {
    pub(super) fn handle_agent_list(&mut self, id: String) -> String {
        encode_success(
            id,
            ResponseResult::AgentList {
                agents: self.collect_agent_infos(),
            },
        )
    }

    pub(super) fn handle_agent_get(&mut self, id: String, target: AgentTarget) -> String {
        self.reconcile_managed_agent_target(&target.target);
        let agent = match self.agent_info_for_target(&target.target) {
            Ok(agent) => agent,
            Err(err) => return encode_error_body(id, self.agent_target_error_body(err)),
        };

        encode_success(id, ResponseResult::AgentInfo { agent })
    }

    pub(super) fn handle_agent_focus(&mut self, id: String, target: AgentTarget) -> String {
        let agent = match self.focus_agent_target(&target.target) {
            Ok(agent) => agent,
            Err(err) => return encode_error_body(id, self.agent_target_error_body(err)),
        };

        encode_success(id, ResponseResult::AgentInfo { agent })
    }

    pub(super) fn handle_agent_rename(&mut self, id: String, params: AgentRenameParams) -> String {
        let agent = match self.rename_agent_target(&params.target, params.name) {
            Ok(agent) => agent,
            Err(err) => return encode_error_body(id, self.agent_rename_error_body(err)),
        };

        encode_success(id, ResponseResult::AgentInfo { agent })
    }

    pub(super) fn handle_agent_start(&mut self, id: String, params: AgentStartParams) -> String {
        let (agent, argv) = match self.start_agent(params) {
            Ok(started) => started,
            Err(err) => return encode_error_body(id, self.agent_start_error_body(err)),
        };

        encode_success(id, ResponseResult::AgentStarted { agent, argv })
    }

    /// `agent.start` with the check that the pane's shell has no child process made off the app loop. The check reads the process table of
    /// the whole machine (0.1 to 1 s on the 3,000-process workstation, 190 ms on average over the starts of one morning), and every start
    /// held the loop for that long (BET-9635). The loop finds the shell's pid and returns; a blocking thread makes the check and sends
    /// `AppEvent::AgentStartShellChecked`, and `finish_deferred_agent_start` does everything else, on the loop, against the app as it is then.
    /// A pane with no shell process to look at (it is gone, or has none) is answered here at once, by the synchronous path.
    pub(crate) fn start_deferred_agent_start(
        &mut self,
        id: String,
        params: AgentStartParams,
        respond_to: std::sync::mpsc::Sender<String>,
    ) {
        let Some(shell_pid) = self.agent_start_shell_pid(&params.pane_id) else {
            let _ = respond_to.send(self.handle_agent_start(id, params));
            return;
        };
        let event_tx = self.event_tx.clone();
        tokio::task::spawn_blocking(move || {
            let shell_name = crate::platform::available_pane_shell(shell_pid);
            let _ = event_tx.blocking_send(crate::events::AppEvent::AgentStartShellChecked(
                Box::new(crate::events::AgentStartShellCheck {
                    request_id: id,
                    params,
                    shell_name,
                    respond_to,
                }),
            ));
        });
    }

    fn agent_start_shell_pid(&self, public_pane_id: &str) -> Option<u32> {
        let (ws_idx, pane_id) = self.parse_current_public_pane_id(public_pane_id)?;
        let terminal_id = self.state.workspaces.get(ws_idx)?.terminal_id(pane_id)?;
        self.terminal_runtimes.get(terminal_id)?.child_pid()
    }

    /// The end of `start_deferred_agent_start`, on the app loop: starts the agent, or answers why it cannot be started, and tells the caller.
    pub(crate) fn finish_deferred_agent_start(&mut self, check: crate::events::AgentStartShellCheck) {
        let crate::events::AgentStartShellCheck {
            request_id,
            params,
            shell_name,
            respond_to,
        } = check;
        let response = match self.start_agent_after_shell_check(
            params,
            crate::app::agents::ShellCheck::Done(shell_name),
        ) {
            Ok((agent, argv)) => {
                encode_success(request_id, ResponseResult::AgentStarted { agent, argv })
            }
            Err(err) => encode_error_body(request_id, self.agent_start_error_body(err)),
        };
        let _ = respond_to.send(response);
    }

    pub(crate) fn handle_deferred_agent_api_request(
        &mut self,
        request: crate::api::schema::Request,
        respond_to: std::sync::mpsc::Sender<String>,
    ) -> bool {
        let crate::api::schema::Method::AgentPrompt(params) = request.method else {
            return false;
        };
        let validated = match self.validate_agent_prompt(request.id, &params) {
            Ok(validated) => validated,
            Err(response) => {
                let _ = respond_to.send(response);
                return true;
            }
        };
        // Start the completion waiter before submitting, so a refused thread
        // fails the request while nothing has been sent yet.
        let (handoff_tx, handoff_rx) = std::sync::mpsc::channel::<(
            String,
            crate::api::schema::AgentInfo,
            std::sync::mpsc::Receiver<std::io::Result<()>>,
        )>();
        let waiter_respond_to = respond_to.clone();
        let spawned = crate::thread_spawn::spawn_named("herdr-agent-prompt", move || {
            let Ok((id, agent, completion)) = handoff_rx.recv() else {
                return;
            };
            let response = match completion.recv() {
                Ok(Ok(())) => encode_success(id, ResponseResult::AgentPrompted { agent }),
                Ok(Err(err)) if err.kind() == std::io::ErrorKind::TimedOut => {
                    encode_error(id, "timeout", err.to_string())
                }
                Ok(Err(err)) => encode_error(id, "agent_prompt_failed", err.to_string()),
                Err(_) => encode_error(id, "agent_prompt_failed", "pty actor closed"),
            };
            let _ = waiter_respond_to.send(response);
        });
        if let Err(err) = spawned {
            tracing::warn!(err = %err, "failed to spawn agent prompt thread");
            let _ = respond_to.send(encode_error(
                validated.id,
                "agent_prompt_failed",
                format!("could not start prompt completion waiter: {err}"),
            ));
            return true;
        }
        match self.submit_agent_prompt(validated, &params) {
            Ok(submission) => {
                let _ = handoff_tx.send(submission);
            }
            Err(response) => {
                let _ = respond_to.send(response);
            }
        }
        true
    }

    /// Runs every check that can reject a prompt without touching the pane.
    fn validate_agent_prompt(
        &self,
        id: String,
        params: &AgentPromptParams,
    ) -> Result<ValidatedAgentPrompt, String> {
        if params.text.is_empty() {
            return Err(encode_error(
                id,
                "empty_agent_prompt",
                "agent prompt must not be empty",
            ));
        }
        let resolved = match self.resolve_agent_target(&params.target) {
            Ok(resolved) => resolved,
            Err(err) => return Err(encode_error_body(id, self.agent_target_error_body(err))),
        };
        let Some(terminal_id) = self
            .state
            .workspaces
            .get(resolved.ws_idx)
            .and_then(|workspace| workspace.terminal_id(resolved.pane_id))
            .cloned()
        else {
            return Err(agent_not_found(id, &params.target));
        };
        let Some(terminal) = self.state.terminals.get(&terminal_id) else {
            return Err(agent_not_found(id, &params.target));
        };
        #[cfg(windows)]
        if terminal.effective_known_agent() == Some(crate::detect::Agent::Claude) {
            let Some(agent) = self.agent_info(resolved.ws_idx, resolved.pane_id) else {
                return Err(agent_not_found(id, &params.target));
            };
            return Ok(ValidatedAgentPrompt {
                id,
                ws_idx: resolved.ws_idx,
                pane_id: resolved.pane_id,
                expected_agent: crate::detect::Agent::Claude,
                agent,
                claude_terminal: Some(terminal_id),
            });
        }
        if terminal.state == crate::detect::AgentState::Blocked {
            return Err(encode_error(
                id,
                "agent_blocked",
                format!(
                    "agent {} is blocked and requires interactive input",
                    params.target
                ),
            ));
        }
        let Some(expected_agent) = terminal.effective_known_agent() else {
            return Err(agent_not_ready(id, &params.target));
        };
        if terminal.managed_agent_launch_pending() {
            return Err(agent_not_ready(id, &params.target));
        }
        let Some(runtime) = self.lookup_runtime_sender(resolved.ws_idx, resolved.pane_id) else {
            return Err(agent_not_found(id, &params.target));
        };
        if !super::super::agents::runtime_hosts_agent(runtime, expected_agent) {
            return Err(encode_error(
                id,
                "agent_not_ready",
                format!(
                    "agent {} is no longer the pane foreground process",
                    params.target
                ),
            ));
        }
        let Some(agent) = self.agent_info(resolved.ws_idx, resolved.pane_id) else {
            return Err(agent_not_found(id, &params.target));
        };
        // Typed text goes into the input box of the agent that has the user's keyboard, where it joins whatever is being typed
        // and the Enter submits it all. A Claude agent is sent a message instead (queue_claude_message); this is every other.
        if agent.focused {
            return Err(encode_error(
                id,
                "agent_input_busy",
                format!(
                    "agent {} is the pane the user is working in: its input box may hold what they are typing, so herdr does not type into it",
                    params.target
                ),
            ));
        }
        Ok(ValidatedAgentPrompt {
            id,
            ws_idx: resolved.ws_idx,
            pane_id: resolved.pane_id,
            expected_agent,
            agent,
            #[cfg(windows)]
            claude_terminal: None,
        })
    }

    fn submit_agent_prompt(
        &mut self,
        validated: ValidatedAgentPrompt,
        params: &AgentPromptParams,
    ) -> Result<
        (
            String,
            crate::api::schema::AgentInfo,
            std::sync::mpsc::Receiver<std::io::Result<()>>,
        ),
        String,
    > {
        let ValidatedAgentPrompt {
            id,
            ws_idx,
            pane_id,
            expected_agent,
            agent,
            #[cfg(windows)]
            claude_terminal,
        } = validated;
        #[cfg(windows)]
        if let Some(terminal_id) = claude_terminal {
            return self.queue_claude_message(id, params, &terminal_id);
        }
        let Some(runtime) = self.lookup_runtime_sender(ws_idx, pane_id) else {
            return Err(agent_not_found(id, &params.target));
        };
        #[cfg(windows)]
        let submit_deadline = params
            .wait
            .as_ref()
            .and_then(|wait| wait.submission_deadline);
        #[cfg(not(windows))]
        let submit_deadline = None;
        if expected_agent == crate::detect::Agent::GithubCopilot {
            // Copilot ignores synthetic Enter after focus loss until it receives focus gained.
            let focus = match crate::ghostty::encode_focus(crate::ghostty::FocusEvent::Gained) {
                Ok(focus) => focus,
                Err(err) => {
                    return Err(encode_error(id, "agent_prompt_failed", err.to_string()));
                }
            };
            if let Err(err) = runtime.try_send_bytes(Bytes::from(focus)) {
                return Err(encode_error(id, "agent_prompt_failed", err.to_string()));
            }
        }
        let (text, enter) =
            crate::app::api_helpers::encode_api_submission_parts(runtime, &params.text);
        #[cfg(windows)]
        let text = if expected_agent == crate::detect::Agent::Codex {
            let mut text = text;
            append_codex_paste_boundary(runtime, &mut text);
            text
        } else {
            text
        };
        let completion = runtime
            .queue_user_input_submission(
                Bytes::from(text),
                Bytes::from(enter),
                AGENT_PROMPT_SUBMIT_DELAY,
                submit_deadline,
            )
            .map_err(|err| encode_error(id.clone(), "agent_prompt_failed", err.to_string()))?;
        Ok((id, agent, completion))
    }

    /// A prompt for a Claude Code agent goes to the session as a cross-session message, never as keystrokes: typed text lands in
    /// the input box, among whatever its user is typing, and the Enter submits it all. A message waits in the session's own
    /// queue and reaches the agent at its next tool round, or wakes it when it is idle. The agent need not be unblocked or the
    /// foreground process of its pane: the message is not typed into anything. It is sent from a thread, never the main loop, as
    /// a busy pipe is retried for seconds. An agent whose restore has not resumed it yet is resumed now, and the message waits
    /// for the resumed session (`App::resume_agent_for_prompt`) on the one thread that serves every such wait
    /// (`claude_peer::deliver_when_up`).
    #[cfg(windows)]
    fn queue_claude_message(
        &mut self,
        id: String,
        params: &AgentPromptParams,
        terminal_id: &crate::terminal::TerminalId,
    ) -> Result<
        (
            String,
            crate::api::schema::AgentInfo,
            std::sync::mpsc::Receiver<std::io::Result<()>>,
        ),
        String,
    > {
        let resolved = match self.resolve_agent_target(&params.target) {
            Ok(resolved) => resolved,
            Err(err) => return Err(encode_error_body(id, self.agent_target_error_body(err))),
        };
        let resuming = match self.resume_agent_for_prompt(terminal_id) {
            Ok(resuming) => resuming,
            Err(err) => return Err(encode_error(id, "agent_resume_failed", err)),
        };
        let Some(agent) = self.agent_info(resolved.ws_idx, resolved.pane_id) else {
            return Err(agent_not_found(id, &params.target));
        };
        let Some(session) = agent
            .agent_session
            .as_ref()
            .map(|session| session.value.clone())
        else {
            return Err(encode_error(
                id,
                "agent_not_ready",
                format!(
                    "agent {} has not reported its Claude session yet",
                    params.target
                ),
            ));
        };
        let Some(dir) = crate::claude_peer::sessions_dir() else {
            return Err(encode_error(
                id,
                "agent_prompt_failed",
                "no home folder to find the Claude Code sessions in",
            ));
        };
        let text = params.text.clone();
        let machine = crate::claude_peer::Machine::current();
        let (done, completion) = std::sync::mpsc::channel();
        if resuming {
            crate::claude_peer::deliver_when_up(
                session,
                text,
                dir,
                machine,
                std::time::Instant::now() + crate::app::agent_resume::RESUMED_SESSION_WAIT,
                done,
            );
        } else {
            crate::thread_spawn::spawn_named("herdr-claude-prompt", move || {
                let sent = crate::claude_peer::deliver(&session, &text, &dir, &machine);
                let _ = done.send(sent.map_err(|err| std::io::Error::other(err.to_string())));
            })
            .map_err(|err| encode_error(id.clone(), "agent_prompt_failed", err.to_string()))?;
        }
        Ok((id, agent, completion))
    }

    pub(super) fn handle_agent_read(
        &mut self,
        id: String,
        params: crate::api::schema::AgentReadParams,
    ) -> String {
        let resolved = match self.resolve_agent_target(&params.target) {
            Ok(resolved) => resolved,
            Err(err) => return encode_error_body(id, self.agent_target_error_body(err)),
        };
        let Some((pane, workspace_id)) = self.lookup_runtime(resolved.ws_idx, resolved.pane_id)
        else {
            return agent_not_found(id, &params.target);
        };
        let snapshot = crate::app::api_helpers::read_terminal_snapshot(
            pane,
            params.source,
            params.format,
            params.lines,
        );

        encode_success(
            id,
            ResponseResult::PaneRead {
                read: PaneReadResult {
                    pane_id: self
                        .public_pane_id(resolved.ws_idx, resolved.pane_id)
                        .unwrap_or_else(|| params.target.clone()),
                    workspace_id,
                    tab_id: self
                        .public_tab_id(resolved.ws_idx, resolved.tab_idx)
                        .unwrap(),
                    source: params.source,
                    format: params.format,
                    text: snapshot.text,
                    revision: 0,
                    truncated: snapshot.truncated,
                },
            },
        )
    }

    pub(super) fn handle_agent_explain(&mut self, id: String, target: AgentTarget) -> String {
        let resolved = match self.resolve_agent_target(&target.target) {
            Ok(resolved) => resolved,
            Err(err) => return encode_error_body(id, self.agent_target_error_body(err)),
        };
        let Some((pane, _workspace_id)) = self.lookup_runtime(resolved.ws_idx, resolved.pane_id)
        else {
            return agent_not_found(id, &target.target);
        };
        let Some(terminal_id) = self
            .state
            .workspaces
            .get(resolved.ws_idx)
            .and_then(|workspace| workspace.terminal_id(resolved.pane_id))
        else {
            return agent_not_found(id, &target.target);
        };
        let Some(terminal) = self.state.terminals.get(terminal_id) else {
            return agent_not_found(id, &target.target);
        };
        if terminal.full_lifecycle_hook_authority_active() {
            let explain = serde_json::json!({
                "agent": terminal.effective_agent_label().unwrap_or("unknown"),
                "state": crate::detect::manifest::agent_state_label(terminal.state),
                "manifest_source": null,
                "manifest_version": null,
                "cached_remote_version": null,
                "local_override_shadowing_remote": false,
                "remote_update_status": null,
                "remote_update_error": null,
                "matched_rule": null,
                "visible_idle": false,
                "visible_blocker": false,
                "visible_working": false,
                "screen_detection_skipped": true,
                "screen_detection_skip_reason": "full_lifecycle_hook_authority",
                "skip_state_update": false,
                "skipped_update_reason": null,
                "fallback_reason": null,
                "warning": null,
                "evaluated_rules": [],
            });
            return encode_success(id, ResponseResult::AgentExplain { explain });
        }
        let Some(agent) = terminal.effective_known_agent().or(terminal.detected_agent) else {
            return encode_error(
                id,
                "agent_explain_unavailable",
                format!(
                    "agent target {} does not have a detected agent label",
                    target.target
                ),
            );
        };

        let screen = pane.detection_text();
        let osc_title = pane.agent_osc_title();
        let osc_progress = pane.agent_osc_progress();
        let explain = crate::detect::manifest::explain_with_input(
            agent,
            crate::detect::manifest::DetectionInput {
                screen: &screen,
                osc_title: &osc_title,
                osc_progress: &osc_progress,
            },
        );
        let value = crate::detect::manifest::explain_to_json_value(&explain);

        encode_success(id, ResponseResult::AgentExplain { explain: value })
    }

    pub(super) fn handle_agent_send_keys(
        &mut self,
        id: String,
        params: AgentSendKeysParams,
    ) -> String {
        let resolved = match self.resolve_agent_target(&params.target) {
            Ok(resolved) => resolved,
            Err(err) => return encode_error_body(id, self.agent_target_error_body(err)),
        };
        let Some(terminal_id) = self
            .state
            .workspaces
            .get(resolved.ws_idx)
            .and_then(|workspace| workspace.terminal_id(resolved.pane_id))
        else {
            return agent_not_found(id, &params.target);
        };
        let Some(expected_agent) = self
            .state
            .terminals
            .get(terminal_id)
            .and_then(|terminal| terminal.effective_known_agent())
        else {
            return agent_not_ready(id, &params.target);
        };
        let Some(runtime) = self.lookup_runtime_sender(resolved.ws_idx, resolved.pane_id) else {
            return agent_not_found(id, &params.target);
        };
        if !super::super::agents::runtime_hosts_agent(runtime, expected_agent) {
            return agent_not_ready(id, &params.target);
        }
        let encoded = match super::super::api_helpers::encode_api_keys(runtime, &params.keys) {
            Ok(encoded) => encoded,
            Err(key) => {
                return encode_error(id, "invalid_key", format!("unsupported key {key}"));
            }
        };
        let bytes: Vec<u8> = encoded.into_iter().flatten().collect();
        if let Err(err) = runtime.try_send_bytes(Bytes::from(bytes)) {
            return encode_error(id, "agent_send_keys_failed", err.to_string());
        }

        encode_success(id, ResponseResult::Ok {})
    }
}

struct ValidatedAgentPrompt {
    id: String,
    ws_idx: usize,
    pane_id: crate::layout::PaneId,
    expected_agent: crate::detect::Agent,
    agent: crate::api::schema::AgentInfo,
    #[cfg(windows)]
    claude_terminal: Option<crate::terminal::TerminalId>,
}

fn agent_not_ready(id: String, target: &str) -> String {
    encode_error(
        id,
        "agent_not_ready",
        format!("agent {target} is not an active named agent"),
    )
}

fn agent_not_found(id: String, target: &str) -> String {
    encode_error(
        id,
        "agent_not_found",
        format!("agent target {target} not found"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        api::schema::{AgentStatus, SuccessResponse},
        app::Mode,
        config::Config,
        detect::{Agent, AgentState},
        workspace::Workspace,
    };

    fn app_with_agent() -> App {
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &Config::default(),
            crate::app::AppPolicy::TEST,
            None,
            api_rx,
            crate::api::EventHub::default(),
        );
        app.state.workspaces = vec![Workspace::test_new("agent")];
        app.state.ensure_test_terminals();
        app.state.active = Some(0);
        app.state.selected = 0;
        app.state.mode = Mode::Terminal;
        app
    }

    fn start_deferred_agent_prompt(
        app: &mut App,
        id: &str,
        params: AgentPromptParams,
    ) -> std::sync::mpsc::Receiver<String> {
        let (respond_to, response_rx) = std::sync::mpsc::channel();
        assert!(app.handle_deferred_agent_api_request(
            crate::api::schema::Request {
                id: id.into(),
                method: crate::api::schema::Method::AgentPrompt(params),
            },
            respond_to,
        ));
        response_rx
    }

    fn run_deferred_agent_prompt(app: &mut App, id: &str, params: AgentPromptParams) -> String {
        start_deferred_agent_prompt(app, id, params)
            .recv_timeout(Duration::from_secs(1))
            .expect("agent prompt responds after submission")
    }

    fn agent_start_params(pane_id: &str, name: &str) -> AgentStartParams {
        AgentStartParams {
            name: name.into(),
            kind: "codex".into(),
            pane_id: pane_id.into(),
            args: Vec::new(),
            timeout_ms: Some(4_000),
        }
    }

    /// The app, its first pane's public id, the pane's terminal id, and the channel its runtime sends keystrokes to.
    fn app_with_idle_shell_pane() -> (
        App,
        String,
        crate::terminal::TerminalId,
        tokio::sync::mpsc::Receiver<Bytes>,
    ) {
        let mut app = app_with_agent();
        let root = app.state.workspaces[0].tabs[0].root_pane;
        let pane_id = app.pane_info(0, root).unwrap().pane_id;
        let terminal_id = app.state.workspaces[0].tabs[0].panes[&root]
            .attached_terminal_id
            .clone();
        let (runtime, rx) = crate::terminal::TerminalRuntime::test_with_channel(80, 24);
        app.terminal_runtimes.insert(terminal_id.clone(), runtime);
        (app, pane_id, terminal_id, rx)
    }

    fn shell_check(
        pane_id: &str,
        name: &str,
        shell_name: Option<&str>,
    ) -> (
        crate::events::AgentStartShellCheck,
        std::sync::mpsc::Receiver<String>,
    ) {
        let (respond_to, response_rx) = std::sync::mpsc::channel();
        (
            crate::events::AgentStartShellCheck {
                request_id: "req_agent_start".into(),
                params: agent_start_params(pane_id, name),
                shell_name: shell_name.map(str::to_string),
                respond_to,
            },
            response_rx,
        )
    }

    #[tokio::test]
    async fn an_agent_start_whose_shell_was_checked_off_the_loop_is_finished_by_the_event() {
        let (mut app, pane_id, terminal_id, mut rx) = app_with_idle_shell_pane();
        let (check, response_rx) = shell_check(&pane_id, "worker", Some("sh"));

        let changed = app.handle_internal_event_with_render_impact(
            crate::events::AppEvent::AgentStartShellChecked(Box::new(check)),
        );

        assert!(changed, "the app has to be drawn again");
        let response = response_rx
            .try_recv()
            .expect("the event answers the request");
        let success: SuccessResponse = serde_json::from_str(&response).unwrap();
        assert!(matches!(
            success.result,
            ResponseResult::AgentStarted { .. }
        ));
        assert_eq!(
            app.state.terminals[&terminal_id].agent_name.as_deref(),
            Some("worker")
        );
        assert!(
            rx.try_recv().is_ok(),
            "the agent's command line was typed into the pane"
        );
    }

    #[tokio::test]
    async fn an_agent_start_whose_shell_was_found_busy_is_refused_and_changes_nothing() {
        let (mut app, pane_id, _terminal_id, mut rx) = app_with_idle_shell_pane();
        let (check, response_rx) = shell_check(&pane_id, "worker", None);

        app.finish_deferred_agent_start(check);

        let response: serde_json::Value =
            serde_json::from_str(&response_rx.try_recv().unwrap()).unwrap();
        assert_eq!(response["error"]["code"], "agent_pane_busy");
        assert!(app.state.terminals.values().all(|t| t.agent_name.is_none()));
        assert!(rx.try_recv().is_err(), "nothing was typed into the pane");
    }

    #[tokio::test]
    async fn an_agent_start_is_checked_against_the_app_as_it_is_when_the_loop_finishes_it() {
        // The shell check ran before another request took the name; the start is validated when the loop gets to it, not when it began.
        let (mut app, pane_id, _terminal_id, mut rx) = app_with_idle_shell_pane();
        app.state.workspaces.push(Workspace::test_new("other"));
        app.state.ensure_test_terminals();
        let other = app.state.workspaces[1].tabs[0].root_pane;
        let other_terminal = app.state.workspaces[1].tabs[0].panes[&other]
            .attached_terminal_id
            .clone();
        let terminal = app.state.terminals.get_mut(&other_terminal).unwrap();
        terminal.set_agent_name("worker".into());
        terminal.set_detected_state(Some(Agent::Codex), AgentState::Idle);
        let (check, response_rx) = shell_check(&pane_id, "worker", Some("sh"));

        app.finish_deferred_agent_start(check);

        let response: serde_json::Value =
            serde_json::from_str(&response_rx.try_recv().unwrap()).unwrap();
        assert_eq!(response["error"]["code"], "agent_name_taken");
        assert!(rx.try_recv().is_err(), "nothing was typed into the pane");
    }

    #[tokio::test]
    async fn an_agent_start_in_a_pane_with_no_shell_process_is_answered_at_once() {
        // A runtime without a child process (the test one, or a pane whose shell is gone) leaves nothing to look at off the loop.
        let (mut app, pane_id, _terminal_id, mut rx) = app_with_idle_shell_pane();
        let (respond_to, response_rx) = std::sync::mpsc::channel();

        app.start_deferred_agent_start(
            "req_agent_start".into(),
            agent_start_params(&pane_id, "worker"),
            respond_to,
        );

        let response = response_rx
            .try_recv()
            .expect("answered before start_deferred_agent_start returned");
        let success: SuccessResponse = serde_json::from_str(&response).unwrap();
        assert!(matches!(
            success.result,
            ResponseResult::AgentStarted { .. }
        ));
        assert!(rx.try_recv().is_ok());
    }

    #[tokio::test]
    async fn an_agent_start_in_a_pane_that_is_gone_is_answered_at_once() {
        let (mut app, _pane_id, _terminal_id, _rx) = app_with_idle_shell_pane();
        let (respond_to, response_rx) = std::sync::mpsc::channel();

        app.start_deferred_agent_start(
            "req_agent_start".into(),
            agent_start_params("w9:p9", "worker"),
            respond_to,
        );

        let response: serde_json::Value =
            serde_json::from_str(&response_rx.try_recv().unwrap()).unwrap();
        assert_eq!(response["error"]["code"], "agent_pane_not_found");
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn windows_codex_prompt_flushes_paste_burst_before_enter() {
        let mut app = app_with_agent();
        app.state.active = None;
        let pane_id = app.state.workspaces[0].tabs[0].root_pane;
        let terminal_id = app.state.workspaces[0].tabs[0].panes[&pane_id]
            .attached_terminal_id
            .clone();
        let terminal = app.state.terminals.get_mut(&terminal_id).unwrap();
        terminal.set_agent_name("reviewer".into());
        terminal.set_detected_state(Some(Agent::Codex), AgentState::Idle);
        let (runtime, mut rx) = crate::terminal::TerminalRuntime::test_with_channel(80, 24);
        app.state.insert_test_runtime(pane_id, runtime);

        let response = run_deferred_agent_prompt(
            &mut app,
            "req",
            AgentPromptParams {
                target: "reviewer".into(),
                text: "A != B".into(),
                wait: None,
            },
        );
        let success: SuccessResponse = serde_json::from_str(&response).unwrap();
        assert!(matches!(
            success.result,
            ResponseResult::AgentPrompted { .. }
        ));
        // The non-character key must precede Enter so Codex commits the paste burst first.
        assert_eq!(rx.try_recv().unwrap(), Bytes::from_static(b"A != B\x1b[C"));
        assert_eq!(rx.try_recv().unwrap(), Bytes::from_static(b"\r"));
    }

    #[tokio::test]
    async fn a_false_process_exit_makes_a_named_live_agent_unreachable_by_name() {
        // Reproduces the registration loss reported on #3225 by rszrszrsz:
        // a live agent pane with an assigned name stops resolving by that name
        // while its process keeps running, and renaming is the only recovery.
        let mut app = app_with_agent();
        let pane_id = app.state.workspaces[0].tabs[0].root_pane;
        let terminal_id = app.state.workspaces[0].tabs[0].panes[&pane_id]
            .attached_terminal_id
            .clone();
        let observed_at = std::time::Instant::now();
        let terminal = app.state.terminals.get_mut(&terminal_id).unwrap();
        terminal.set_detected_state(Some(Agent::Pi), AgentState::Working);
        terminal.set_agent_name("reviewer".into());

        let found = app.handle_agent_get(
            "req:before".into(),
            AgentTarget {
                target: "reviewer".into(),
            },
        );
        assert!(
            serde_json::from_str::<SuccessResponse>(&found).is_ok(),
            "the assigned name must resolve while the agent is running: {found}"
        );

        // One process-exit observation, then the same agent is observed alive
        // again on the next probe - the process never actually went away.
        app.handle_internal_event(crate::events::AppEvent::StateChanged {
            pane_id,
            agent: Some(Agent::Pi),
            state: AgentState::Idle,
            visible_blocker: false,
            visible_working: false,
            process_exited: true,
            observed_at,
        });
        app.handle_internal_event(crate::events::AppEvent::AgentProcessDetected {
            pane_id,
            agent: Agent::Pi,
            observed_at: observed_at + std::time::Duration::from_secs(1),
        });

        let terminal = &app.state.terminals[&terminal_id];
        assert_eq!(
            terminal.detected_agent,
            Some(Agent::Pi),
            "the agent process is still there"
        );

        let after = app.handle_agent_get(
            "req:after".into(),
            AgentTarget {
                target: "reviewer".into(),
            },
        );
        assert!(
            serde_json::from_str::<SuccessResponse>(&after).is_ok(),
            "a live agent must stay reachable by its assigned name: {after}"
        );
    }

    #[tokio::test]
    async fn agent_prompt_sends_text_then_delays_enter() {
        let mut app = app_with_agent();
        app.state.active = None;
        let pane_id = app.state.workspaces[0].tabs[0].root_pane;
        let terminal_id = app.state.workspaces[0].tabs[0].panes[&pane_id]
            .attached_terminal_id
            .clone();
        let terminal = app.state.terminals.get_mut(&terminal_id).unwrap();
        terminal.set_agent_name("reviewer".into());
        terminal.set_detected_state(Some(Agent::OpenCode), AgentState::Working);
        let (runtime, mut rx) =
            crate::terminal::TerminalRuntime::test_with_channel_and_scrollback_bytes(
                80, 24, 0, b"", 2,
            );
        runtime.test_process_pty_bytes(b"\x1b[?2004h");
        app.state.insert_test_runtime(pane_id, runtime);

        let public_pane_id = app.public_pane_id(0, pane_id).unwrap();
        let bracketed_started = std::time::Instant::now();
        let response_rx = start_deferred_agent_prompt(
            &mut app,
            "req",
            AgentPromptParams {
                target: public_pane_id,
                text: "A != B".into(),
                wait: None,
            },
        );
        assert!(response_rx.try_recv().is_err());
        let response = response_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("agent prompt responds after submission");
        let success: SuccessResponse = serde_json::from_str(&response).unwrap();
        let ResponseResult::AgentPrompted { agent, .. } = success.result else {
            panic!("expected prompted response");
        };
        assert_eq!(agent.name.as_deref(), Some("reviewer"));
        assert_eq!(
            rx.try_recv().unwrap(),
            Bytes::from_static(b"\x1b[200~A != B\x1b[201~")
        );
        assert_eq!(rx.try_recv().unwrap(), Bytes::from_static(b"\r"));
        assert!(bracketed_started.elapsed() >= AGENT_PROMPT_SUBMIT_DELAY);

        app.lookup_runtime_sender(0, pane_id)
            .unwrap()
            .test_process_pty_bytes(b"\x1b[?2004l");
        let raw_started = std::time::Instant::now();
        let raw = run_deferred_agent_prompt(
            &mut app,
            "req-raw",
            AgentPromptParams {
                target: "reviewer".into(),
                text: "A != B".into(),
                wait: None,
            },
        );
        let raw: SuccessResponse = serde_json::from_str(&raw).unwrap();
        assert!(matches!(raw.result, ResponseResult::AgentPrompted { .. }));
        assert_eq!(rx.try_recv().unwrap(), Bytes::from_static(b"A != B"));
        assert_eq!(rx.try_recv().unwrap(), Bytes::from_static(b"\r"));
        assert!(raw_started.elapsed() >= AGENT_PROMPT_SUBMIT_DELAY);

        let rejected = run_deferred_agent_prompt(
            &mut app,
            "req-label",
            AgentPromptParams {
                target: "opencode".into(),
                text: "wrong target".into(),
                wait: None,
            },
        );
        let error: crate::api::schema::ErrorResponse = serde_json::from_str(&rejected).unwrap();
        assert_eq!(error.error.code, "agent_not_found");
        assert!(rx.try_recv().is_err());
    }

    // A prompt to a Claude Code agent is a cross-session message, not keystrokes: the text that is in the pane's input box
    // (what its user is typing) is untouched, no byte reaches the pane, and the message reaches the session's pipe.
    #[cfg(windows)]
    mod claude_message {
        use super::*;
        use crate::agent_resume::{AgentSessionRef, PersistedAgentSession};
        use crate::claude_peer::{key_file_name, Machine, PeerError, TEST_MACHINE};
        use std::sync::Mutex;

        const SESSION: &str = "3c53fb2a-3fd1-4dc5-a857-064c412e0162";
        const PIPE: &str = r"\\.\pipe\LOCAL\cc-msg-774131b569a4d0ab92d8d71730bc96c9";
        const STARTED: u64 = 134370848602109159;

        static SENT: Mutex<Vec<(String, String)>> = Mutex::new(Vec::new());

        fn write(pipe: &str, bytes: &[u8]) -> Result<(), PeerError> {
            SENT.lock()
                .unwrap()
                .push((pipe.to_owned(), String::from_utf8(bytes.to_vec()).unwrap()));
            Ok(())
        }

        fn running(_pid: u32) -> Option<u64> {
            Some(STARTED)
        }

        /// A sessions folder with the session's record and key, the fake machine, and the env a send reads; undone on drop.
        struct Fixture {
            dir: std::path::PathBuf,
            _env: std::sync::MutexGuard<'static, ()>,
        }

        impl Fixture {
            fn new(with_session: bool) -> Self {
                let env = crate::config::test_config_env_lock()
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                let dir = std::env::temp_dir().join(format!(
                    "herdr-claude-message-{}-{}",
                    std::process::id(),
                    SENT.lock().unwrap().len()
                ));
                std::fs::create_dir_all(&dir).unwrap();
                if with_session {
                    write_session(&dir);
                }
                std::env::set_var("HERDR_CLAUDE_SESSIONS_DIR", &dir);
                *TEST_MACHINE.lock().unwrap() = Some(Machine {
                    process_start: running,
                    write_pipe: write,
                });
                SENT.lock().unwrap().clear();
                Self { dir, _env: env }
            }
        }

        /// The session's record and key, as Claude Code writes them once it is up.
        fn write_session(dir: &std::path::Path) {
            std::fs::write(
                dir.join("105968.json"),
                format!(
                    r#"{{"pid":105968,"sessionId":"{SESSION}","messagingSocketPath":{PIPE:?},"procStart":"{STARTED}","pidDomain":"win32:test"}}"#
                ),
            )
            .unwrap();
            std::fs::write(
                dir.join(key_file_name(105968, PIPE)),
                format!(r#"{{"peerToken":"0123456789abcdef0123456789abcdef","procStartFt":{STARTED},"pidDomain":"win32:test"}}"#),
            )
            .unwrap();
        }

        impl Drop for Fixture {
            fn drop(&mut self) {
                *TEST_MACHINE.lock().unwrap() = None;
                std::env::remove_var("HERDR_CLAUDE_SESSIONS_DIR");
                let _ = std::fs::remove_dir_all(&self.dir);
            }
        }

        /// A Claude agent in the pane the user is working in, whose input box shows `typed`.
        fn claude_pane(
            typed: &[u8],
            state: AgentState,
        ) -> (App, tokio::sync::mpsc::Receiver<Bytes>) {
            let mut app = app_with_agent();
            let pane_id = app.state.workspaces[0].tabs[0].root_pane;
            let terminal_id = app.state.workspaces[0].tabs[0].panes[&pane_id]
                .attached_terminal_id
                .clone();
            let terminal = app.state.terminals.get_mut(&terminal_id).unwrap();
            terminal.set_agent_name("reviewer".into());
            terminal.set_detected_state(Some(Agent::Claude), state);
            terminal.set_persisted_agent_session(PersistedAgentSession {
                source: "herdr:claude".into(),
                agent: "claude".into(),
                session_ref: AgentSessionRef::id(SESSION).unwrap(),
            });
            let (runtime, rx) =
                crate::terminal::TerminalRuntime::test_with_channel_and_scrollback_bytes(
                    80, 24, 0, typed, 4,
                );
            app.state.insert_test_runtime(pane_id, runtime);
            (app, rx)
        }

        fn prompt(app: &mut App, text: &str) -> String {
            run_deferred_agent_prompt(
                app,
                "req",
                AgentPromptParams {
                    target: "reviewer".into(),
                    text: text.into(),
                    wait: None,
                },
            )
        }

        async fn nothing_typed(rx: &mut tokio::sync::mpsc::Receiver<Bytes>) -> bool {
            tokio::time::timeout(
                AGENT_PROMPT_SUBMIT_DELAY + Duration::from_millis(150),
                rx.recv(),
            )
            .await
            .is_err()
        }

        #[tokio::test]
        async fn a_prompt_leaves_the_text_in_the_input_box_untouched_and_still_arrives() {
            let _fixture = Fixture::new(true);
            let (mut app, mut rx) =
                claude_pane(b"> fix the door in level 4 and also", AgentState::Idle);
            let response = prompt(&mut app, "[bet land] Your land did not submit: <CL> 22890");
            let success: SuccessResponse = serde_json::from_str(&response).unwrap();
            assert!(
                matches!(success.result, ResponseResult::AgentPrompted { .. }),
                "{response}"
            );

            assert!(
                nothing_typed(&mut rx).await,
                "bytes reached the pane: its input box was written to"
            );
            let sent = SENT.lock().unwrap();
            assert_eq!(sent.len(), 1, "one message");
            let (pipe, wire) = &sent[0];
            assert_eq!(pipe, PIPE);
            let lines: Vec<&str> = wire.split_terminator('\n').collect();
            assert_eq!(lines.len(), 2, "{wire:?}");
            assert!(
                lines[0].contains("\"auth\"")
                    && lines[0].contains("0123456789abcdef0123456789abcdef")
            );
            let message: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
            assert_eq!(
                message["message"]["content"],
                "<cross-session-message>\n[bet land] Your land did not submit: <CL> 22890\n</cross-session-message>"
            );
        }

        #[tokio::test]
        async fn an_agent_waiting_on_a_prompt_still_gets_the_message_and_nothing_is_typed() {
            let _fixture = Fixture::new(true);
            let (mut app, mut rx) =
                claude_pane(b"Do you want to proceed? 1. Yes", AgentState::Blocked);
            let response = prompt(&mut app, "wake up");
            let success: SuccessResponse = serde_json::from_str(&response).unwrap();
            assert!(
                matches!(success.result, ResponseResult::AgentPrompted { .. }),
                "{response}"
            );
            assert!(
                nothing_typed(&mut rx).await,
                "an answer was typed into the blocking prompt"
            );
            assert_eq!(SENT.lock().unwrap().len(), 1);
        }

        #[tokio::test]
        async fn a_session_that_cannot_be_reached_is_an_error_and_nothing_is_typed() {
            let _fixture = Fixture::new(false);
            let (mut app, mut rx) = claude_pane(b"> half typed", AgentState::Idle);
            let response = prompt(&mut app, "wake up");
            let error: crate::api::schema::ErrorResponse = serde_json::from_str(&response).unwrap();
            assert_eq!(error.error.code, "agent_prompt_failed");
            assert!(
                error
                    .error
                    .message
                    .contains("no Claude Code session record"),
                "{}",
                error.error.message
            );
            assert!(
                nothing_typed(&mut rx).await,
                "the prompt fell back to typing"
            );
            assert!(SENT.lock().unwrap().is_empty());
        }

        #[tokio::test]
        async fn an_agent_with_no_session_yet_is_not_ready_and_nothing_is_typed() {
            let _fixture = Fixture::new(true);
            let (mut app, mut rx) = claude_pane(b"", AgentState::Idle);
            let pane_id = app.state.workspaces[0].tabs[0].root_pane;
            let terminal_id = app.state.workspaces[0].tabs[0].panes[&pane_id]
                .attached_terminal_id
                .clone();
            app.state
                .terminals
                .get_mut(&terminal_id)
                .unwrap()
                .persisted_agent_session = None;
            let response = prompt(&mut app, "wake up");
            let error: crate::api::schema::ErrorResponse = serde_json::from_str(&response).unwrap();
            assert_eq!(error.error.code, "agent_not_ready");
            assert!(nothing_typed(&mut rx).await);
        }

        fn root_terminal(app: &App) -> crate::terminal::TerminalId {
            let pane_id = app.state.workspaces[0].tabs[0].root_pane;
            app.state.workspaces[0].tabs[0].panes[&pane_id]
                .attached_terminal_id
                .clone()
        }

        // A prompt to an agent whose resume still waits in the restore queue starts that resume at once, instead of failing
        // because the session is not up (BET-9635); a resume that cannot start says why.
        #[tokio::test]
        async fn a_prompt_to_an_agent_whose_resume_waits_starts_the_resume() {
            let _fixture = Fixture::new(false);
            let (mut app, _rx) = claude_pane(b"", AgentState::Idle);
            app.state.view.terminal_area = ratatui::layout::Rect::new(0, 0, 100, 30);
            let terminal_id = root_terminal(&app);
            let terminal = app.state.terminals.get_mut(&terminal_id).unwrap();
            terminal.cwd = std::env::current_dir()
                .unwrap()
                .join("__herdr_missing_resume_cwd__");
            terminal.pending_agent_resume_plan = Some(crate::agent_resume::AgentResumePlan {
                agent: "claude".into(),
                argv: vec!["claude".into(), "--resume".into(), SESSION.into()],
                dedupe_key: SESSION.into(),
            });

            let response = prompt(&mut app, "wake up");
            let error: crate::api::schema::ErrorResponse = serde_json::from_str(&response).unwrap();
            assert_eq!(error.error.code, "agent_resume_failed", "{response}");
            assert!(
                error
                    .error
                    .message
                    .contains("Saved directory is unavailable"),
                "{}",
                error.error.message
            );
            assert!(app.state.terminals[&terminal_id]
                .pending_agent_resume_plan
                .is_none());
            assert!(SENT.lock().unwrap().is_empty());
        }

        fn resuming_claude_pane() -> (App, tokio::sync::mpsc::Receiver<Bytes>) {
            let (mut app, rx) = claude_pane(b"", AgentState::Idle);
            let terminal_id = root_terminal(&app);
            app.state
                .terminals
                .get_mut(&terminal_id)
                .unwrap()
                .agent_resume_started_at = Some(std::time::Instant::now());
            (app, rx)
        }

        fn start_prompt(app: &mut App, text: &str) -> std::sync::mpsc::Receiver<String> {
            start_deferred_agent_prompt(
                app,
                "req",
                AgentPromptParams {
                    target: "reviewer".into(),
                    text: text.into(),
                    wait: None,
                },
            )
        }

        fn prompted(response: std::sync::mpsc::Receiver<String>) {
            let response = response
                .recv_timeout(Duration::from_secs(5))
                .expect("the prompt answers once the session is up");
            let success: SuccessResponse = serde_json::from_str(&response).unwrap();
            assert!(
                matches!(success.result, ResponseResult::AgentPrompted { .. }),
                "{response}"
            );
        }

        // A resumed Claude Code writes its session record and messaging key only once it is up: a prompt sent to it before
        // then waits for them and arrives, where it used to fail with no session (BET-9635). The wait is off the main loop:
        // while the prompt waits, the loop that handled it answers a pane.get.
        #[tokio::test]
        async fn a_prompt_to_an_agent_being_resumed_waits_for_its_session_off_the_main_loop() {
            let fixture = Fixture::new(false);
            let (mut app, mut rx) = resuming_claude_pane();
            let response = start_prompt(&mut app, "after the restart");
            std::thread::sleep(Duration::from_millis(600));
            assert!(response.try_recv().is_err(), "the session is not up yet");

            let pane_id = app
                .public_pane_id(0, app.state.workspaces[0].tabs[0].root_pane)
                .unwrap();
            let get = app.handle_api_request(crate::api::schema::Request {
                id: "get".into(),
                method: crate::api::schema::Method::PaneGet(crate::api::schema::PaneTarget {
                    pane_id,
                }),
            });
            assert!(
                serde_json::from_str::<SuccessResponse>(&get).is_ok(),
                "{get}"
            );
            assert!(response.try_recv().is_err(), "the prompt still waits");

            write_session(&fixture.dir);
            prompted(response);
            assert_eq!(SENT.lock().unwrap().len(), 1);
            assert!(nothing_typed(&mut rx).await);
        }

        // Prompts waiting for resumed sessions share one thread, which reads the session folder once a poll however many wait:
        // a broadcast to a restoring fleet must not start a thread and a folder scan every 500 ms per pane. A second prompt to
        // the same agent arrives after the first.
        #[tokio::test]
        async fn prompts_waiting_for_a_resumed_session_share_one_waiter_and_arrive_in_order() {
            use crate::claude_peer::{WAITER_POLLS_AND_READS, WAITER_THREADS_STARTED};
            use std::sync::atomic::Ordering;
            let fixture = Fixture::new(false);
            let (mut app, _rx) = resuming_claude_pane();
            let threads = WAITER_THREADS_STARTED.load(Ordering::Relaxed);
            let (polls, reads) = *WAITER_POLLS_AND_READS.lock().unwrap();

            let first = start_prompt(&mut app, "first");
            let second = start_prompt(&mut app, "second");
            std::thread::sleep(Duration::from_millis(1200));
            write_session(&fixture.dir);
            prompted(first);
            prompted(second);

            assert!(WAITER_THREADS_STARTED.load(Ordering::Relaxed) - threads <= 1);
            let (polls_after, reads_after) = *WAITER_POLLS_AND_READS.lock().unwrap();
            assert!(polls_after - polls >= 3, "{} polls", polls_after - polls);
            assert_eq!(
                reads_after - reads,
                polls_after - polls,
                "one folder read a poll"
            );
            let sent = SENT.lock().unwrap();
            assert_eq!(sent.len(), 2);
            assert!(
                sent[0].1.contains("first") && sent[1].1.contains("second"),
                "{sent:?}"
            );
        }
    }

    // Any other agent is typed to, and only when the user is not in its pane: its input box may hold what they are typing.
    #[tokio::test]
    async fn agent_prompt_does_not_type_into_the_pane_the_user_is_working_in() {
        let mut app = app_with_agent();
        let pane_id = app.state.workspaces[0].tabs[0].root_pane;
        let terminal_id = app.state.workspaces[0].tabs[0].panes[&pane_id]
            .attached_terminal_id
            .clone();
        let terminal = app.state.terminals.get_mut(&terminal_id).unwrap();
        terminal.set_agent_name("reviewer".into());
        terminal.set_detected_state(Some(Agent::OpenCode), AgentState::Idle);
        let (runtime, mut rx) = crate::terminal::TerminalRuntime::test_with_channel(80, 24);
        app.state.insert_test_runtime(pane_id, runtime);

        let params = || AgentPromptParams {
            target: "reviewer".into(),
            text: "A != B".into(),
            wait: None,
        };
        let refused = run_deferred_agent_prompt(&mut app, "req", params());
        let error: crate::api::schema::ErrorResponse = serde_json::from_str(&refused).unwrap();
        assert_eq!(error.error.code, "agent_input_busy");
        assert!(
            tokio::time::timeout(
                AGENT_PROMPT_SUBMIT_DELAY + Duration::from_millis(100),
                rx.recv()
            )
            .await
            .is_err(),
            "text was typed into the focused pane"
        );

        app.state.active = None;
        let accepted = run_deferred_agent_prompt(&mut app, "req", params());
        let success: SuccessResponse = serde_json::from_str(&accepted).unwrap();
        assert!(
            matches!(success.result, ResponseResult::AgentPrompted { .. }),
            "{accepted}"
        );
        assert_eq!(rx.try_recv().unwrap(), Bytes::from_static(b"A != B"));
    }

    #[tokio::test]
    async fn agent_prompt_waiter_spawn_failure_fails_before_submitting() {
        let mut app = app_with_agent();
        let pane_id = app.state.workspaces[0].tabs[0].root_pane;
        let terminal_id = app.state.workspaces[0].tabs[0].panes[&pane_id]
            .attached_terminal_id
            .clone();
        let terminal = app.state.terminals.get_mut(&terminal_id).unwrap();
        terminal.set_agent_name("reviewer".into());
        terminal.set_detected_state(Some(Agent::OpenCode), AgentState::Idle);
        let (runtime, mut rx) = crate::terminal::TerminalRuntime::test_with_channel(80, 24);
        app.state.insert_test_runtime(pane_id, runtime);
        app.state.active = None;

        crate::thread_spawn::test_hook::fail_next_spawns(1);
        let response = run_deferred_agent_prompt(
            &mut app,
            "req",
            AgentPromptParams {
                target: "reviewer".into(),
                text: "hello".into(),
                wait: None,
            },
        );

        let error: crate::api::schema::ErrorResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(error.error.code, "agent_prompt_failed");
        std::thread::sleep(AGENT_PROMPT_SUBMIT_DELAY * 2);
        assert!(rx.try_recv().is_err(), "a failed prompt must not be sent");
    }

    #[tokio::test]
    async fn agent_prompt_validation_errors_win_over_waiter_spawn_failure() {
        let mut app = app_with_agent();
        let pane_id = app.state.workspaces[0].tabs[0].root_pane;
        let terminal_id = app.state.workspaces[0].tabs[0].panes[&pane_id]
            .attached_terminal_id
            .clone();
        let terminal = app.state.terminals.get_mut(&terminal_id).unwrap();
        terminal.set_agent_name("reviewer".into());
        terminal.set_detected_state(Some(Agent::OpenCode), AgentState::Idle);
        let (runtime, mut rx) = crate::terminal::TerminalRuntime::test_with_channel(80, 24);
        app.state.insert_test_runtime(pane_id, runtime);

        for (target, text, code) in [
            ("reviewer", "", "empty_agent_prompt"),
            ("missing", "hello", "agent_not_found"),
        ] {
            crate::thread_spawn::test_hook::fail_next_spawns(1);
            let response = run_deferred_agent_prompt(
                &mut app,
                "req",
                AgentPromptParams {
                    target: target.into(),
                    text: text.into(),
                    wait: None,
                },
            );

            let error: crate::api::schema::ErrorResponse = serde_json::from_str(&response).unwrap();
            assert_eq!(error.error.code, code);
            assert!(
                crate::thread_spawn::spawn_named("probe", || {}).is_err(),
                "a rejected prompt must not start a waiter thread"
            );
        }
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn agent_prompt_rejects_blocked_agent_without_writing() {
        let mut app = app_with_agent();
        let pane_id = app.state.workspaces[0].tabs[0].root_pane;
        let terminal_id = app.state.workspaces[0].tabs[0].panes[&pane_id]
            .attached_terminal_id
            .clone();
        let terminal = app.state.terminals.get_mut(&terminal_id).unwrap();
        terminal.set_agent_name("reviewer".into());
        terminal.set_detected_state(Some(Agent::GithubCopilot), AgentState::Blocked);
        let (runtime, mut rx) = crate::terminal::TerminalRuntime::test_with_channel(80, 24);
        app.state.insert_test_runtime(pane_id, runtime);

        let response = run_deferred_agent_prompt(
            &mut app,
            "req",
            AgentPromptParams {
                target: "reviewer".into(),
                text: "unrelated prompt".into(),
                wait: None,
            },
        );

        let error: crate::api::schema::ErrorResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(error.error.code, "agent_blocked");
        assert!(
            tokio::time::timeout(
                AGENT_PROMPT_SUBMIT_DELAY + Duration::from_millis(100),
                rx.recv()
            )
            .await
            .is_err(),
            "blocked prompt wrote or scheduled terminal input"
        );
    }

    #[tokio::test]
    async fn agent_prompt_focuses_copilot_before_submitting() {
        let mut app = app_with_agent();
        app.state.active = None;
        let pane_id = app.state.workspaces[0].tabs[0].root_pane;
        let terminal_id = app.state.workspaces[0].tabs[0].panes[&pane_id]
            .attached_terminal_id
            .clone();
        let terminal = app.state.terminals.get_mut(&terminal_id).unwrap();
        terminal.set_agent_name("reviewer".into());
        terminal.set_detected_state(Some(Agent::GithubCopilot), AgentState::Idle);
        let (runtime, mut rx) =
            crate::terminal::TerminalRuntime::test_with_channel_and_scrollback_bytes(
                80, 24, 0, b"", 3,
            );
        runtime.test_process_pty_bytes(b"\x1b[?2004h");
        app.state.insert_test_runtime(pane_id, runtime);

        let response = run_deferred_agent_prompt(
            &mut app,
            "req",
            AgentPromptParams {
                target: "reviewer".into(),
                text: "A != B".into(),
                wait: None,
            },
        );
        let success: SuccessResponse = serde_json::from_str(&response).unwrap();
        assert!(matches!(
            success.result,
            ResponseResult::AgentPrompted { .. }
        ));
        assert_eq!(rx.try_recv().unwrap(), Bytes::from_static(b"\x1b[I"));
        assert_eq!(
            rx.try_recv().unwrap(),
            Bytes::from_static(b"\x1b[200~A != B\x1b[201~")
        );
        assert_eq!(rx.try_recv().unwrap(), Bytes::from_static(b"\r"));
    }

    #[tokio::test]
    async fn agent_send_keys_validates_every_key_before_writing() {
        let mut app = app_with_agent();
        let pane_id = app.state.workspaces[0].tabs[0].root_pane;
        let terminal_id = app.state.workspaces[0].tabs[0].panes[&pane_id]
            .attached_terminal_id
            .clone();
        let terminal = app.state.terminals.get_mut(&terminal_id).unwrap();
        terminal.set_agent_name("reviewer".into());
        terminal.set_detected_state(Some(Agent::Pi), AgentState::Idle);
        let (runtime, mut rx) = crate::terminal::TerminalRuntime::test_with_channel(80, 24);
        app.state.insert_test_runtime(pane_id, runtime);

        let rejected = app.handle_agent_send_keys(
            "req-invalid".into(),
            AgentSendKeysParams {
                target: "reviewer".into(),
                keys: vec!["enter".into(), "not-a-key".into()],
            },
        );
        let error: crate::api::schema::ErrorResponse = serde_json::from_str(&rejected).unwrap();
        assert_eq!(error.error.code, "invalid_key");
        assert!(rx.try_recv().is_err());

        let sent = app.handle_agent_send_keys(
            "req-valid".into(),
            AgentSendKeysParams {
                target: "reviewer".into(),
                keys: vec!["up".into(), "enter".into()],
            },
        );
        let success: SuccessResponse = serde_json::from_str(&sent).unwrap();
        assert!(matches!(success.result, ResponseResult::Ok {}));
        assert_eq!(rx.try_recv().unwrap(), Bytes::from_static(b"\x1b[A\r"));
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn agent_prompt_rejects_managed_agent_while_startup_is_pending() {
        let mut app = app_with_agent();
        let pane_id = app.state.workspaces[0].tabs[0].root_pane;
        let terminal_id = app.state.workspaces[0].tabs[0].panes[&pane_id]
            .attached_terminal_id
            .clone();
        let terminal = app.state.terminals.get_mut(&terminal_id).unwrap();
        let now = std::time::Instant::now();
        terminal.begin_managed_agent(
            "reviewer".into(),
            Agent::OpenCode,
            now,
            std::time::Duration::from_secs(3),
            std::time::Duration::from_secs(10),
        );
        terminal.set_detected_state(Some(Agent::OpenCode), AgentState::Idle);
        let (runtime, mut rx) = crate::terminal::TerminalRuntime::test_with_channel(80, 24);
        app.state.insert_test_runtime(pane_id, runtime);

        let response = run_deferred_agent_prompt(
            &mut app,
            "req-pending",
            AgentPromptParams {
                target: "reviewer".into(),
                text: "A != B".into(),
                wait: None,
            },
        );
        let error: crate::api::schema::ErrorResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(error.error.code, "agent_not_ready");
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn agent_focus_marks_already_focused_done_agent_seen() {
        let mut app = app_with_agent();
        app.state.outer_terminal_focus = Some(false);

        let pane_id = app.state.workspaces[0].tabs[0].root_pane;
        let terminal_id = app.state.workspaces[0].tabs[0].panes[&pane_id]
            .attached_terminal_id
            .clone();
        app.state
            .terminals
            .get_mut(&terminal_id)
            .unwrap()
            .set_detected_state(Some(Agent::Pi), AgentState::Idle);
        app.state.workspaces[0].tabs[0]
            .panes
            .get_mut(&pane_id)
            .unwrap()
            .seen = false;
        app.state.workspaces[0].tabs[0].layout.focus_pane(pane_id);

        let response = app.handle_agent_focus(
            "req".into(),
            AgentTarget {
                target: app.public_pane_id(0, pane_id).unwrap(),
            },
        );

        let success: SuccessResponse = serde_json::from_str(&response).unwrap();
        let ResponseResult::AgentInfo { agent } = success.result else {
            panic!("expected agent info response");
        };
        assert_eq!(agent.agent_status, AgentStatus::Idle);
    }

    #[test]
    fn agent_rename_does_not_replace_the_pane_label() {
        let mut app = app_with_agent();
        let pane_id = app.state.workspaces[0].tabs[0].root_pane;
        let terminal_id = app.state.workspaces[0].tabs[0].panes[&pane_id]
            .attached_terminal_id
            .clone();
        let terminal = app.state.terminals.get_mut(&terminal_id).unwrap();
        terminal.set_manual_label("shell-pane".into());
        terminal.set_detected_state(Some(Agent::Pi), AgentState::Idle);
        let target = app.public_pane_id(0, pane_id).unwrap();

        for name in [Some("reviewer".to_string()), None] {
            let response = app.handle_agent_rename(
                "req".into(),
                AgentRenameParams {
                    target: target.clone(),
                    name,
                },
            );
            let success: SuccessResponse = serde_json::from_str(&response).unwrap();
            assert!(matches!(success.result, ResponseResult::AgentInfo { .. }));
            assert_eq!(
                app.state.terminals[&terminal_id].manual_label.as_deref(),
                Some("shell-pane")
            );
        }
    }
}
