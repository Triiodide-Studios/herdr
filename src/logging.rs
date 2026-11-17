use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tracing_subscriber::fmt::writer::MakeWriter;
use tracing_subscriber::EnvFilter;

const DEFAULT_MAX_LOG_BYTES: u64 = 5 * 1024 * 1024;
const DEFAULT_RETAINED_LOG_FILES: usize = 0;

pub(crate) fn init_file_logging(file_name: &str) {
    let Ok(make_writer) = RotatingFileMakeWriter::new(
        crate::session::data_dir(),
        file_name,
        DEFAULT_MAX_LOG_BYTES,
        DEFAULT_RETAINED_LOG_FILES,
    ) else {
        return;
    };

    let filter =
        EnvFilter::try_from_env("HERDR_LOG").unwrap_or_else(|_| EnvFilter::new("herdr=info"));

    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(make_writer)
        .with_ansi(false)
        .with_target(true)
        .try_init();
}

pub(crate) fn help_log_paths_summary() -> String {
    let dir = crate::session::data_dir();
    format!(
        "{} (plus herdr-client.log, herdr-server.log)",
        dir.join("herdr.log").display()
    )
}

pub(crate) fn startup(role: &'static str) {
    tracing::info!(
        event = "app.startup",
        subsystem = role,
        outcome = "started",
        pid = std::process::id(),
        "herdr starting"
    );
}

pub(crate) fn shutdown(role: &'static str) {
    tracing::info!(
        event = "app.shutdown",
        subsystem = role,
        outcome = "completed",
        pid = std::process::id(),
        "herdr exiting"
    );
}

pub(crate) fn api_request_started(request_id: &str, method: &'static str, changes_ui: bool) {
    let event = "api.request.start";
    let subsystem = "api";
    let outcome = "started";
    let message = "api request received";
    if changes_ui && !is_routine_api_method(method) {
        tracing::info!(
            event,
            subsystem,
            outcome,
            request_id,
            method,
            changes_ui,
            "{message}"
        );
    } else {
        tracing::debug!(
            event,
            subsystem,
            outcome,
            request_id,
            method,
            changes_ui,
            "{message}"
        );
    }
}

pub(crate) fn api_request_completed(
    request_id: &str,
    method: &'static str,
    outcome: &'static str,
    changes_ui: bool,
) {
    let event = "api.request.complete";
    let subsystem = "api";
    let message = "api request completed";
    if outcome != "ok" || (changes_ui && !is_routine_api_method(method)) {
        tracing::info!(event, subsystem, outcome, request_id, method, "{message}");
    } else {
        tracing::debug!(event, subsystem, outcome, request_id, method, "{message}");
    }
}

fn is_routine_api_method(method: &str) -> bool {
    matches!(
        method,
        "pane.get"
            | "pane.read"
            | "pane.list"
            | "workspace.list"
            | "tab.list"
            | "pane.report_agent"
            | "pane.report_agent_session"
            | "pane.report_metadata"
    )
}

pub(crate) fn api_request_failed(request_id: &str, method: &'static str, err: &str) {
    tracing::warn!(
        event = "api.request.fail",
        subsystem = "api",
        outcome = "error",
        request_id,
        method,
        err,
        "api request failed"
    );
}

// What the API served and what held the app loop (BET-9635). The log above names only the requests that change the screen, and only
// when they start and end, and herdr kept no count of the rest: on 11/16 the loop was held for hundreds of milliseconds at a time and
// 5 of 28 such holds overlapped any logged request. Every request that the connection thread answers is counted here by who sent it
// (the part of its id before the first ':', "luna" for "luna:agent.get") and its method, reads included, and one line a minute says
// how many, how long they took and which kept the loop longest. A request that took 100 ms or the loop for 50 ms is also logged alone.

/// A request that took this long, from its read to its answer written, is logged by itself.
const API_SLOW_REQUEST: Duration = Duration::from_millis(100);
/// A request that kept the app loop this long is logged by itself.
const API_LOOP_HELD: Duration = Duration::from_millis(50);
/// The internal events drained before a request ran for this long are logged by themselves.
const API_BACKLOG_DRAIN: Duration = Duration::from_millis(50);
const API_STATS_WINDOW: Duration = Duration::from_secs(60);
/// Callers and methods are told by the sender, so the table is bounded: beyond this many pairs the rest are counted as "other:other".
const API_STATS_MAX_KEYS: usize = 128;
const API_STATS_TOP: usize = 10;

