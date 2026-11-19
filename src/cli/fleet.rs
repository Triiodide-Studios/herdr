//! Fleet commands: `herdr resolve`, `herdr broadcast`, the compact `agent list` and the multi-target `agent wait`.
//!
//! Each one replaces a loop agents ran by hand (one message per session, a 28 KB agent list grepped for a name, a
//! health poll per turn). They are built on the existing `agent.list`, `agent.prompt` and `agent.wait` requests, so the
//! server is unchanged.

use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Duration;

use crate::api::schema::{AgentPromptParams, AgentWaitParams, EmptyParams, Method, Request};

const BRIEF_NAME_WIDTH: usize = 40;

/// A Claude Code session file from `~/.claude/sessions/*.json`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ChatSession {
    pub session_id: String,
    pub name: Option<String>,
    pub pipe: Option<String>,
    pub status: Option<String>,
}

/// The fields of one `agent.list` entry the fleet commands use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct AgentRow {
    pub pane_id: String,
    pub status: String,
    pub name: Option<String>,
    pub display: Option<String>,
    pub title: Option<String>,
    pub cwd: Option<String>,
    pub session_id: Option<String>,
    pub agent: Option<String>,
}

impl AgentRow {
    pub(super) fn from_value(value: &serde_json::Value) -> Option<Self> {
        let text = |key: &str| value[key].as_str().map(str::to_owned);
        Some(Self {
            pane_id: text("pane_id")?,
            status: text("agent_status").unwrap_or_else(|| "unknown".into()),
            name: text("name"),
            display: text("display_agent"),
            title: text("title"),
            cwd: text("cwd"),
            session_id: value["agent_session"]["value"].as_str().map(str::to_owned),
            agent: text("agent"),
        })
    }

    pub(super) fn label(&self) -> &str {
        self.name
            .as_deref()
            .or(self.display.as_deref())
            .or(self.title.as_deref())
            .unwrap_or("-")
    }

    pub(super) fn worktree(&self) -> String {
        worktree_of(self.cwd.as_deref().unwrap_or_default())
    }

    fn chat<'a>(&self, sessions: &'a [ChatSession]) -> Option<&'a ChatSession> {
        let id = self.session_id.as_deref()?;
        sessions.iter().find(|session| session.session_id == id)
    }
}

/// `G:\lorefs\worktrees\<name>\BET` is worktree `<name>`; any other directory is the main checkout.
pub(super) fn worktree_of(cwd: &str) -> String {
    let normalized = cwd.replace('\\', "/");
    let mut parts = normalized.split('/').filter(|part| !part.is_empty());
    while let Some(part) = parts.next() {
        if part.eq_ignore_ascii_case("worktrees") {
            if let Some(name) = parts.next() {
                return name.to_owned();
            }
        }
    }
    "main".into()
}

pub(super) fn parse_agent_rows(response: &serde_json::Value) -> Vec<AgentRow> {
    response["result"]["agents"]
        .as_array()
        .map(|agents| agents.iter().filter_map(AgentRow::from_value).collect())
        .unwrap_or_default()
}

pub(crate) fn sessions_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("HERDR_CLAUDE_SESSIONS_DIR") {
        return Some(PathBuf::from(dir));
    }
    let home = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME"))?;
    Some(Path::new(&home).join(".claude").join("sessions"))
}

pub(super) fn read_chat_sessions(dir: &Path) -> Vec<ChatSession> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut sessions = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
            continue;
        }
        let Ok(content) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&content) else {
            continue;
        };
        let text = |key: &str| value[key].as_str().map(str::to_owned);
        let Some(session_id) = text("sessionId") else {
            continue;
        };
        sessions.push(ChatSession {
            session_id,
            name: text("name"),
            pipe: text("messagingSocketPath"),
            status: text("status"),
        });
    }
    sessions
}

fn eq(a: Option<&str>, query: &str) -> bool {
    a.is_some_and(|a| a.eq_ignore_ascii_case(query))
}

fn contains(a: Option<&str>, query: &str) -> bool {
    a.is_some_and(|a| a.to_lowercase().contains(query))
}

