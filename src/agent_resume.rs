use std::path::Path;

use serde::{Deserialize, Serialize};

const MAX_SESSION_ID_LEN: usize = 512;
const MAX_SESSION_PATH_LEN: usize = 4096;
const MAX_RESUME_ARGS: usize = 64;
const MAX_RESUME_ARGV_BYTES: usize = 8192;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentSessionRef {
    pub kind: AgentSessionRefKind,
    pub value: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AgentSessionRefKind {
    Id,
    Path,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentResumePlan {
    pub agent: String,
    pub argv: Vec<String>,
    pub dedupe_key: String,
}

/// A resume command reported by the agent itself, run in the restored pane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReportedAgentResume {
    pub source: String,
    pub agent: String,
    pub argv: Vec<String>,
}

impl ReportedAgentResume {
    /// The same command can name different sessions in different directories,
    /// for example `agent --continue`, so the directory is part of its identity.
    pub fn plan(&self, cwd: &Path) -> AgentResumePlan {
        AgentResumePlan {
            agent: self.agent.clone(),
            argv: self.argv.clone(),
            dedupe_key: format!(
                "{}\u{0}{}\u{0}{}\u{0}argv\u{0}{}",
                self.source,
                self.agent,
                cwd.display(),
                self.argv.join("\u{0}")
            ),
        }
    }
}

/// Restore types the command into the pane's shell, so the executable must be a
/// bare command name: shells disagree on how to invoke a quoted path.
pub fn validate_resume_argv(argv: &[String]) -> Result<(), String> {
    let Some(command) = argv.first() else {
        return Err("resume_argv must not be empty".into());
    };
    if argv.len() > MAX_RESUME_ARGS {
        return Err(format!(
            "resume_argv allows at most {MAX_RESUME_ARGS} arguments"
        ));
    }
    if argv.iter().map(String::len).sum::<usize>() > MAX_RESUME_ARGV_BYTES {
        return Err(format!(
            "resume_argv allows at most {MAX_RESUME_ARGV_BYTES} bytes"
        ));
    }
    if argv.iter().any(|arg| arg.chars().any(char::is_control)) {
        return Err("resume_argv must not contain control characters".into());
    }
    // Restore quotes arguments POSIX-style, which PowerShell reads differently
    // only when an argument itself contains an apostrophe.
    if argv.iter().any(|arg| arg.contains('\'')) {
        return Err("resume_argv must not contain apostrophes".into());
    }
    let plain_command = !command.is_empty()
        && !command.starts_with('-')
        && command
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'));
    if !plain_command {
        return Err("resume_argv must start with a plain command name, not a path".into());
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PersistedAgentSession {
    pub source: String,
    pub agent: String,
    pub session_ref: AgentSessionRef,
}

impl AgentSessionRef {
    pub fn id(value: impl Into<String>) -> Option<Self> {
        let value = value.into();
        valid_session_id(&value).then_some(Self {
            kind: AgentSessionRefKind::Id,
            value,
        })
    }

    pub fn path(value: impl Into<String>) -> Option<Self> {
        let value = value.into();
        valid_session_path(&value).then_some(Self {
            kind: AgentSessionRefKind::Path,
            value,
        })
    }
}

pub fn session_ref_from_report(
    source: &str,
    agent: &str,
    agent_session_id: Option<String>,
    _agent_session_path: Option<String>,
) -> Option<AgentSessionRef> {
    if !is_official_agent_source(source, agent) {
        return None;
    }

    if agent == "pi" || agent == "omp" {
        return _agent_session_path
            .and_then(AgentSessionRef::path)
            .or_else(|| agent_session_id.and_then(AgentSessionRef::id));
    }

    agent_session_id.and_then(AgentSessionRef::id)
}

pub fn persisted_session_from_launch_args(
    agent: crate::detect::Agent,
    args: &[String],
) -> Option<PersistedAgentSession> {
    let [command, session_id] = args else {
        return None;
    };
    if agent != crate::detect::Agent::Codex || command != "resume" || session_id.starts_with('-') {
        return None;
    }

    Some(PersistedAgentSession {
        source: "herdr:codex".into(),
        agent: "codex".into(),
        session_ref: AgentSessionRef::id(session_id.clone())?,
    })
}

pub fn normalize_session_start_source(value: Option<String>) -> Option<String> {
    match value.as_deref().map(str::trim) {
        Some(
            source @ ("startup" | "resume" | "clear" | "compact" | "branch" | "new" | "fork"
            | "select"),
        ) => Some(source.to_string()),
        _ => None,
    }
}

pub fn is_reserved_native_state_source(source: &str, agent: &str) -> bool {
    matches!(
        (source, agent),
        ("herdr:claude", "claude")
            | ("herdr:codex", "codex")
            | ("herdr:copilot", "copilot")
            | ("herdr:devin", "devin")
            | ("herdr:droid", "droid")
            | ("herdr:qodercli", "qodercli")
            | ("herdr:qwen", "qwen")
            | ("herdr:cursor", "cursor")
            | ("herdr:grok", "grok")
    )
}

pub fn session_ref_from_snapshot(
    source: &str,
    agent: &str,
    kind: AgentSessionRefKind,
    value: &str,
) -> Option<PersistedAgentSession> {
    if !is_official_agent_source(source, agent) {
        return None;
    }
    let session_ref = match (agent, kind) {
        ("pi" | "omp", AgentSessionRefKind::Path) => AgentSessionRef::path(value)?,
        (_, AgentSessionRefKind::Id) => AgentSessionRef::id(value)?,
        _ => return None,
    };
    Some(PersistedAgentSession {
        source: source.to_string(),
        agent: agent.to_string(),
        session_ref,
    })
}

//// How one agent's command line is read for the flags a resumed session must be started with again. A resume command
/// restores the conversation, and often the model, but not the rest of how the process was launched: a restored Claude
/// pane came back at the model's default effort, and a restored Codex pane could not start at all without `--no-daemon`.
/// Everything else on the command line (the session, the prompt, paths) is left out.
struct LaunchFlagSpec {
    /// Flags with one value, as every spelling the CLI accepts and the spelling written back; the last one given wins.
    values: &'static [(&'static [&'static str], &'static str)],
    /// Flags with one `key=value` value that can be given again; every one is kept, in order.
    repeated: &'static [(&'static [&'static str], &'static str)],
    switches: &'static [&'static str],
}

const CLAUDE_FLAGS: LaunchFlagSpec = LaunchFlagSpec {
    values: &[
        (&["--model"], "--model"),
        (&["--effort"], "--effort"),
        (&["--permission-mode"], "--permission-mode"),
    ],
    repeated: &[],
    switches: &["--dangerously-skip-permissions"],
};

const CODEX_FLAGS: LaunchFlagSpec = LaunchFlagSpec {
    values: &[
        (&["-p", "--profile"], "-p"),
        (&["-m", "--model"], "-m"),
        (&["-s", "--sandbox"], "-s"),
        (&["-a", "--ask-for-approval"], "-a"),
    ],
    repeated: &[(&["-c", "--config"], "-c")],
    switches: &[
        "--no-daemon",
        "--dangerously-bypass-approvals-and-sandbox",
        "--dangerously-bypass-hook-trust",
        "--approve-for-me",
        "--search",
    ],
};

const MAX_LAUNCH_ARG_VALUE_LEN: usize = 64;
const MAX_REPEATED_LAUNCH_FLAGS: usize = 16;

/// A value Herdr types back into a shell unquoted-safe: letters, digits and the punctuation of model names
/// (`claude-opus-5-5`, `opus[1m]`) and of `-c key=value` overrides.
fn launch_arg_value_is_safe(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_LAUNCH_ARG_VALUE_LEN
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(byte, b'.' | b'_' | b'-' | b':' | b'[' | b']' | b'=')
        })
        && !value.starts_with('-')
}