#[derive(Default, Clone, Copy)]
struct ApiStat {
    served: u64,
    served_total: Duration,
    served_max: Duration,
    held: u64,
    held_total: Duration,
    held_max: Duration,
}

struct ApiStats {
    since: Instant,
    by_key: BTreeMap<String, ApiStat>,
    slow: u64,
    held_over: u64,
}

impl ApiStats {
    fn new(now: Instant) -> Self {
        Self {
            since: now,
            by_key: BTreeMap::new(),
            slow: 0,
            held_over: 0,
        }
    }

    fn stat(&mut self, request_id: &str, method: &str) -> &mut ApiStat {
        let mut key = format!("{}:{method}", api_caller(request_id));
        if !self.by_key.contains_key(&key) && self.by_key.len() >= API_STATS_MAX_KEYS {
            key = "other:other".to_string();
        }
        self.by_key.entry(key).or_default()
    }

    fn record_served(&mut self, request_id: &str, method: &str, elapsed: Duration) {
        let slow = elapsed >= API_SLOW_REQUEST;
        let stat = self.stat(request_id, method);
        stat.served += 1;
        stat.served_total += elapsed;
        stat.served_max = stat.served_max.max(elapsed);
        if slow {
            self.slow += 1;
        }
    }

    fn record_held(&mut self, request_id: &str, method: &str, held: Duration) {
        let over = held >= API_LOOP_HELD;
        let stat = self.stat(request_id, method);
        stat.held += 1;
        stat.held_total += held;
        stat.held_max = stat.held_max.max(held);
        if over {
            self.held_over += 1;
        }
    }

    fn due(&self, now: Instant) -> bool {
        now.duration_since(self.since) >= API_STATS_WINDOW
    }

    /// The pairs that sent the most requests, and the pairs that kept the app loop longest, each as one line of text.
    fn render(&self) -> (String, String) {
        let ms = |d: Duration| d.as_secs_f64() * 1000.0;
        let mut by_count: Vec<_> = self.by_key.iter().filter(|(_, s)| s.served > 0).collect();
        by_count.sort_by(|a, b| b.1.served.cmp(&a.1.served).then(a.0.cmp(b.0)));
        let busiest = by_count
            .iter()
            .take(API_STATS_TOP)
            .map(|(key, s)| {
                format!(
                    "{key}={} (avg {:.1} ms, max {:.0} ms)",
                    s.served,
                    ms(s.served_total) / s.served as f64,
                    ms(s.served_max)
                )
            })
            .collect::<Vec<_>>()
            .join("; ");
        let mut by_held: Vec<_> = self.by_key.iter().filter(|(_, s)| s.held > 0).collect();
        by_held.sort_by(|a, b| b.1.held_total.cmp(&a.1.held_total).then(a.0.cmp(b.0)));
        let longest = by_held
            .iter()
            .take(API_STATS_TOP)
            .map(|(key, s)| {
                format!(
                    "{key}={} on the loop for {:.0} ms in all (max {:.0} ms)",
                    s.held,
                    ms(s.held_total),
                    ms(s.held_max)
                )
            })
            .collect::<Vec<_>>()
            .join("; ");
        (busiest, longest)
    }
}

/// Who sent a request: the part of its id before the first ':' ("luna" for "luna:agent.get", "cli" for "cli:pane:close"). An id with no
/// ':' or a long or empty first part is "other".
fn api_caller(request_id: &str) -> &str {
    match request_id.split_once(':') {
        Some((caller, _)) if !caller.is_empty() && caller.len() <= 24 => caller,
        _ => "other",
    }
}

static API_STATS: Mutex<Option<ApiStats>> = Mutex::new(None);