/// The agents `query` names, best tier first: an exact name, pane id, chat name or session id; then a session-id
/// prefix or a name prefix; then a substring of a name or title. A query that matches nothing returns no rows.
pub(super) fn resolve_query<'a>(
    query: &str,
    rows: &'a [AgentRow],
    sessions: &[ChatSession],
) -> Vec<&'a AgentRow> {
    let lower = query.to_lowercase();
    let tiers: [&dyn Fn(&AgentRow) -> bool; 3] = [
        &|row| {
            eq(row.name.as_deref(), query)
                || eq(row.display.as_deref(), query)
                || eq(Some(&row.pane_id), query)
                || eq(row.session_id.as_deref(), query)
                || eq(row.chat(sessions).and_then(|c| c.name.as_deref()), query)
        },
        &|row| {
            (lower.len() >= 6
                && row
                    .session_id
                    .as_deref()
                    .is_some_and(|id| id.to_lowercase().starts_with(&lower)))
                || [row.name.as_deref(), row.display.as_deref()]
                    .into_iter()
                    .flatten()
                    .any(|name| name.to_lowercase().starts_with(&lower))
        },
        &|row| {
            contains(row.name.as_deref(), &lower)
                || contains(row.display.as_deref(), &lower)
                || contains(row.title.as_deref(), &lower)
        },
    ];
    for tier in tiers {
        let hits: Vec<&AgentRow> = rows.iter().filter(|row| tier(row)).collect();
        if !hits.is_empty() {
            return hits;
        }
    }
    Vec::new()
}

fn truncate(text: &str, width: usize) -> String {
    if text.chars().count() <= width {
        return text.to_owned();
    }
    let mut cut: String = text.chars().take(width.saturating_sub(1)).collect();
    cut.push('…');
    cut
}

/// `w1:p4 working ps5-gpu-hillclimb main`
pub(super) fn brief_line(row: &AgentRow) -> String {
    format!(
        "{} {} {} {}",
        row.pane_id,
        row.status,
        truncate(row.label(), BRIEF_NAME_WIDTH),
        row.worktree()
    )
}

/// `w1:p4 working name=x chat=bet-a8 worktree=main cwd=... session=<id> pipe=<path>`
pub(super) fn resolve_line(row: &AgentRow, sessions: &[ChatSession]) -> String {
    let chat = row.chat(sessions);
    let mut line = format!("{} {} name={}", row.pane_id, row.status, row.label());
    if let Some(name) = chat.and_then(|chat| chat.name.as_deref()) {
        line.push_str(&format!(" chat={name}"));
    }
    line.push_str(&format!(" worktree={}", row.worktree()));
    if let Some(cwd) = row.cwd.as_deref() {
        line.push_str(&format!(" cwd={cwd}"));
    }
    if let Some(id) = row.session_id.as_deref() {
        line.push_str(&format!(" session={id}"));
    }
    if let Some(pipe) = chat.and_then(|chat| chat.pipe.as_deref()) {
        line.push_str(&format!(" pipe={pipe}"));
    }
    line
}

/// One broadcast target, or the reason a name in `--to` could not be turned into one.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Target {
    Pane(String),
    Unresolved { query: String, reason: &'static str },
}

/// `all-active` is every agent with a Claude session that is not `unknown`, except the sender's own pane; otherwise
/// `to` is a comma-separated list of names, pane ids, chat names or session ids that each name one agent.
pub(super) fn select_targets(
    to: &str,
    rows: &[AgentRow],
    sessions: &[ChatSession],
    own_pane: Option<&str>,
) -> Vec<Target> {
    if to == "all-active" || to == "all" {
        return rows
            .iter()
            .filter(|row| row.session_id.is_some() && row.status != "unknown")
            .filter(|row| Some(row.pane_id.as_str()) != own_pane)
            .map(|row| Target::Pane(row.pane_id.clone()))
            .collect();
    }
    let mut targets: Vec<Target> = Vec::new();
    for query in to.split(',').map(str::trim).filter(|q| !q.is_empty()) {
        let hits = resolve_query(query, rows, sessions);
        let target = match hits.as_slice() {
            [] => Target::Unresolved {
                query: query.to_owned(),
                reason: "not_found",
            },
            [row] => Target::Pane(row.pane_id.clone()),
            _ => Target::Unresolved {
                query: query.to_owned(),
                reason: "ambiguous",
            },
        };
        if !targets.contains(&target) {
            targets.push(target);
        }
    }
    targets
}