/// The flags in `args` that a restored session keeps, in a fixed order: value flags, then repeated ones, then switches.
/// A flag given twice keeps its last value, as the CLI does; a flag with a value that is missing or not safe to type
/// back is dropped, never the whole list.
fn launch_flags(args: &[String], spec: &LaunchFlagSpec) -> Vec<String> {
    let position = |table: &[(&[&str], &str)], name: &str| {
        table
            .iter()
            .position(|(spellings, _)| spellings.contains(&name))
    };
    let mut values: Vec<Option<String>> = vec![None; spec.values.len()];
    let mut repeated: Vec<String> = Vec::new();
    let mut switches = vec![false; spec.switches.len()];
    let mut index = 0;
    while index < args.len() {
        let arg = args[index].as_str();
        let (name, inline_value) = match arg.split_once('=') {
            Some((name, value)) if name.starts_with("--") => (name, Some(value)),
            _ => (arg, None),
        };
        let next = args.get(index + 1).map(String::as_str);
        let value = inline_value.or(next);
        let takes_next = inline_value.is_none() && next.is_some_and(|next| !next.starts_with('-'));
        let mut consumed = 1;
        if let Some(slot) = position(spec.values, name) {
            values[slot] = value
                .filter(|value| launch_arg_value_is_safe(value))
                .map(String::from);
            consumed = if takes_next { 2 } else { 1 };
        } else if let Some(slot) = position(spec.repeated, name) {
            let keep = value.filter(|value| {
                launch_arg_value_is_safe(value) && value.contains('=') && !value.starts_with('=')
            });
            if let Some(value) = keep {
                if repeated.len() < 2 * MAX_REPEATED_LAUNCH_FLAGS {
                    repeated.push(spec.repeated[slot].1.to_string());
                    repeated.push(value.to_string());
                }
            }
            consumed = if takes_next { 2 } else { 1 };
        } else if let Some(slot) = spec.switches.iter().position(|flag| *flag == name) {
            switches[slot] = true;
        }
        index += consumed;
    }
    let mut flags = Vec::new();
    for ((_, flag), value) in spec.values.iter().zip(values) {
        if let Some(value) = value {
            flags.push((*flag).to_string());
            flags.push(value);
        }
    }
    flags.extend(repeated);
    for (flag, on) in spec.switches.iter().zip(switches) {
        if on {
            flags.push((*flag).to_string());
        }
    }
    flags
}

fn claude_flags(args: &[String]) -> Vec<String> {
    launch_flags(args, &CLAUDE_FLAGS)
}

/// The effort a resumed Opus session runs at when its recorded launch asked for xhigh or max (Blake 11/11: no Opus at extra-high).
const OPUS_RESUME_EFFORT: &str = "medium";

/// `claude_flags` for a resume command: an Opus model recorded with `--effort xhigh` or `max` comes back at `OPUS_RESUME_EFFORT`, and nothing else
/// changes. What a running process was launched with is still recorded as it is (`agent_launch`); only the command that starts it again is clamped.
fn claude_resume_flags(args: &[String]) -> Vec<String> {
    let mut flags = claude_flags(args);
    let opus = flags
        .windows(2)
        .any(|pair| pair[0] == "--model" && pair[1].to_ascii_lowercase().contains("opus"));
    if opus {
        for index in 1..flags.len() {
            if flags[index - 1] == "--effort" && matches!(flags[index].as_str(), "xhigh" | "max") {
                flags[index] = OPUS_RESUME_EFFORT.to_string();
            }
        }
    }
    flags
}

/// Where Luna's Claude mod keeps what each Claude session last ran at, under herdr's state directory: one
/// `<session id>.json` holding `{"model": "...", "effort": "..."}`, written on every main-loop request that moved
/// (`/model`, `/effort`, a restore). It is the one record that follows a session through changes made after launch;
/// the launch flags in session.json never do.
const CLAUDE_SESSION_RECORDS: [&str; 2] = ["plugins/triiodide.luna", "effort"];

pub fn claude_session_records_dir() -> std::path::PathBuf {
    CLAUDE_SESSION_RECORDS
        .iter()
        .fold(crate::config::state_dir(), |dir, part| dir.join(part))
}

#[derive(Deserialize)]
struct ClaudeSessionRecord {
    model: String,
    /// A level (`"medium"`), or an integer budget, which is not a flag and is left out.
    effort: serde_json::Value,
}

/// The `--model` and `--effort` the session last ran at, when its record in `records_dir` names a model that can be
/// typed back. The effort belongs to that model: a record whose effort is a budget or unsafe gives the model alone.
fn claude_session_record_flags(records_dir: &Path, session_id: &str) -> Option<Vec<String>> {
    if !is_uuid(session_id) {
        return None;
    }
    let text = std::fs::read_to_string(records_dir.join(format!("{session_id}.json"))).ok()?;
    let record: ClaudeSessionRecord = serde_json::from_str(&text).ok()?;
    if !launch_arg_value_is_safe(&record.model) {
        return None;
    }
    let mut flags = vec!["--model".to_string(), record.model];
    if let Some(effort) = record
        .effort
        .as_str()
        .filter(|effort| launch_arg_value_is_safe(effort))
    {
        flags.extend(["--effort".to_string(), effort.to_string()]);
    }
    Some(flags)
}

/// The launch flags a restore resumes a session with. A Claude session resumes at the model and effort its record
/// says it last ran at, in place of the ones it was launched with and never together with them: a tab started as Opus
/// and switched to Fable resumes as Fable at Fable's effort, and one moved down to medium stays there. The rest of the
/// launch flags (permission mode, switches) stay. Another agent, or a session with no record, keeps its launch flags.
pub fn restore_launch_args(
    records_dir: &Path,
    agent: &str,
    session_ref: &AgentSessionRef,
    launch_args: &[String],
) -> Vec<String> {
    if agent != "claude" || session_ref.kind != AgentSessionRefKind::Id {
        return launch_args.to_vec();
    }
    let Some(record) = claude_session_record_flags(records_dir, &session_ref.value) else {
        return launch_args.to_vec();
    };
    let mut args = Vec::new();
    let mut launched = claude_flags(launch_args).into_iter();
    while let Some(flag) = launched.next() {
        let value = CLAUDE_FLAGS
            .values
            .iter()
            .any(|(_, name)| *name == flag)
            .then(|| launched.next())
            .flatten();
        if !matches!(flag.as_str(), "--model" | "--effort") {
            args.push(flag);
            args.extend(value);
        }
    }
    args.extend(record);
    args
}

fn codex_flags(args: &[String]) -> Vec<String> {
    launch_flags(args, &CODEX_FLAGS)
}

/// A session id as the agents print it: a UUID. A `--resume` that names a search term, or a bare `resume --last`, has
/// no id to restore.
fn is_uuid(value: &str) -> bool {
    let groups: Vec<&str> = value.split('-').collect();
    groups.len() == 5
        && groups
            .iter()
            .zip([8, 4, 4, 4, 12])
            .all(|(group, len)| group.len() == len && group.bytes().all(|b| b.is_ascii_hexdigit()))
}

/// The session a Claude Code command line resumes or starts: `--resume <id>`, `-r <id>`, `--resume=<id>`, `--session-id <id>`.
fn claude_session_id(args: &[String]) -> Option<String> {
    let mut found = None;
    let mut index = 0;
    while index < args.len() {
        let arg = args[index].as_str();
        let value = match arg.split_once('=') {
            Some(("--resume" | "--session-id", value)) => Some(value),
            _ if matches!(arg, "--resume" | "-r" | "--session-id") => {
                index += 1;
                args.get(index).map(String::as_str)
            }
            _ => None,
        };
        if let Some(value) = value.filter(|value| is_uuid(value)) {
            found = Some(value.to_string());
        }
        index += 1;
    }
    found
}

/// The session a Codex command line resumes: the first id after `resume`, wherever the flags sit around it.
fn codex_session_id(args: &[String]) -> Option<String> {
    let resume = args.iter().position(|arg| arg == "resume")?;
    args[resume + 1..].iter().find(|arg| is_uuid(arg)).cloned()
}

/// What a running agent process was started as: the session it resumes when its command line names one, and the flags
/// a restored session keeps.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentLaunch {
    pub agent: crate::detect::Agent,
    pub session_id: Option<String>,
    pub flags: Vec<String>,
}

/// The launch of a Claude Code or Codex process from its command line (`argv[0]` is the program); other agents have none.
pub fn agent_launch(agent: crate::detect::Agent, argv: &[String]) -> Option<AgentLaunch> {
    let args = argv.get(1..).unwrap_or_default();
    let (session_id, flags) = match agent {
        crate::detect::Agent::Claude => (claude_session_id(args), claude_flags(args)),
        crate::detect::Agent::Codex => (codex_session_id(args), codex_flags(args)),
        _ => return None,
    };
    Some(AgentLaunch {
        agent,
        session_id,
        flags,
    })
}