fn with_api_stats(record: impl FnOnce(&mut ApiStats)) {
    let now = Instant::now();
    let finished = {
        let Ok(mut guard) = API_STATS.lock() else {
            return;
        };
        let stats = guard.get_or_insert_with(|| ApiStats::new(now));
        record(stats);
        if stats.due(now) {
            guard.take()
        } else {
            None
        }
    };
    if let Some(stats) = finished {
        let (busiest, longest) = stats.render();
        let requests: u64 = stats.by_key.values().map(|s| s.served).sum();
        tracing::info!(
            event = "api.stats",
            subsystem = "api",
            window_s = now.duration_since(stats.since).as_secs(),
            requests,
            slow = stats.slow,
            loop_held_over_50ms = stats.held_over,
            busiest = %busiest,
            longest_on_loop = %longest,
            "api requests served in the last minute"
        );
    }
}

/// A thread for an api connection that took this long to begin, from the accept, is logged by itself.
const API_THREAD_START_SLOW: Duration = Duration::from_millis(50);

/// A request the connection thread answered, `elapsed` from its read to its answer written (the wait for the app loop included).
/// `thread_start` is how long the connection's thread took to begin after the accept, and `accept_to_request` how long from the accept to
/// the request line read (the client's own delay between connecting and writing is in it). A request that is slow, or whose thread was
/// slow to begin, is logged with both, so that time lost before the request was read is told from time lost waiting for the app loop.
/// Not called for the requests that wait for something by design (a prompt, an event wait, a subscription).
pub(crate) fn api_request_served(
    request_id: &str,
    method: &'static str,
    elapsed: Duration,
    thread_start: Duration,
    accept_to_request: Duration,
) {
    if elapsed >= API_SLOW_REQUEST || thread_start >= API_THREAD_START_SLOW {
        tracing::warn!(
            event = "api.request.slow",
            subsystem = "api",
            request_id,
            method,
            elapsed_ms = elapsed.as_millis() as u64,
            thread_start_ms = thread_start.as_millis() as u64,
            accept_to_request_ms = accept_to_request.as_millis() as u64,
            "api request slow"
        );
    }
    with_api_stats(|stats| stats.record_served(request_id, method, elapsed));
}

/// A request the app loop handled, `held` from the loop picking it up to its answer sent; nothing else ran on the loop meanwhile.
pub(crate) fn api_loop_held(request_id: &str, method: &'static str, held: Duration) {
    if held >= API_LOOP_HELD {
        tracing::warn!(
            event = "api.loop.held",
            subsystem = "api",
            request_id,
            method,
            held_ms = held.as_millis() as u64,
            "api request held the app loop"
        );
    }
    with_api_stats(|stats| stats.record_held(request_id, method, held));
}

/// The internal events (pane output, state changes) that the loop drains before it runs an API request: every one that is pending, so a
/// request that arrives during a burst waits for the whole backlog.
pub(crate) fn api_backlog_drained(request_id: &str, method: &'static str, drained: Duration) {
    if drained >= API_BACKLOG_DRAIN {
        tracing::warn!(
            event = "api.backlog.drain",
            subsystem = "api",
            request_id,
            method,
            drained_ms = drained.as_millis() as u64,
            "internal events drained before an api request"
        );
    }
}

/// A pass of the app loop that did this much work (everything but the wait for the next event) is logged as `loop.slow`.
const LOOP_SLOW: Duration = Duration::from_millis(100);
/// A phase is named in a `loop.slow` line only from this long, so that the line stays short.
const LOOP_PHASE_NAMED: Duration = Duration::from_millis(20);

/// Where one pass of the app loop spends its time: the loop calls `mark` after each of its phases, `skip` after the wait for the next
/// event, and `finish` at the top of the next pass, which returns the report when the pass did `LOOP_SLOW` of work. A 25 s stall on 11/16
/// had 7 s in named API handlers and 18 s in work that nothing timed (BET-9635); this says which phase it was in.
pub(crate) struct LoopPhases {
    last: std::time::Instant,
    spent: Vec<(&'static str, Duration)>,
}

impl LoopPhases {
    pub(crate) fn new() -> Self {
        Self {
            last: std::time::Instant::now(),
            spent: Vec::with_capacity(16),
        }
    }

    /// Adds the time since the last mark to `label` (the phases of a pass are few, so a linear search does).
    pub(crate) fn mark(&mut self, label: &'static str) {
        let now = std::time::Instant::now();
        let elapsed = now.duration_since(self.last);
        self.last = now;
        match self.spent.iter_mut().find(|(known, _)| *known == label) {
            Some((_, total)) => *total += elapsed,
            None => self.spent.push((label, elapsed)),
        }
    }