/// The text to send: the contents of the file when `arg` names one, otherwise `arg` itself.
pub(super) fn broadcast_text(arg: &str) -> std::io::Result<String> {
    if Path::new(arg).is_file() {
        return Ok(std::fs::read_to_string(arg)?.trim_end().to_owned());
    }
    Ok(arg.to_owned())
}

fn list_rows() -> std::io::Result<Result<Vec<AgentRow>, serde_json::Value>> {
    let response = super::send_request(&Request {
        id: "cli:fleet:list".into(),
        method: Method::AgentList(EmptyParams::default()),
    })?;
    if response.get("error").is_some() {
        return Ok(Err(response));
    }
    Ok(Ok(parse_agent_rows(&response)))
}

fn current_sessions() -> Vec<ChatSession> {
    sessions_dir()
        .map(|dir| read_chat_sessions(&dir))
        .unwrap_or_default()
}

/// `herdr agent list` prints the same JSON as ever; `--brief` prints one short line per agent instead.
pub(super) fn agent_list_brief() -> std::io::Result<i32> {
    let response = super::send_request(&Request {
        id: "cli:agent:list".into(),
        method: Method::AgentList(EmptyParams::default()),
    })?;
    if response.get("error").is_some() {
        return super::print_response(&response);
    }
    for row in parse_agent_rows(&response) {
        println!("{}", brief_line(&row));
    }
    Ok(0)
}

pub(super) fn run_resolve_command(args: &[String]) -> std::io::Result<i32> {
    let [query] = args else {
        eprintln!("usage: herdr resolve <name|pane|chat|session-id>");
        return Ok(2);
    };
    if matches!(query.as_str(), "help" | "--help" | "-h") {
        eprintln!("usage: herdr resolve <name|pane|chat|session-id>");
        return Ok(0);
    }
    let rows = match list_rows()? {
        Ok(rows) => rows,
        Err(error) => return super::print_response(&error),
    };
    let sessions = current_sessions();
    let hits = resolve_query(query, &rows, &sessions);
    match hits.as_slice() {
        [] => {
            eprintln!("no agent matches {query}");
            Ok(3)
        }
        [row] => {
            println!("{}", resolve_line(row, &sessions));
            Ok(0)
        }
        many => {
            for row in many {
                println!("{}", resolve_line(row, &sessions));
            }
            eprintln!("{} agents match {query}", many.len());
            Ok(4)
        }
    }
}

pub(super) fn run_broadcast_command(args: &[String]) -> std::io::Result<i32> {
    const USAGE: &str = "usage: herdr broadcast --to <names|all-active> <text|file> [--dry-run]";
    let mut to = None;
    let mut text = None;
    let mut dry_run = false;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--to" => {
                let Some(value) = args.get(index + 1) else {
                    eprintln!("missing value for --to\n{USAGE}");
                    return Ok(2);
                };
                to = Some(value.clone());
                index += 2;
            }
            "--dry-run" => {
                dry_run = true;
                index += 1;
            }
            "help" | "--help" | "-h" => {
                eprintln!("{USAGE}");
                return Ok(0);
            }
            value if value.starts_with("--") => {
                eprintln!("unknown option: {value}\n{USAGE}");
                return Ok(2);
            }
            value => {
                if text.is_some() {
                    eprintln!("{USAGE}");
                    return Ok(2);
                }
                text = Some(value.to_owned());
                index += 1;
            }
        }
    }
    let (Some(to), Some(text)) = (to, text) else {
        eprintln!("{USAGE}");
        return Ok(2);
    };
    let text = broadcast_text(&text)?;
    let rows = match list_rows()? {
        Ok(rows) => rows,
        Err(error) => return super::print_response(&error),
    };
    let sessions = current_sessions();
    let own_pane = std::env::var("HERDR_PANE_ID").ok();
    let targets = select_targets(&to, &rows, &sessions, own_pane.as_deref());
    if targets.is_empty() {
        eprintln!("no agents match --to {to}");
        return Ok(3);
    }
    let mut failed = 0;
    for target in &targets {
        match target {
            Target::Unresolved { query, reason } => {
                failed += 1;
                println!("{query} {reason}");
            }
            Target::Pane(pane) if dry_run => println!("{pane} would_send"),
            Target::Pane(pane) => {
                let response = super::send_request_unchecked(&Request {
                    id: "cli:broadcast".into(),
                    method: Method::AgentPrompt(AgentPromptParams {
                        target: pane.clone(),
                        text: text.clone(),
                        wait: None,
                    }),
                })?;
                match response["error"]["code"].as_str() {
                    None => println!("{pane} ok"),
                    Some(code) => {
                        failed += 1;
                        println!("{pane} {code}");
                    }
                }
            }
        }
    }
    println!("{}/{} sent", targets.len() - failed, targets.len());
    Ok(i32::from(failed > 0))
}