/// The launch of the `agent` process in a pane's foreground job, or `None` when the job has no such process whose
/// command line could be read.
pub fn agent_launch_in_job(
    agent: crate::detect::Agent,
    job: &crate::platform::ForegroundJob,
) -> Option<AgentLaunch> {
    job.processes.iter().find_map(|process| {
        // The detector's own reading of a process, which also sees an agent behind a wrapper: `codex` typed in a shell is the npm
        // shim, node running codex.js, and the foreground job can hold that node.exe alone.
        let alone = crate::platform::ForegroundJob {
            process_group_id: process.pid,
            processes: vec![process.clone()],
        };
        if crate::detect::identify_agent_in_job(&alone)?.0 != agent {
            return None;
        }
        process
            .argv
            .as_deref()
            .and_then(|argv| agent_launch(agent, argv))
    })
}

pub fn plan(source: &str, agent: &str, session_ref: &AgentSessionRef) -> Option<AgentResumePlan> {
    plan_with_launch_args(source, agent, session_ref, &[])
}

/// The command that resumes a session. `launch_args` are the flags the session's process was started with
/// (`AgentLaunch::flags`); Claude and Codex get them again, other agents ignore them.
pub fn plan_with_launch_args(
    source: &str,
    agent: &str,
    session_ref: &AgentSessionRef,
    launch_args: &[String],
) -> Option<AgentResumePlan> {
    if !is_official_agent_source(source, agent) {
        return None;
    }

    let argv = match (source, agent, session_ref.kind) {
        ("herdr:claude", "claude", AgentSessionRefKind::Id) => {
            let mut argv = vec![
                "claude".into(),
                "--resume".into(),
                session_ref.value.clone(),
            ];
            // The list is read back from session.json, which anyone can edit: sanitize it again.
            argv.extend(claude_resume_flags(launch_args));
            argv
        }
        ("herdr:codex", "codex", AgentSessionRefKind::Id) => {
            let mut argv = vec!["codex".into(), "resume".into(), session_ref.value.clone()];
            argv.extend(codex_flags(launch_args));
            argv
        }
        ("herdr:copilot", "copilot", AgentSessionRefKind::Id) => {
            vec!["copilot".into(), format!("--resume={}", session_ref.value)]
        }
        ("herdr:devin", "devin", AgentSessionRefKind::Id) => {
            vec!["devin".into(), "--resume".into(), session_ref.value.clone()]
        }
        ("herdr:droid", "droid", AgentSessionRefKind::Id) => {
            vec!["droid".into(), "--resume".into(), session_ref.value.clone()]
        }
        ("herdr:kimi", "kimi", AgentSessionRefKind::Id) => {
            vec!["kimi".into(), "--session".into(), session_ref.value.clone()]
        }
        ("herdr:mastracode", "mastracode", AgentSessionRefKind::Id) => {
            vec![
                "mastracode".into(),
                "--thread".into(),
                session_ref.value.clone(),
            ]
        }
        ("herdr:pi", "pi", AgentSessionRefKind::Path | AgentSessionRefKind::Id) => {
            vec!["pi".into(), "--session".into(), session_ref.value.clone()]
        }
        ("herdr:omp", "omp", AgentSessionRefKind::Path | AgentSessionRefKind::Id) => {
            // omp resume is `-r, --resume=<value>` (ID prefix or path); it has no
            // `--session` flag, unlike pi.
            vec!["omp".into(), format!("--resume={}", session_ref.value)]
        }
        ("herdr:hermes", "hermes", AgentSessionRefKind::Id) => {
            vec![
                "hermes".into(),
                "--resume".into(),
                session_ref.value.clone(),
            ]
        }
        ("herdr:opencode", "opencode", AgentSessionRefKind::Id) => {
            vec![
                "opencode".into(),
                "--session".into(),
                session_ref.value.clone(),
            ]
        }
        ("herdr:qodercli", "qodercli", AgentSessionRefKind::Id) => {
            vec![
                "qodercli".into(),
                "--resume".into(),
                session_ref.value.clone(),
            ]
        }
        ("herdr:qwen", "qwen", AgentSessionRefKind::Id) => {
            vec!["qwen".into(), "--resume".into(), session_ref.value.clone()]
        }
        ("herdr:kilo", "kilo", AgentSessionRefKind::Id) => {
            vec!["kilo".into(), "--session".into(), session_ref.value.clone()]
        }
        ("herdr:cursor", "cursor", AgentSessionRefKind::Id) => {
            vec![
                if cfg!(windows) {
                    "cursor-agent.cmd"
                } else {
                    "cursor-agent"
                }
                .into(),
                "--resume".into(),
                session_ref.value.clone(),
            ]
        }
        ("herdr:antigravity_cli", "agy", AgentSessionRefKind::Id) => {
            vec![
                "agy".into(),
                "--conversation".into(),
                session_ref.value.clone(),
            ]
        }
        ("herdr:grok", "grok", AgentSessionRefKind::Id) => {
            vec!["grok".into(), "--resume".into(), session_ref.value.clone()]
        }
        ("herdr:letta", "letta", AgentSessionRefKind::Id) => {
            if let Some(agent_id) = session_ref.value.strip_prefix("default:") {
                if agent_id.is_empty() {
                    return None;
                }
                vec![
                    "letta".into(),
                    "--conversation".into(),
                    "default".into(),
                    "--agent".into(),
                    agent_id.into(),
                ]
            } else {
                vec![
                    "letta".into(),
                    "--conversation".into(),
                    session_ref.value.clone(),
                ]
            }
        }
        _ => return None,
    };

    Some(AgentResumePlan {
        agent: agent.to_string(),
        argv,
        dedupe_key: dedupe_key(source, agent, session_ref),
    })
}

/// The model and reasoning effort of a Codex thread's last turn, as its rollout records them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodexRolloutSettings {
    pub model: String,
    pub effort: String,
}

const MAX_ROLLOUT_TAIL_BYTES: u64 = 8 * 1024 * 1024;

/// The rollout file of a Codex thread: `sessions/YYYY/MM/DD/rollout-<time>-<id>.jsonl`, found newest day first.
fn codex_rollout_path(codex_home: &Path, session_id: &str) -> Option<std::path::PathBuf> {
    fn newest_first(dir: &Path) -> Vec<std::path::PathBuf> {
        let mut entries: Vec<_> = std::fs::read_dir(dir)
            .into_iter()
            .flatten()
            .flatten()
            .map(|entry| entry.path())
            .collect();
        entries.sort();
        entries.reverse();
        entries
    }
    let suffix = format!("-{session_id}.jsonl");
    for year in newest_first(&codex_home.join("sessions")) {
        for month in newest_first(&year) {
            for day in newest_first(&month) {
                for file in newest_first(&day) {
                    let is_rollout = file
                        .file_name()
                        .and_then(|name| name.to_str())
                        .is_some_and(|name| name.starts_with("rollout-") && name.ends_with(&suffix));
                    if is_rollout {
                        return Some(file);
                    }
                }
            }
        }
    }
    None
}

/// The model and effort of the last `turn_context` in a Codex thread's rollout, which is what its next turn runs with
/// when the resume command names neither. `None` when the thread has no rollout or no turn yet.
pub fn codex_rollout_settings(codex_home: &Path, session_id: &str) -> Option<CodexRolloutSettings> {
    use std::io::{Read, Seek, SeekFrom};

    let mut file = std::fs::File::open(codex_rollout_path(codex_home, session_id)?).ok()?;
    let length = file.metadata().ok()?.len();
    file.seek(SeekFrom::Start(length.saturating_sub(MAX_ROLLOUT_TAIL_BYTES)))
        .ok()?;
    let mut tail = Vec::new();
    file.read_to_end(&mut tail).ok()?;
    String::from_utf8_lossy(&tail).lines().rev().find_map(|line| {
        let turn: serde_json::Value = serde_json::from_str(line).ok()?;
        if turn.get("type")?.as_str()? != "turn_context" {
            return None;
        }
        let payload = turn.get("payload")?;
        let model = payload.get("model")?.as_str()?;
        let effort = payload.get("effort")?.as_str()?;
        (launch_arg_value_is_safe(model) && launch_arg_value_is_safe(effort)).then(|| {
            CodexRolloutSettings {
                model: model.to_string(),
                effort: effort.to_string(),
            }
        })
    })
}