    /// Forgets the time since the last mark: it was the wait for the next event, not work.
    pub(crate) fn skip(&mut self) {
        self.last = std::time::Instant::now();
    }

    /// Ends the pass: whatever ran since the last mark is `label`'s. Returns the report when the pass did `LOOP_SLOW` of work, and starts the
    /// next pass either way.
    pub(crate) fn finish(&mut self, label: &'static str) -> Option<String> {
        self.mark(label);
        let report = self.report(LOOP_SLOW);
        self.spent.clear();
        report
    }

    fn report(&self, slow: Duration) -> Option<String> {
        let busy: Duration = self.spent.iter().map(|(_, spent)| *spent).sum();
        if busy < slow {
            return None;
        }
        let mut named: Vec<&(&'static str, Duration)> = self
            .spent
            .iter()
            .filter(|(_, spent)| *spent >= LOOP_PHASE_NAMED)
            .collect();
        named.sort_by(|a, b| b.1.cmp(&a.1));
        let named = named
            .iter()
            .map(|(label, spent)| format!("{label}={}", spent.as_millis()))
            .collect::<Vec<_>>()
            .join(" ");
        Some(format!("busy_ms={} phases=\"{named}\"", busy.as_millis()))
    }
}

/// One slow pass of the app loop, as `LoopPhases::finish` reported it.
pub(crate) fn loop_slow(report: &str) {
    tracing::warn!(
        event = "loop.slow",
        subsystem = "loop",
        report,
        "app loop pass did a lot of work"
    );
}

pub(crate) fn api_wait_started(request_id: &str, pane_id: &str, timeout_ms: Option<u64>) {
    tracing::info!(
        event = "api.wait.start",
        subsystem = "api",
        outcome = "started",
        request_id,
        pane_id,
        timeout_ms,
        "api output wait started"
    );
}

pub(crate) fn api_wait_completed(request_id: &str, pane_id: &str, outcome: &'static str) {
    tracing::info!(
        event = "api.wait.complete",
        subsystem = "api",
        outcome,
        request_id,
        pane_id,
        "api output wait finished"
    );
}

pub(crate) fn api_wait_timed_out(request_id: &str, pane_id: &str) {
    tracing::warn!(
        event = "api.wait.timeout",
        subsystem = "api",
        outcome = "timeout",
        request_id,
        pane_id,
        "api output wait timed out"
    );
}

pub(crate) fn pane_spawn_started(
    pane_id: u32,
    rows: u16,
    cols: u16,
    scrollback_limit_bytes: usize,
) {
    tracing::info!(
        event = "pane.spawn.start",
        subsystem = "pane",
        outcome = "started",
        pane_id,
        rows,
        cols,
        scrollback_limit_bytes,
        "spawning pane terminal"
    );
}

pub(crate) fn pane_spawned(pane_id: u32, pid: u32) {
    tracing::info!(
        event = "pane.spawned",
        subsystem = "pane",
        outcome = "ok",
        pane_id,
        pid,
        "pane child spawned"
    );
}

pub(crate) fn pane_exited(pane_id: u32, status: &str) {
    tracing::info!(
        event = "pane.exit",
        subsystem = "pane",
        outcome = "completed",
        pane_id,
        status,
        "pane child exited"
    );
}

pub(crate) fn pane_exit_failed(pane_id: u32, err: &str) {
    tracing::error!(
        event = "pane.exit",
        subsystem = "pane",
        outcome = "error",
        pane_id,
        err,
        "pane child wait failed"
    );
}

pub(crate) fn workspace_created(workspace_id: &str, root_pane_id: u32) {
    tracing::info!(
        event = "workspace.create",
        subsystem = "workspace",
        outcome = "ok",
        workspace_id,
        pane_id = root_pane_id,
        "workspace created"
    );
}

pub(crate) fn workspace_focused(workspace_id: &str) {
    tracing::info!(
        event = "workspace.focus",
        subsystem = "workspace",
        outcome = "ok",
        workspace_id,
        "workspace focused"
    );
}

pub(crate) fn workspace_closed(workspace_id: &str) {
    tracing::info!(
        event = "workspace.close",
        subsystem = "workspace",
        outcome = "ok",
        workspace_id,
        "workspace closed"
    );
}

pub(crate) fn workspace_renamed(workspace_id: &str) {
    tracing::info!(
        event = "workspace.rename",
        subsystem = "workspace",
        outcome = "ok",
        workspace_id,
        "workspace renamed"
    );
}

pub(crate) fn tab_focused(workspace_id: &str, tab_id: &str) {
    tracing::info!(
        event = "tab.focus",
        subsystem = "tab",
        outcome = "ok",
        workspace_id,
        tab_id,
        "tab focused"
    );
}

#[cfg(test)]
pub(crate) fn tab_closed(workspace_id: &str, tab_id: &str) {
    tracing::info!(
        event = "tab.close",
        subsystem = "tab",
        outcome = "ok",
        workspace_id,
        tab_id,
        "tab closed"
    );
}

pub(crate) fn tab_renamed(workspace_id: &str, tab_id: &str) {
    tracing::info!(
        event = "tab.rename",
        subsystem = "tab",
        outcome = "ok",
        workspace_id,
        tab_id,
        "tab renamed"
    );
}

pub(crate) fn session_saved(path: &Path, workspaces: usize) {
    tracing::info!(
        event = "persist.save",
        subsystem = "persist",
        outcome = "ok",
        path = %path.display(),
        workspaces,
        "session saved"
    );
}

pub(crate) fn session_save_failed(path: &Path, err: &str) {
    tracing::error!(
        event = "persist.save",
        subsystem = "persist",
        outcome = "error",
        path = %path.display(),
        err,
        "failed to save session"
    );
}

pub(crate) fn session_cleared(path: &Path) {
    tracing::info!(
        event = "persist.clear",
        subsystem = "persist",
        outcome = "ok",
        path = %path.display(),
        "session cleared"
    );
}

pub(crate) fn session_clear_failed(path: &Path, err: &str) {
    tracing::error!(
        event = "persist.clear",
        subsystem = "persist",
        outcome = "error",
        path = %path.display(),
        err,
        "failed to clear session"
    );
}

pub(crate) fn session_restored(workspaces: usize, outcome: &'static str) {
    tracing::info!(
        event = "persist.restore",
        subsystem = "persist",
        outcome,
        workspaces,
        "session restore evaluated"
    );
}

pub(crate) fn update_check_started() {
    tracing::info!(
        event = "update.check.start",
        subsystem = "update",
        outcome = "started",
        "checking for updates"
    );
}

pub(crate) fn update_check_failed(err: &str) {
    tracing::warn!(
        event = "update.check.complete",
        subsystem = "update",
        outcome = "error",
        err,
        "update check failed"
    );
}

pub(crate) fn update_available(version: &str) {
    tracing::info!(
        event = "update.available",
        subsystem = "update",
        outcome = "ok",
        version,
        "update available"
    );
}

pub(crate) fn integration_action(
    action: &'static str,
    target: &'static str,
    outcome: &'static str,
) {
    tracing::info!(
        event = "integration.action",
        subsystem = "integration",
        outcome,
        action,
        target,
        "integration action finished"
    );
}

struct RotatingFileMakeWriter {
    state: Arc<Mutex<RotatingFileState>>,
}

impl RotatingFileMakeWriter {
    fn new(
        dir: PathBuf,
        file_name: &str,
        max_bytes: u64,
        retained_files: usize,
    ) -> io::Result<Self> {
        fs::create_dir_all(&dir)?;
        let path = dir.join(file_name);
        let mut state = RotatingFileState {
            path,
            max_bytes,
            retained_files,
            file: None,
            current_size: 0,
            disabled: false,
        };
        state.open_current_file()?;
        Ok(Self {
            state: Arc::new(Mutex::new(state)),
        })
    }
}

impl<'a> MakeWriter<'a> for RotatingFileMakeWriter {
    type Writer = RotatingFileGuard;