/// `herdr agent wait <target>... [--any-of a,b | --until a]... [--timeout MS] [--brief]`
///
/// With one target and no `--brief` this prints the single `agent.wait` JSON as before. Otherwise every target waits on
/// its own thread and the first to reach a status prints one line: `<pane> <status> <name>`.
pub(super) fn agent_wait(
    targets: Vec<String>,
    params: AgentWaitParams,
    brief: bool,
) -> std::io::Result<i32> {
    let AgentWaitParams {
        until, timeout_ms, ..
    } = params;
    let (sender, receiver) = mpsc::channel();
    for target in targets.iter().cloned() {
        let sender = sender.clone();
        let until = until.clone();
        std::thread::spawn(move || {
            let response = super::send_request(&Request {
                id: "cli:agent:wait".into(),
                method: Method::AgentWait(AgentWaitParams {
                    target: target.clone(),
                    until,
                    timeout_ms,
                }),
            });
            let _ = sender.send((target, response));
        });
    }
    drop(sender);
    let mut last_error = None;
    let mut remaining = targets.len();
    while remaining > 0 {
        let Ok((target, response)) = receiver.recv_timeout(Duration::from_secs(3600)) else {
            break;
        };
        remaining -= 1;
        let response = response?;
        if response.get("error").is_some() {
            last_error = Some(response);
            continue;
        }
        if !brief && targets.len() == 1 {
            return super::print_response(&response);
        }
        let agent = &response["result"]["agent"];
        let row = AgentRow::from_value(agent);
        match row {
            Some(row) => println!("{} {} {}", row.pane_id, row.status, row.label()),
            None => println!("{target} matched"),
        }
        return Ok(0);
    }
    match last_error {
        Some(error) => super::print_response(&error),
        None => Ok(1),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(pane: &str, status: &str, name: &str, cwd: &str, session: &str) -> serde_json::Value {
        serde_json::json!({
            "pane_id": pane, "agent_status": status, "display_agent": name, "cwd": cwd,
            "agent": "claude", "title": format!("title of {name}"),
            "agent_session": {"source": "herdr:claude", "agent": "claude", "kind": "id", "value": session},
        })
    }

    fn fixture() -> (Vec<AgentRow>, Vec<ChatSession>) {
        let list = serde_json::json!({"result": {"agents": [
            row("w1:p4", "working", "ps5-gpu-hillclimb", "G:\\BET\\blake_trws_blake_main\\BET", "aaaaaaaa-1111"),
            row("w1:p5", "idle", "ps5-gpu-notes", "G:\\lorefs\\worktrees\\c3-notes\\BET", "bbbbbbbb-2222"),
            row("w2:p1", "unknown", "shell-ish", "C:\\x", "cccccccc-3333"),
        ]}});
        let mut rows = parse_agent_rows(&list);
        rows[2].session_id = None;
        let sessions = vec![ChatSession {
            session_id: "aaaaaaaa-1111".into(),
            name: Some("bet-a8".into()),
            pipe: Some("\\\\.\\pipe\\LOCAL\\cc-msg-0b1c".into()),
            status: Some("busy".into()),
        }];
        (rows, sessions)
    }

    #[test]
    fn worktree_comes_from_the_lorefs_path() {
        assert_eq!(
            worktree_of("G:\\lorefs\\worktrees\\mp-floor\\BET"),
            "mp-floor"
        );
        assert_eq!(worktree_of("G:\\BET\\blake_trws_blake_main\\BET"), "main");
    }

    #[test]
    fn exact_name_beats_a_longer_name_that_contains_it() {
        let (rows, sessions) = fixture();
        let hits = resolve_query("ps5-gpu-hillclimb", &rows, &sessions);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].pane_id, "w1:p4");
        assert_eq!(resolve_query("ps5-gpu", &rows, &sessions).len(), 2);
        assert!(resolve_query("nothing", &rows, &sessions).is_empty());
    }

    #[test]
    fn a_chat_name_finds_the_pane_through_the_session_id() {
        let (rows, sessions) = fixture();
        let hits = resolve_query("BET-A8", &rows, &sessions);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].pane_id, "w1:p4");
        assert_eq!(
            resolve_query("aaaaaaaa", &rows, &sessions)[0].pane_id,
            "w1:p4"
        );
    }

    #[test]
    fn resolve_line_carries_pane_status_worktree_and_pipe() {
        let (rows, sessions) = fixture();
        let line = resolve_line(&rows[0], &sessions);
        assert_eq!(
            line,
            "w1:p4 working name=ps5-gpu-hillclimb chat=bet-a8 worktree=main \
             cwd=G:\\BET\\blake_trws_blake_main\\BET session=aaaaaaaa-1111 pipe=\\\\.\\pipe\\LOCAL\\cc-msg-0b1c"
        );
    }

    #[test]
    fn brief_line_is_short() {
        let (rows, _) = fixture();
        assert_eq!(brief_line(&rows[1]), "w1:p5 idle ps5-gpu-notes c3-notes");
    }

    #[test]
    fn all_active_skips_unknown_agents_and_the_senders_own_pane() {
        let (rows, sessions) = fixture();
        assert_eq!(
            select_targets("all-active", &rows, &sessions, Some("w1:p4")),
            vec![Target::Pane("w1:p5".into())]
        );
    }

    #[test]
    fn named_targets_report_each_name_that_does_not_resolve() {
        let (rows, sessions) = fixture();
        let targets = select_targets("bet-a8, ps5-gpu, nothing, w1:p4", &rows, &sessions, None);
        assert_eq!(
            targets,
            vec![
                Target::Pane("w1:p4".into()),
                Target::Unresolved {
                    query: "ps5-gpu".into(),
                    reason: "ambiguous"
                },
                Target::Unresolved {
                    query: "nothing".into(),
                    reason: "not_found"
                },
            ]
        );
    }

    #[test]
    fn chat_sessions_are_read_from_the_sessions_directory() {
        let dir = std::env::temp_dir().join(format!("herdr-fleet-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("1.json"),
            r#"{"sessionId":"s-1","name":"bet-1","messagingSocketPath":"\\\\.\\pipe\\p","status":"idle"}"#,
        )
        .unwrap();
        std::fs::write(dir.join("1.key"), "not json").unwrap();
        std::fs::write(dir.join("bad.json"), "{").unwrap();
        let sessions = read_chat_sessions(&dir);
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].pipe.as_deref(), Some("\\\\.\\pipe\\p"));
    }

    #[test]
    fn broadcast_text_reads_a_file_when_the_argument_names_one() {
        let path = std::env::temp_dir().join(format!("herdr-bcast-{}.txt", std::process::id()));
        std::fs::write(&path, "hello fleet\n").unwrap();
        let from_file = broadcast_text(path.to_str().unwrap()).unwrap();
        std::fs::remove_file(&path).unwrap();
        assert_eq!(from_file, "hello fleet");
        assert_eq!(broadcast_text("plain text").unwrap(), "plain text");
    }

    #[test]
    fn every_agent_in_the_list_resolves_to_itself_by_pane_id_and_by_name() {
        let (rows, sessions) = fixture();
        for row in &rows {
            let by_pane = resolve_query(&row.pane_id, &rows, &sessions);
            assert_eq!(by_pane.len(), 1, "{}", row.pane_id);
            assert_eq!(by_pane[0].pane_id, row.pane_id);
            if let Some(name) = row.display.as_deref() {
                assert!(resolve_query(name, &rows, &sessions)
                    .iter()
                    .any(|hit| hit.pane_id == row.pane_id));
            }
        }
    }

    #[test]
    fn brief_keeps_pane_status_and_name_of_every_agent() {
        let (rows, _) = fixture();
        for row in &rows {
            let line = brief_line(row);
            assert!(line.starts_with(&format!("{} {} ", row.pane_id, row.status)));
            assert!(line.contains(row.label()));
        }
    }
}