/// `plan_with_launch_args` for a restore: a Codex thread whose recorded flags name no model or no effort gets them from its
/// rollout, so that a thread resumed from a session.json written before the flags were recorded still runs as it did.
pub fn plan_for_restore(
    source: &str,
    agent: &str,
    session_ref: &AgentSessionRef,
    launch_args: &[String],
    codex_home: &Path,
) -> Option<AgentResumePlan> {
    let mut plan = plan_with_launch_args(source, agent, session_ref, launch_args)?;
    if agent != "codex" || session_ref.kind != AgentSessionRefKind::Id {
        return Some(plan);
    }
    let has_model = plan.argv.iter().any(|arg| arg == "-m");
    let has_effort = plan
        .argv
        .windows(2)
        .any(|pair| pair[0] == "-c" && pair[1].starts_with("model_reasoning_effort="));
    if has_model && has_effort {
        return Some(plan);
    }
    if let Some(last_turn) = codex_rollout_settings(codex_home, &session_ref.value) {
        if !has_model {
            plan.argv.extend(["-m".into(), last_turn.model]);
        }
        if !has_effort {
            plan.argv.extend([
                "-c".into(),
                format!("model_reasoning_effort={}", last_turn.effort),
            ]);
        }
    }
    Some(plan)
}

pub fn dedupe_key(source: &str, agent: &str, session_ref: &AgentSessionRef) -> String {
    format!(
        "{source}\u{0}{agent}\u{0}{:?}\u{0}{}",
        session_ref.kind, session_ref.value
    )
}

pub(crate) fn is_official_agent_source(source: &str, agent: &str) -> bool {
    matches!(
        (source, agent),
        ("herdr:claude", "claude")
            | ("herdr:codex", "codex")
            | ("herdr:copilot", "copilot")
            | ("herdr:devin", "devin")
            | ("herdr:droid", "droid")
            | ("herdr:kimi", "kimi")
            | ("herdr:omp", "omp")
            | ("herdr:mastracode", "mastracode")
            | ("herdr:pi", "pi")
            | ("herdr:hermes", "hermes")
            | ("herdr:opencode", "opencode")
            | ("herdr:qodercli", "qodercli")
            | ("herdr:qwen", "qwen")
            | ("herdr:kilo", "kilo")
            | ("herdr:cursor", "cursor")
            | ("herdr:antigravity_cli", "agy")
            | ("herdr:grok", "grok")
            | ("herdr:letta", "letta")
    )
}

fn valid_session_id(value: &str) -> bool {
    !value.is_empty() && value.len() <= MAX_SESSION_ID_LEN && !value.chars().any(char::is_control)
}