    fn make_writer(&'a self) -> Self::Writer {
        RotatingFileGuard {
            state: Arc::clone(&self.state),
        }
    }
}

struct RotatingFileGuard {
    state: Arc<Mutex<RotatingFileState>>,
}

impl Write for RotatingFileGuard {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let Ok(mut state) = self.state.lock() else {
            return Ok(buf.len());
        };
        if state.disabled {
            return Ok(buf.len());
        }
        if state.rotate_if_needed(buf.len() as u64).is_err() {
            state.disabled = true;
            return Ok(buf.len());
        }
        if let Some(file) = state.file.as_mut() {
            match file.write(buf) {
                Ok(written) => {
                    state.current_size = state.current_size.saturating_add(written as u64);
                    Ok(written)
                }
                Err(_) => {
                    state.disabled = true;
                    Ok(buf.len())
                }
            }
        } else {
            Ok(buf.len())
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        let Ok(mut state) = self.state.lock() else {
            return Ok(());
        };
        if state.disabled {
            return Ok(());
        }
        match state.file.as_mut() {
            Some(file) => match file.flush() {
                Ok(()) => Ok(()),
                Err(_) => {
                    state.disabled = true;
                    Ok(())
                }
            },
            None => Ok(()),
        }
    }
}

struct RotatingFileState {
    path: PathBuf,
    max_bytes: u64,
    retained_files: usize,
    file: Option<File>,
    current_size: u64,
    disabled: bool,
}

impl RotatingFileState {
    fn rotate_if_needed(&mut self, incoming_len: u64) -> io::Result<()> {
        if self.file.is_none() {
            self.open_current_file()?;
        }
        if self.max_bytes == 0 || self.current_size.saturating_add(incoming_len) <= self.max_bytes {
            return Ok(());
        }
        self.rotate_files()?;
        self.open_current_file()
    }