fn valid_session_path(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_SESSION_PATH_LEN
        && !value.chars().any(char::is_control)
        && Path::new(value).is_absolute()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn absolute_test_path(name: &str) -> String {
        std::env::current_dir()
            .unwrap()
            .join(name)
            .display()
            .to_string()
    }

    fn argv(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|part| part.to_string()).collect()
    }

    #[test]
    fn reported_resume_argv_requires_a_plain_command_name() {
        assert!(validate_resume_argv(&argv(&[
            "prime-agent",
            "--resume",
            "01a0de21",
            "--model",
            "gpt 6"
        ]))
        .is_ok());
        assert!(validate_resume_argv(&argv(&["cursor-agent.cmd", "--resume", "id"])).is_ok());

        for invalid in [
            argv(&[]),
            argv(&[""]),
            argv(&["/usr/bin/prime-agent", "--resume", "id"]),
            argv(&["C:\\Program Files\\Prime\\prime.exe"]),
            argv(&["prime agent"]),
            argv(&["-prime"]),
            argv(&["prime-agent", "bad\nline"]),
            argv(&["prime-agent", "--name", "can's session"]),
            vec!["prime-agent".to_string(); MAX_RESUME_ARGS + 1],
            argv(&["prime-agent", &"x".repeat(MAX_RESUME_ARGV_BYTES)]),
        ] {
            assert!(validate_resume_argv(&invalid).is_err(), "{invalid:?}");
        }
    }

    #[test]
    fn native_state_reservation_excludes_full_lifecycle_sources() {
        assert!(is_reserved_native_state_source("herdr:claude", "claude"));
        assert!(is_reserved_native_state_source("herdr:codex", "codex"));
        assert!(is_reserved_native_state_source("herdr:devin", "devin"));
        assert!(!is_reserved_native_state_source("herdr:kimi", "kimi"));
        assert!(!is_reserved_native_state_source(
            "herdr:opencode",
            "opencode"
        ));
    }

    #[test]
    fn codex_noncanonical_resume_launch_has_no_explicit_session() {
        assert_eq!(
            persisted_session_from_launch_args(
                crate::detect::Agent::Codex,
                &["resume".into(), "codex-session".into()]
            )
            .unwrap()
            .session_ref
            .value,
            "codex-session"
        );
        assert!(persisted_session_from_launch_args(
            crate::detect::Agent::Codex,
            &["resume".into(), "--last".into()]
        )
        .is_none());
        assert!(persisted_session_from_launch_args(
            crate::detect::Agent::Codex,
            &["resume".into(), "not-a-session".into(), "--last".into()]
        )
        .is_none());
        assert!(persisted_session_from_launch_args(
            crate::detect::Agent::Codex,
            &[
                "--remote".into(),
                "ws://example.test".into(),
                "resume".into(),
                "remote-session".into(),
            ]
        )
        .is_none());
    }

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|arg| arg.to_string()).collect()
    }

    fn claude_launch_args(argv: &[String]) -> Vec<String> {
        agent_launch(crate::detect::Agent::Claude, argv)
            .unwrap()
            .flags
    }

    #[test]
    fn claude_launch_args_keep_model_effort_and_permission_mode() {
        let argv = args(&[
            r"C:\Users\me\.local\bin\claude.exe",
            "--resume",
            "e5042d6c-961c-4613-9ad2-b0c820b87cb3",
            "--model",
            "claude-sonnet-5-5",
            "--effort",
            "xhigh",
            "--permission-mode",
            "auto",
            "--dangerously-skip-permissions",
        ]);
        assert_eq!(
            claude_launch_args(&argv),
            args(&[
                "--model",
                "claude-sonnet-5-5",
                "--effort",
                "xhigh",
                "--permission-mode",
                "auto",
                "--dangerously-skip-permissions",
            ])
        );
    }

    #[test]
    fn claude_launch_args_come_out_in_a_fixed_order_and_the_last_value_wins() {
        let argv = args(&[
            "claude",
            "--effort",
            "low",
            "--model=opus[1m]",
            "--effort=high",
            "--model",
            "sonnet",
        ]);
        assert_eq!(
            claude_launch_args(&argv),
            args(&["--model", "sonnet", "--effort", "high"])
        );
    }

    #[test]
    fn claude_launch_args_leave_out_the_session_the_prompt_and_other_flags() {
        let argv = args(&[
            "claude",
            "--resume",
            "abc",
            "-c",
            "--session-id",
            "def",
            "--plugin-dir",
            r"C:\mods\luna",
            "-p",
            "fix --model and --effort bugs",
        ]);
        assert!(claude_launch_args(&argv).is_empty());
        assert!(claude_launch_args(&[]).is_empty());
        assert!(claude_launch_args(&args(&["claude"])).is_empty());
    }

    #[test]
    fn claude_launch_args_drop_a_flag_whose_value_is_missing_or_unsafe_not_the_list() {
        let argv = args(&[
            "claude",
            "--model",
            "--effort",
            "high",
            "--permission-mode",
            "it's",
        ]);
        assert_eq!(claude_launch_args(&argv), args(&["--effort", "high"]));
        for unsafe_value in [
            "a b",
            "x;rm",
            "$(x)",
            "a'b",
            "",
            "-x",
            "m".repeat(65).as_str(),
        ] {
            assert!(
                claude_launch_args(&args(&["claude", "--model", unsafe_value])).is_empty(),
                "{unsafe_value:?} was kept"
            );
        }
        // Whatever is kept is something a resume command can carry.
        let kept = plan_with_launch_args(
            "herdr:claude",
            "claude",
            &AgentSessionRef::id("s").unwrap(),
            &args(&["--model", "opus[1m]", "--effort", "xhigh"]),
        )
        .unwrap();
        assert!(validate_resume_argv(&kept.argv).is_ok());
    }

    #[test]
    fn claude_resume_carries_its_launch_args_and_other_agents_ignore_them() {
        let session = AgentSessionRef::id("claude-session").unwrap();
        let flags = args(&["--model", "claude-sonnet-5-5", "--effort", "xhigh"]);
        let resumed = plan_with_launch_args("herdr:claude", "claude", &session, &flags).unwrap();
        assert_eq!(
            resumed.argv,
            args(&[
                "claude",
                "--resume",
                "claude-session",
                "--model",
                "claude-sonnet-5-5",
                "--effort",
                "xhigh"
            ])
        );
        assert_eq!(
            resumed.dedupe_key,
            plan("herdr:claude", "claude", &session).unwrap().dedupe_key,
            "the flags do not make a second session"
        );
        let opencode = AgentSessionRef::id("opencode-session").unwrap();
        assert_eq!(
            plan_with_launch_args("herdr:opencode", "opencode", &opencode, &flags)
                .unwrap()
                .argv,
            args(&["opencode", "--session", "opencode-session"])
        );
        // A hand-edited list is cleaned: only the known flags with safe values get through.
        let edited = args(&[
            "--dangerously-skip-permissions",
            "--resume",
            "other",
            "--model",
            "x y",
            "--effort",
            "low",
        ]);
        assert_eq!(
            plan_with_launch_args("herdr:claude", "claude", &session, &edited)
                .unwrap()
                .argv,
            args(&[
                "claude",
                "--resume",
                "claude-session",
                "--effort",
                "low",
                "--dangerously-skip-permissions"
            ])
        );
    }

    #[test]
    fn claude_resume_brings_an_opus_model_at_xhigh_or_max_down_to_medium_and_changes_nothing_else() {
        let session = AgentSessionRef::id("claude-session").unwrap();
        for model in ["claude-opus-5-5", "claude-opus-5-5[1m]", "opus", "opus[1m]", "CLAUDE-OPUS-5-5"] {
            for effort in ["xhigh", "max"] {
                let recorded = args(&[
                    "--model",
                    model,
                    "--effort",
                    effort,
                    "--permission-mode",
                    "auto",
                    "--dangerously-skip-permissions",
                ]);
                assert_eq!(
                    plan_with_launch_args("herdr:claude", "claude", &session, &recorded)
                        .unwrap()
                        .argv,
                    args(&[
                        "claude",
                        "--resume",
                        "claude-session",
                        "--model",
                        model,
                        "--effort",
                        OPUS_RESUME_EFFORT,
                        "--permission-mode",
                        "auto",
                        "--dangerously-skip-permissions",
                    ]),
                    "{model} at {effort}"
                );
            }
        }
    }

    #[test]
    fn claude_resume_leaves_every_other_model_and_effort_as_recorded() {
        let session = AgentSessionRef::id("claude-session").unwrap();
        let resumed = |recorded: &[&str]| {
            plan_with_launch_args("herdr:claude", "claude", &session, &args(recorded))
                .unwrap()
                .argv
        };
        for effort in ["low", "medium", "high"] {
            assert_eq!(
                resumed(&["--model", "claude-opus-5-5", "--effort", effort]),
                args(&["claude", "--resume", "claude-session", "--model", "claude-opus-5-5", "--effort", effort])
            );
        }
        for model in ["claude-sonnet-5-5", "claude-fable-5-2", "claude-haiku-5-5", "sonnet"] {
            for effort in ["xhigh", "max"] {
                assert_eq!(
                    resumed(&["--model", model, "--effort", effort]),
                    args(&["claude", "--resume", "claude-session", "--model", model, "--effort", effort]),
                    "{model} at {effort}"
                );
            }
        }
        // No effort recorded, or none with a model: nothing to bring down.
        assert_eq!(
            resumed(&["--model", "claude-opus-5-5"]),
            args(&["claude", "--resume", "claude-session", "--model", "claude-opus-5-5"])
        );
        assert_eq!(
            resumed(&["--effort", "xhigh"]),
            args(&["claude", "--resume", "claude-session", "--effort", "xhigh"])
        );
    }

    #[test]
    fn what_an_opus_tab_runs_at_is_still_recorded_as_it_was_launched() {
        let launch = agent_launch(
            crate::detect::Agent::Claude,
            &args(&["claude", "--model", "claude-opus-5-5", "--effort", "xhigh"]),
        )
        .unwrap();
        assert_eq!(launch.flags, args(&["--model", "claude-opus-5-5", "--effort", "xhigh"]));
    }

    // The command lines below are the ones the fleet's tabs run (bet ps, 10/30).
    const CODEX_TAB: &[&str] = &[
        r"C:\Users\BlakeThomas\AppData\Roaming\npm\node_modules\@openai\codex\node_modules\@openai\codex-win32-x64\vendor\x86_64-pc-windows-msvc\bin\codex.exe",
        "resume",
        "01a189f6-0826-7b60-8e88-522e79c04a9a",
        "--no-daemon",
        "-p",
        "lorefs",
        "-m",
        "gpt-6.1-sol",
        "-c",
        "model_reasoning_effort=max",
        "-c",
        "features.default_mode_request_user_input=true",
        "-c",
        "include_collaboration_mode_instructions=false",
    ];

    #[test]
    fn codex_launch_keeps_the_flags_a_resume_needs_and_the_session() {
        let launch = agent_launch(crate::detect::Agent::Codex, &args(CODEX_TAB)).unwrap();
        assert_eq!(
            launch.session_id.as_deref(),
            Some("01a189f6-0826-7b60-8e88-522e79c04a9a")
        );
        assert_eq!(
            launch.flags,
            args(&[
                "-p",
                "lorefs",
                "-m",
                "gpt-6.1-sol",
                "-c",
                "model_reasoning_effort=max",
                "-c",
                "features.default_mode_request_user_input=true",
                "-c",
                "include_collaboration_mode_instructions=false",
                "--no-daemon",
            ])
        );
    }

    fn foreground_process(pid: u32, name: &str, argv: &[&str]) -> crate::platform::ForegroundProcess {
        crate::platform::ForegroundProcess {
            pid,
            name: name.into(),
            argv0: argv.first().map(|arg| arg.to_string()),
            argv: Some(args(argv)),
            cmdline: Some(argv.join(" ")),
        }
    }

    #[test]
    fn the_codex_behind_a_node_wrapper_is_found_in_the_foreground_job() {
        // pane 3 of the throwaway-server proof: `codex` is codex.js run by node, which starts the vendored codex.exe
        let job = crate::platform::ForegroundJob {
            process_group_id: 179364,
            processes: vec![
                foreground_process(
                    179364,
                    "node.exe",
                    &[
                        "node",
                        r"C:\Users\me\AppData\Roaming\npm\\node_modules\@openai\codex\bin\codex.js",
                        "resume",
                        "01a18ce6-1111-7c01-9098-fee917068521",
                        "--no-daemon",
                        "-p",
                        "lorefs",
                    ],
                ),
                foreground_process(
                    108324,
                    "codex.exe",
                    &[
                        r"C:\Users\me\AppData\Roaming\npm\node_modules\@openai\codex\node_modules\@openai\codex-win32-x64\vendor\x86_64-pc-windows-msvc\bin\codex.exe",
                        "resume",
                        "01a18ce6-1111-7c01-9098-fee917068521",
                        "--no-daemon",
                        "-p",
                        "lorefs",
                    ],
                ),
            ],
        };
        let launch = agent_launch_in_job(crate::detect::Agent::Codex, &job).expect("a Codex launch");
        assert_eq!(
            launch.session_id.as_deref(),
            Some("01a18ce6-1111-7c01-9098-fee917068521")
        );
        assert_eq!(launch.flags, args(&["-p", "lorefs", "--no-daemon"]));
        assert!(agent_launch_in_job(crate::detect::Agent::Claude, &job).is_none());
    }

    #[test]
    fn a_foreground_job_of_only_the_node_wrapper_still_names_the_codex_launch() {
        // What the Windows foreground selection gave the running server for a Codex typed by hand (`codex` is the npm shim):
        // node.exe running codex.js, with no codex.exe process in the job.
        let job = crate::platform::ForegroundJob {
            process_group_id: 179364,
            processes: vec![foreground_process(
                179364,
                "node.exe",
                &[
                    "node",
                    r"C:\Users\me\AppData\Roaming\npm\\node_modules\@openai\codex\bin\codex.js",
                    "resume",
                    "01a18ce6-1111-7c01-9098-fee917068521",
                    "--no-daemon",
                    "-p",
                    "lorefs",
                    "-m",
                    "hand-model",
                ],
            )],
        };
        let launch = agent_launch_in_job(crate::detect::Agent::Codex, &job).expect("a Codex launch");
        assert_eq!(
            launch.session_id.as_deref(),
            Some("01a18ce6-1111-7c01-9098-fee917068521")
        );
        assert_eq!(
            launch.flags,
            args(&["-p", "lorefs", "-m", "hand-model", "--no-daemon"])
        );
    }

    #[test]
    fn codex_launch_reads_flags_on_either_side_of_resume() {
        // clouds-astra's tab: the flags come first and `resume <id>` last.
        let argv = args(&[
            "codex.exe",
            "--no-daemon",
            "-p",
            "lorefs",
            "-m",
            "gpt-6.1-sol",
            "-c",
            "model_reasoning_effort=xhigh",
            "resume",
            "01a18ae5-96a8-7a02-a94d-ccd78285ac41",
        ]);
        let launch = agent_launch(crate::detect::Agent::Codex, &argv).unwrap();
        assert_eq!(
            launch.session_id.as_deref(),
            Some("01a18ae5-96a8-7a02-a94d-ccd78285ac41")
        );
        assert_eq!(
            launch.flags,
            args(&[
                "-p",
                "lorefs",
                "-m",
                "gpt-6.1-sol",
                "-c",
                "model_reasoning_effort=xhigh",
                "--no-daemon",
            ])
        );
    }

    #[test]
    fn codex_launch_without_a_session_id_names_no_session() {
        for argv in [
            args(&["codex"]),
            args(&["codex", "resume", "--last"]),
            args(&["codex", "resume", "my thread name"]),
            args(&["codex", "exec", "01a189f6-0826-7b60-8e88-522e79c04a9a"]),
        ] {
            let launch = agent_launch(crate::detect::Agent::Codex, &argv).unwrap();
            assert_eq!(launch.session_id, None, "{argv:?}");
        }
    }

    #[test]
    fn claude_launch_names_the_session_it_resumes_or_starts() {
        let id = "411b5331-9af1-4abd-a344-8b4a8d217045";
        for argv in [
            args(&["claude", "--resume", id, "--model", "x"]),
            args(&["claude", "-r", id]),
            args(&["claude", "--resume=411b5331-9af1-4abd-a344-8b4a8d217045"]),
            args(&["claude", "--model", "x", "--session-id", id]),
        ] {
            let launch = agent_launch(crate::detect::Agent::Claude, &argv).unwrap();
            assert_eq!(launch.session_id.as_deref(), Some(id), "{argv:?}");
        }
        for argv in [
            args(&["claude"]),
            args(&["claude", "--resume"]),
            args(&["claude", "--resume", "fix the build"]),
            args(&["claude", "-c"]),
        ] {
            let launch = agent_launch(crate::detect::Agent::Claude, &argv).unwrap();
            assert_eq!(launch.session_id, None, "{argv:?}");
        }
        assert!(agent_launch(crate::detect::Agent::Pi, &args(&["pi", "--session", "x"])).is_none());
    }

    #[test]
    fn codex_resume_carries_its_launch_args_and_cleans_a_hand_edited_list() {
        let session = AgentSessionRef::id("codex-session").unwrap();
        let flags = args(&[
            "-p",
            "lorefs",
            "-m",
            "gpt-6.1-sol",
            "-c",
            "model_reasoning_effort=max",
            "--no-daemon",
        ]);
        let resumed = plan_with_launch_args("herdr:codex", "codex", &session, &flags).unwrap();
        assert_eq!(
            resumed.argv,
            [args(&["codex", "resume", "codex-session"]), flags.clone()].concat()
        );
        assert_eq!(
            resumed.dedupe_key,
            plan("herdr:codex", "codex", &session).unwrap().dedupe_key,
            "the flags do not make a second session"
        );
        assert!(validate_resume_argv(&resumed.argv).is_ok());

        // session.json can be edited by anyone: only the known flags with safe values get through.
        let edited = args(&[
            "--remote",
            "ws://evil.test",
            "-c",
            "notakeyvalue",
            "-c",
            "x=$(calc)",
            "-m",
            "a b",
            "-p",
            "lorefs",
            "resume",
            "other",
            "--no-daemon",
        ]);
        assert_eq!(
            plan_with_launch_args("herdr:codex", "codex", &session, &edited)
                .unwrap()
                .argv,
            args(&["codex", "resume", "codex-session", "-p", "lorefs", "--no-daemon"])
        );
    }

    fn write_rollout(home: &Path, day: &str, session: &str, lines: &[&str]) {
        let dir = home.join("sessions").join(day);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join(format!("rollout-2026-10-30T00-00-00-{session}.jsonl")),
            lines.join("\n"),
        )
        .unwrap();
    }

    fn temp_home(name: &str) -> std::path::PathBuf {
        let home = std::env::temp_dir().join(format!("herdr-codex-home-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();
        home
    }

    #[test]
    fn codex_rollout_gives_the_model_and_effort_of_its_last_turn() {
        let home = temp_home("rollout");
        let session = "01a18ce6-42f2-7c01-9098-fee91706852d";
        write_rollout(
            &home,
            "2026/10/30",
            session,
            &[
                r#"{"type":"session_meta","payload":{"id":"x"}}"#,
                r#"{"type":"turn_context","payload":{"model":"gpt-6-astra","effort":"high"}}"#,
                r#"{"type":"event_msg","payload":{"type":"agent_message"}}"#,
                r#"{"type":"turn_context","payload":{"model":"gpt-6.1-sol","effort":"max"}}"#,
                r#"{"type":"event_msg","payload":{"type":"token_count"}}"#,
            ],
        );
        assert_eq!(
            codex_rollout_settings(&home, session),
            Some(CodexRolloutSettings {
                model: "gpt-6.1-sol".into(),
                effort: "max".into()
            })
        );
        assert_eq!(codex_rollout_settings(&home, "01a18ce6-0000-7c01-9098-fee91706852d"), None);
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn codex_restore_fills_the_model_and_effort_from_the_rollout_only_where_the_launch_lacks_them() {
        let home = temp_home("fallback");
        let id = "01a18ce6-42f2-7c01-9098-fee91706852d";
        write_rollout(
            &home,
            "2026/10/29",
            id,
            &[r#"{"type":"turn_context","payload":{"model":"gpt-6.1-sol","effort":"max"}}"#],
        );
        let session = AgentSessionRef::id(id).unwrap();
        let plan_for = |launch_args: &[&str]| {
            plan_for_restore("herdr:codex", "codex", &session, &args(launch_args), &home)
                .unwrap()
                .argv
        };
        // nothing recorded: the rollout supplies both
        assert_eq!(
            plan_for(&[]),
            args(&["codex", "resume", id, "-m", "gpt-6.1-sol", "-c", "model_reasoning_effort=max"])
        );
        // a recorded model and effort are the launch's own and stay
        assert_eq!(
            plan_for(&["-m", "gpt-6-astra", "-c", "model_reasoning_effort=xhigh", "--no-daemon"]),
            args(&[
                "codex", "resume", id, "-m", "gpt-6-astra", "-c", "model_reasoning_effort=xhigh",
                "--no-daemon"
            ])
        );
        // only the effort is missing
        assert_eq!(
            plan_for(&["-p", "lorefs", "-m", "gpt-6-astra"]),
            args(&[
                "codex", "resume", id, "-p", "lorefs", "-m", "gpt-6-astra", "-c",
                "model_reasoning_effort=max"
            ])
        );
        // no rollout for the session: the recorded flags alone
        let unknown = AgentSessionRef::id("01a18ce6-0000-7c01-9098-fee91706852d").unwrap();
        assert_eq!(
            plan_for_restore("herdr:codex", "codex", &unknown, &args(&["--no-daemon"]), &home)
                .unwrap()
                .argv,
            args(&["codex", "resume", "01a18ce6-0000-7c01-9098-fee91706852d", "--no-daemon"])
        );
        // other agents are planned as before
        let claude = AgentSessionRef::id("s").unwrap();
        assert_eq!(
            plan_for_restore("herdr:claude", "claude", &claude, &args(&["--effort", "high"]), &home)
                .unwrap()
                .argv,
            args(&["claude", "--resume", "s", "--effort", "high"])
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn planner_allows_supported_agents() {
        let pi_session = absolute_test_path("pi-session.jsonl");
        let omp_session = absolute_test_path("omp-session.jsonl");
        assert_eq!(
            plan(
                "herdr:claude",
                "claude",
                &AgentSessionRef::id("claude-session").unwrap()
            )
            .unwrap()
            .argv,
            vec!["claude", "--resume", "claude-session"]
        );
        assert_eq!(
            plan(
                "herdr:codex",
                "codex",
                &AgentSessionRef::id("codex-session").unwrap()
            )
            .unwrap()
            .argv,
            vec!["codex", "resume", "codex-session"]
        );
        assert_eq!(
            plan(
                "herdr:copilot",
                "copilot",
                &AgentSessionRef::id("copilot-session").unwrap()
            )
            .unwrap()
            .argv,
            vec!["copilot", "--resume=copilot-session"]
        );
        assert_eq!(
            plan(
                "herdr:devin",
                "devin",
                &AgentSessionRef::id("devin-session").unwrap()
            )
            .unwrap()
            .argv,
            vec!["devin", "--resume", "devin-session"]
        );
        assert_eq!(
            plan(
                "herdr:droid",
                "droid",
                &AgentSessionRef::id("droid-session").unwrap()
            )
            .unwrap()
            .argv,
            vec!["droid", "--resume", "droid-session"]
        );
        assert_eq!(
            plan(
                "herdr:kimi",
                "kimi",
                &AgentSessionRef::id("kimi-session").unwrap()
            )
            .unwrap()
            .argv,
            vec!["kimi", "--session", "kimi-session"]
        );
        assert_eq!(
            plan(
                "herdr:mastracode",
                "mastracode",
                &AgentSessionRef::id("mastracode-session").unwrap()
            )
            .unwrap()
            .argv,
            vec!["mastracode", "--thread", "mastracode-session"]
        );
        assert_eq!(
            plan(
                "herdr:pi",
                "pi",
                &AgentSessionRef::path(&pi_session).unwrap()
            )
            .unwrap()
            .argv,
            vec!["pi", "--session", pi_session.as_str()]
        );
        assert_eq!(
            plan(
                "herdr:omp",
                "omp",
                &AgentSessionRef::path(&omp_session).unwrap()
            )
            .unwrap()
            .argv,
            vec!["omp", format!("--resume={omp_session}").as_str()]
        );
        assert_eq!(
            plan(
                "herdr:hermes",
                "hermes",
                &AgentSessionRef::id("hermes-session").unwrap()
            )
            .unwrap()
            .argv,
            vec!["hermes", "--resume", "hermes-session"]
        );
        assert_eq!(
            plan(
                "herdr:opencode",
                "opencode",
                &AgentSessionRef::id("opencode-session").unwrap()
            )
            .unwrap()
            .argv,
            vec!["opencode", "--session", "opencode-session"]
        );
        assert_eq!(
            plan(
                "herdr:qodercli",
                "qodercli",
                &AgentSessionRef::id("qoder-session").unwrap()
            )
            .unwrap()
            .argv,
            vec!["qodercli", "--resume", "qoder-session"]
        );
        assert_eq!(
            plan(
                "herdr:qwen",
                "qwen",
                &AgentSessionRef::id("qwen-session").unwrap()
            )
            .unwrap()
            .argv,
            vec!["qwen", "--resume", "qwen-session"]
        );
        assert_eq!(
            plan(
                "herdr:kilo",
                "kilo",
                &AgentSessionRef::id("kilo-session").unwrap()
            )
            .unwrap()
            .argv,
            vec!["kilo", "--session", "kilo-session"]
        );
        assert_eq!(
            plan(
                "herdr:cursor",
                "cursor",
                &AgentSessionRef::id("cursor-session").unwrap()
            )
            .unwrap()
            .argv,
            vec![
                if cfg!(windows) {
                    "cursor-agent.cmd"
                } else {
                    "cursor-agent"
                },
                "--resume",
                "cursor-session",
            ]
        );
        assert_eq!(
            plan(
                "herdr:antigravity_cli",
                "agy",
                &AgentSessionRef::id("agy-session").unwrap()
            )
            .unwrap()
            .argv,
            vec!["agy", "--conversation", "agy-session"]
        );
        assert_eq!(
            plan(
                "herdr:grok",
                "grok",
                &AgentSessionRef::id("grok-session").unwrap()
            )
            .unwrap()
            .argv,
            vec!["grok", "--resume", "grok-session"]
        );
        assert_eq!(
            plan(
                "herdr:letta",
                "letta",
                &AgentSessionRef::id("conversation-123").unwrap()
            )
            .unwrap()
            .argv,
            vec!["letta", "--conversation", "conversation-123"]
        );
        assert_eq!(
            plan(
                "herdr:letta",
                "letta",
                &AgentSessionRef::id("default:agent-123").unwrap()
            )
            .unwrap()
            .argv,
            vec!["letta", "--conversation", "default", "--agent", "agent-123"]
        );
        assert!(plan(
            "herdr:letta",
            "letta",
            &AgentSessionRef::id("default:").unwrap()
        )
        .is_none());
    }

    #[test]
    fn planner_rejects_custom_and_unsupported_path_refs() {
        let claude_session = absolute_test_path("claude-session");
        assert!(plan(
            "custom:claude",
            "claude",
            &AgentSessionRef::id("session").unwrap()
        )
        .is_none());
        assert!(plan(
            "herdr:claude",
            "claude",
            &AgentSessionRef::path(&claude_session).unwrap()
        )
        .is_none());
    }

    #[test]
    fn report_ref_prefers_pi_and_omp_paths_and_validates_values() {
        let pi_session = absolute_test_path("pi-session.jsonl");
        let omp_session = absolute_test_path("omp-session.jsonl");
        let claude_session = absolute_test_path("claude-session");
        let copilot_session = absolute_test_path("copilot-session");
        let session_ref = session_ref_from_report(
            "herdr:pi",
            "pi",
            Some("pi-id".into()),
            Some(pi_session.clone()),
        )
        .unwrap();
        assert_eq!(session_ref.kind, AgentSessionRefKind::Path);
        assert_eq!(session_ref.value, pi_session);

        assert!(session_ref_from_report("herdr:pi", "pi", Some("bad\nid".into()), None).is_none());
        assert!(
            session_ref_from_report("herdr:pi", "pi", None, Some("relative.jsonl".into()))
                .is_none()
        );
        assert!(session_ref_from_report("custom:pi", "pi", Some("pi-id".into()), None).is_none());

        let session_ref = session_ref_from_report(
            "herdr:omp",
            "omp",
            Some("omp-id".into()),
            Some(omp_session.clone()),
        )
        .unwrap();
        assert_eq!(session_ref.kind, AgentSessionRefKind::Path);
        assert_eq!(session_ref.value, omp_session);

        let session_ref =
            session_ref_from_report("herdr:omp", "omp", Some("omp-id".into()), None).unwrap();
        assert_eq!(session_ref.kind, AgentSessionRefKind::Id);
        assert_eq!(session_ref.value, "omp-id");
        let session_ref = session_ref_from_report(
            "herdr:omp",
            "omp",
            Some("omp-id".into()),
            Some("relative.jsonl".into()),
        )
        .unwrap();
        assert_eq!(session_ref.kind, AgentSessionRefKind::Id);
        assert_eq!(session_ref.value, "omp-id");
        assert!(
            session_ref_from_report("herdr:omp", "omp", None, Some("relative.jsonl".into()))
                .is_none()
        );

        assert!(
            session_ref_from_report("herdr:claude", "claude", None, Some(claude_session)).is_none()
        );

        let session_ref =
            session_ref_from_report("herdr:copilot", "copilot", Some("copilot-id".into()), None)
                .unwrap();
        assert_eq!(session_ref.kind, AgentSessionRefKind::Id);
        assert_eq!(session_ref.value, "copilot-id");
        assert!(
            session_ref_from_report("herdr:copilot", "copilot", None, Some(copilot_session))
                .is_none()
        );

        let session_ref =
            session_ref_from_report("herdr:devin", "devin", Some("devin-id".into()), None).unwrap();
        assert_eq!(session_ref.kind, AgentSessionRefKind::Id);
        assert_eq!(session_ref.value, "devin-id");

        let session_ref =
            session_ref_from_report("herdr:droid", "droid", Some("droid-id".into()), None).unwrap();
        assert_eq!(session_ref.kind, AgentSessionRefKind::Id);
        assert_eq!(session_ref.value, "droid-id");
        assert!(session_ref_from_report(
            "herdr:droid",
            "droid",
            None,
            Some("/tmp/droid-session".into())
        )
        .is_none());

        let session_ref =
            session_ref_from_report("herdr:kimi", "kimi", Some("kimi-id".into()), None).unwrap();
        assert_eq!(session_ref.kind, AgentSessionRefKind::Id);
        assert_eq!(session_ref.value, "kimi-id");

        let session_ref = session_ref_from_report(
            "herdr:mastracode",
            "mastracode",
            Some("mastracode-id".into()),
            None,
        )
        .unwrap();
        assert_eq!(session_ref.kind, AgentSessionRefKind::Id);
        assert_eq!(session_ref.value, "mastracode-id");

        let session_ref =
            session_ref_from_report("herdr:kilo", "kilo", Some("kilo-id".into()), None).unwrap();
        assert_eq!(session_ref.kind, AgentSessionRefKind::Id);
        assert_eq!(session_ref.value, "kilo-id");

        let session_ref =
            session_ref_from_report("herdr:qodercli", "qodercli", Some("qoder-id".into()), None)
                .unwrap();
        assert_eq!(session_ref.kind, AgentSessionRefKind::Id);
        assert_eq!(session_ref.value, "qoder-id");

        let session_ref =
            session_ref_from_report("herdr:qwen", "qwen", Some("qwen-id".into()), None).unwrap();
        assert_eq!(session_ref.kind, AgentSessionRefKind::Id);
        assert_eq!(session_ref.value, "qwen-id");

        let session_ref =
            session_ref_from_report("herdr:antigravity_cli", "agy", Some("agy-id".into()), None)
                .unwrap();
        assert_eq!(session_ref.kind, AgentSessionRefKind::Id);
        assert_eq!(session_ref.value, "agy-id");
    }

    #[test]
    fn normalize_session_start_source_allows_known_values() {
        assert_eq!(
            normalize_session_start_source(Some("startup".into())),
            Some("startup".into())
        );
        assert_eq!(
            normalize_session_start_source(Some("resume".into())),
            Some("resume".into())
        );
        assert_eq!(
            normalize_session_start_source(Some("clear".into())),
            Some("clear".into())
        );
        assert_eq!(
            normalize_session_start_source(Some("compact".into())),
            Some("compact".into())
        );
        assert_eq!(
            normalize_session_start_source(Some("branch".into())),
            Some("branch".into())
        );
        assert_eq!(
            normalize_session_start_source(Some("new".into())),
            Some("new".into())
        );
        assert_eq!(
            normalize_session_start_source(Some("fork".into())),
            Some("fork".into())
        );
        assert_eq!(
            normalize_session_start_source(Some("select".into())),
            Some("select".into())
        );
        assert_eq!(
            normalize_session_start_source(Some(" resume ".into())),
            Some("resume".into())
        );
        assert_eq!(normalize_session_start_source(Some("other".into())), None);
        assert_eq!(normalize_session_start_source(None), None);
    }

    #[test]
    fn ids_are_data_not_shell_text() {
        let id = "abc; rm -rf /";
        let codex_plan = plan("herdr:codex", "codex", &AgentSessionRef::id(id).unwrap()).unwrap();
        assert_eq!(codex_plan.argv, vec!["codex", "resume", id]);

        let copilot_plan = plan(
            "herdr:copilot",
            "copilot",
            &AgentSessionRef::id(id).unwrap(),
        )
        .unwrap();
        assert_eq!(copilot_plan.argv, vec!["copilot", "--resume=abc; rm -rf /"]);

        let devin_plan = plan("herdr:devin", "devin", &AgentSessionRef::id(id).unwrap()).unwrap();
        assert_eq!(devin_plan.argv, vec!["devin", "--resume", id]);
    }

    #[test]
    fn planner_rejects_path_refs_for_id_only_agents() {
        let hermes_session = absolute_test_path("hermes-session");
        let opencode_session = absolute_test_path("opencode-session");
        let kilo_session = absolute_test_path("kilo-session");
        let copilot_session = absolute_test_path("copilot-session");
        let devin_session = absolute_test_path("devin-session");
        assert!(plan(
            "herdr:hermes",
            "hermes",
            &AgentSessionRef::path(&hermes_session).unwrap()
        )
        .is_none());
        assert!(plan(
            "herdr:opencode",
            "opencode",
            &AgentSessionRef::path(&opencode_session).unwrap()
        )
        .is_none());
        assert!(plan(
            "herdr:kilo",
            "kilo",
            &AgentSessionRef::path(&kilo_session).unwrap()
        )
        .is_none());
        assert!(plan(
            "herdr:copilot",
            "copilot",
            &AgentSessionRef::path(&copilot_session).unwrap()
        )
        .is_none());
        assert!(plan(
            "herdr:devin",
            "devin",
            &AgentSessionRef::path(&devin_session).unwrap()
        )
        .is_none());
        assert!(session_ref_from_snapshot(
            "herdr:mastracode",
            "mastracode",
            AgentSessionRefKind::Id,
            "mastracode-session"
        )
        .is_some());
        assert!(session_ref_from_snapshot(
            "herdr:hermes",
            "hermes",
            AgentSessionRefKind::Id,
            "hermes-session"
        )
        .is_some());
        assert!(session_ref_from_snapshot(
            "herdr:opencode",
            "opencode",
            AgentSessionRefKind::Id,
            "opencode-session"
        )
        .is_some());
        assert!(session_ref_from_snapshot(
            "herdr:kilo",
            "kilo",
            AgentSessionRefKind::Id,
            "kilo-session"
        )
        .is_some());
        assert!(session_ref_from_snapshot(
            "herdr:copilot",
            "copilot",
            AgentSessionRefKind::Id,
            "copilot-session"
        )
        .is_some());
        assert!(session_ref_from_snapshot(
            "herdr:devin",
            "devin",
            AgentSessionRefKind::Id,
            "devin-session"
        )
        .is_some());
        assert!(session_ref_from_snapshot(
            "herdr:antigravity_cli",
            "agy",
            AgentSessionRefKind::Id,
            "agy-session"
        )
        .is_some());
        let agy_session = absolute_test_path("agy-session");
        assert!(plan(
            "herdr:antigravity_cli",
            "agy",
            &AgentSessionRef::path(&agy_session).unwrap()
        )
        .is_none());
    }

    #[test]
    fn a_claude_session_record_replaces_the_launch_model_and_effort_and_keeps_the_rest() {
        let dir = std::env::temp_dir().join(format!("herdr-session-records-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let id = "5c0e8d2a-7b41-4e96-a3f5-1d2c9b8e7f60";
        let session = AgentSessionRef::id(id).unwrap();
        let launched = args(&[
            "--model",
            "claude-opus-5-5",
            "--effort",
            "xhigh",
            "--permission-mode",
            "auto",
            "--dangerously-skip-permissions",
        ]);
        let write = |text: &str| std::fs::write(dir.join(format!("{id}.json")), text).unwrap();
        let args_for = |agent: &str, session: &AgentSessionRef| restore_launch_args(&dir, agent, session, &launched);

        write(r#"{"model":"claude-fable-5-2","effort":"low"}"#);
        assert_eq!(
            args_for("claude", &session),
            args(&["--permission-mode", "auto", "--dangerously-skip-permissions", "--model", "claude-fable-5-2", "--effort", "low"])
        );
        // the resume command puts them in its fixed order and clamps nothing for Fable
        let plan = plan_with_launch_args("herdr:claude", "claude", &session, &args_for("claude", &session)).unwrap();
        assert_eq!(
            plan.argv,
            args(&["claude", "--resume", id, "--model", "claude-fable-5-2", "--effort", "low", "--permission-mode", "auto", "--dangerously-skip-permissions"])
        );

        // a record that cannot be used leaves the launch flags alone: unsafe model, no model, not JSON, a missing file
        for unusable in [r#"{"model":"a b","effort":"low"}"#, r#"{"effort":"low"}"#, "not json", ""] {
            write(unusable);
            assert_eq!(args_for("claude", &session), launched, "{unusable:?}");
        }
        std::fs::remove_file(dir.join(format!("{id}.json"))).unwrap();
        assert_eq!(args_for("claude", &session), launched);

        // another agent never reads a Claude record, and a session id that is not a UUID never names a file
        write(r#"{"model":"claude-fable-5-2","effort":"low"}"#);
        assert_eq!(args_for("codex", &session), launched);
        let outside = AgentSessionRef::id("../../etc/passwd").unwrap();
        assert_eq!(args_for("claude", &outside), launched);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