    fn open_current_file(&mut self) -> io::Result<()> {
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        self.current_size = file.metadata().map(|meta| meta.len()).unwrap_or(0);
        self.file = Some(file);
        Ok(())
    }

    fn rotate_files(&mut self) -> io::Result<()> {
        self.file.take();
        if self.retained_files == 0 {
            match fs::remove_file(&self.path) {
                Ok(()) => {}
                Err(err) if err.kind() == io::ErrorKind::NotFound => {}
                Err(err) => return Err(err),
            }
            self.current_size = 0;
            return Ok(());
        }

        let oldest = rotated_log_path(&self.path, self.retained_files);
        match fs::remove_file(&oldest) {
            Ok(()) => {}
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) => return Err(err),
        }

        for index in (1..=self.retained_files).rev() {
            let source = if index == 1 {
                self.path.clone()
            } else {
                rotated_log_path(&self.path, index - 1)
            };
            let target = rotated_log_path(&self.path, index);
            if !source.exists() {
                continue;
            }
            fs::rename(source, target)?;
        }

        self.current_size = 0;
        Ok(())
    }
}

fn rotated_log_path(path: &Path, index: usize) -> PathBuf {
    let suffix = format!(".{}", index);
    let file_name = path
        .file_name()
        .map(|name| {
            let mut name = name.to_os_string();
            name.push(&suffix);
            name
        })
        .unwrap_or_else(|| suffix.clone().into());
    path.with_file_name(file_name)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_log_path(name: &str) -> PathBuf {
        let unique = format!(
            "herdr-logging-tests-{}-{}-{}",
            name,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        std::env::temp_dir().join(unique).join("herdr.log")
    }

    #[test]
    fn rotated_log_path_appends_numeric_suffix() {
        let path = PathBuf::from("/tmp/herdr.log");
        assert_eq!(
            rotated_log_path(&path, 2),
            PathBuf::from("/tmp/herdr.log.2")
        );
    }

    #[test]
    fn rotate_files_shifts_existing_generations() {
        let path = temp_log_path("rotate");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "current").unwrap();
        fs::write(rotated_log_path(&path, 1), "older").unwrap();

        let mut state = RotatingFileState {
            path: path.clone(),
            max_bytes: 128,
            retained_files: 2,
            file: None,
            current_size: 0,
            disabled: false,
        };
        state.rotate_files().unwrap();

        assert_eq!(
            fs::read_to_string(rotated_log_path(&path, 1)).unwrap(),
            "current"
        );
        assert_eq!(
            fs::read_to_string(rotated_log_path(&path, 2)).unwrap(),
            "older"
        );
        assert!(!path.exists());

        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn write_replaces_log_without_retained_files_when_size_limit_is_reached() {
        let path = temp_log_path("replace");
        let dir = path.parent().unwrap().to_path_buf();
        fs::create_dir_all(&dir).unwrap();

        let writer = RotatingFileMakeWriter::new(dir.clone(), "herdr.log", 8, 0).unwrap();
        {
            let mut guard = writer.make_writer();
            guard.write_all(b"12345678").unwrap();
            guard.write_all(b"abc").unwrap();
            guard.flush().unwrap();
        }

        assert_eq!(fs::read_to_string(&path).unwrap(), "abc");
        assert!(!rotated_log_path(&path, 1).exists());

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn api_caller_is_the_part_of_the_id_before_the_first_colon() {
        assert_eq!(api_caller("luna:agent.get"), "luna");
        assert_eq!(api_caller("cli:pane:close"), "cli");
        assert_eq!(api_caller("prober"), "other");
        assert_eq!(api_caller(":x"), "other");
        assert_eq!(
            api_caller("a-caller-with-a-very-long-name-indeed:x"),
            "other"
        );
    }

    #[test]
    fn api_stats_count_each_caller_and_method_and_name_the_longest_on_the_loop() {
        let mut stats = ApiStats::new(Instant::now());
        for _ in 0..3 {
            stats.record_served("luna:1", "agent.get", Duration::from_millis(2));
        }
        stats.record_served("cli:2", "pane.close", Duration::from_millis(900));
        stats.record_held("cli:2", "pane.close", Duration::from_millis(870));
        stats.record_held("luna:1", "agent.get", Duration::from_millis(3));

        assert_eq!(stats.slow, 1, "only the 900 ms request is slow");
        assert_eq!(stats.held_over, 1, "only the 870 ms hold is over 50 ms");
        let (busiest, longest) = stats.render();
        assert!(
            busiest.starts_with("luna:agent.get=3 (avg 2.0 ms, max 2 ms)"),
            "{busiest}"
        );
        assert!(busiest.contains("cli:pane.close=1 (avg 900.0 ms, max 900 ms)"));
        assert!(
            longest.starts_with("cli:pane.close=1 on the loop for 870 ms in all (max 870 ms)"),
            "{longest}"
        );
    }

    #[test]
    fn api_stats_window_is_due_after_a_minute_and_the_table_is_bounded() {
        let start = Instant::now();
        let mut stats = ApiStats::new(start);
        assert!(!stats.due(start + Duration::from_secs(59)));
        assert!(stats.due(start + Duration::from_secs(60)));

        for i in 0..(API_STATS_MAX_KEYS + 40) {
            stats.record_served(&format!("c{i}:x"), "ping", Duration::ZERO);
        }
        assert_eq!(
            stats.by_key.len(),
            API_STATS_MAX_KEYS + 1,
            "the rest fold into other:other"
        );
        assert_eq!(stats.by_key["other:other"].served, 40);
    }

    #[test]
    fn loop_phases_name_the_phase_that_took_the_time_and_add_up_a_repeated_one() {
        let mut phases = LoopPhases::new();
        phases.mark("render_signal");
        std::thread::sleep(Duration::from_millis(70));
        phases.mark("scheduled_tasks");
        std::thread::sleep(Duration::from_millis(70));
        phases.mark("scheduled_tasks");
        let report = phases
            .finish("event:api")
            .expect("140 ms of work is a slow pass");
        assert!(report.starts_with("busy_ms=1"), "{report}");
        assert!(report.contains("scheduled_tasks=1"), "{report}");
        assert!(
            !report.contains("render_signal"),
            "a phase under {LOOP_PHASE_NAMED:?} is not named: {report}"
        );
        assert!(
            phases.finish("event:api").is_none(),
            "a finished pass starts the next one from nothing"
        );
    }

    #[test]
    fn loop_phases_do_not_count_the_wait_for_the_next_event() {
        let mut phases = LoopPhases::new();
        phases.mark("render");
        std::thread::sleep(Duration::from_millis(150));
        phases.skip();
        assert!(
            phases.finish("event:timer").is_none(),
            "150 ms of waiting is not work"
        );
    }
}
