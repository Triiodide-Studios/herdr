//! A message to a Claude Code session, sent the way another Claude session sends one: through the session's own
//! cross-session pipe, the channel its SendMessage tool uses. It arrives as "Another Claude session sent a message", in the
//! session's message queue, and never touches the input box, so it cannot land in what the user is typing and submit it
//! mangled (`herdr agent prompt` and `herdr broadcast` typed the text into the pane as keystrokes).
//!
//! What a sender has to do (Windows, Claude Code 2.1.30x; read from its own client and tried against a live session):
//! - `~/.claude/sessions/<pid>.json` names a session (`sessionId`), its pipe (`messagingSocketPath`) and the process
//!   (`procStart`, a FILETIME).
//! - `<pid>.<sha256 of the lower-cased pipe path>.key` in the same folder holds the pipe's token (`peerToken`).
//! - One connection, then close: the line `{"type":"auth","token":<token>}`, then the message line
//!   `{"msgV":1,"msg_id":<uuid>,"type":"user","message":{"role":"user","content":<envelope>},"priority":"next"}`, where the
//!   envelope is the text between `<cross-session-message>` tags. There is no reply: a written message is a sent message.
//!
//! The session folder is `$HERDR_CLAUDE_SESSIONS_DIR` or `~/.claude/sessions` (`herdr resolve` reads the same).

use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};

const TAG: &str = "cross-session-message";

/// The most a message line may hold; Claude Code drops a longer one.
const MAX_LINE_BYTES: usize = 1 << 20;

/// How long a busy pipe is retried: a pipe takes one writer at a time, and a session that is receiving frees it in moments.
const PIPE_BUSY_RETRY: Duration = Duration::from_secs(3);

#[derive(Debug)]
pub(crate) enum PeerError {
    /// No session record names the session (it is not a Claude Code session of this user, or its record is gone).
    NoSession(String),
    /// The session's process is not running (a record or key a crash left behind).
    SessionGone(u32),
    /// The session has no key for its pipe, so it takes no messages.
    NoKey(u32),
    TooLarge(usize),
    /// The pipe stayed busy for `PIPE_BUSY_RETRY`.
    Busy,
    Io(io::Error),
}

impl fmt::Display for PeerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoSession(session) => write!(f, "no Claude Code session record names session {session}"),
            Self::SessionGone(pid) => write!(f, "the Claude Code process {pid} of the session is not running"),
            Self::NoKey(pid) => write!(f, "the Claude Code process {pid} has published no messaging key"),
            Self::TooLarge(len) => write!(f, "the message is {len} bytes; Claude Code takes {MAX_LINE_BYTES} at most"),
            Self::Busy => write!(f, "the session's message pipe stayed busy for {PIPE_BUSY_RETRY:?}"),
            Self::Io(err) => write!(f, "writing to the session's message pipe failed: {err}"),
        }
    }
}

/// What a message needs of a session: where to send it and the token that lets it in.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Target {
    pub pid: u32,
    pub pipe: String,
    pub token: String,
}

/// The two things about the machine a send depends on, replaced in tests.
#[derive(Clone, Copy)]
pub(crate) struct Machine {
    /// When the process started, as a FILETIME, or None when it is not running.
    pub process_start: fn(u32) -> Option<u64>,
    /// Writes the bytes to the pipe.
    pub write_pipe: fn(&str, &[u8]) -> Result<(), PeerError>,
}

impl Machine {
    pub(crate) fn real() -> Self {
        Self {
            process_start,
            write_pipe,
        }
    }

    /// The machine a send runs on: the real one, or the one a test set.
    pub(crate) fn current() -> Self {
        #[cfg(test)]
        if let Some(machine) = *TEST_MACHINE.lock().unwrap() {
            return machine;
        }
        Self::real()
    }
}

#[cfg(test)]
pub(crate) static TEST_MACHINE: std::sync::Mutex<Option<Machine>> = std::sync::Mutex::new(None);

pub(crate) fn sessions_dir() -> Option<PathBuf> {
    crate::cli::fleet::sessions_dir()
}

/// Sends `text` to Claude Code session `session` as a cross-session message.
pub(crate) fn deliver(
    session: &str,
    text: &str,
    dir: &Path,
    machine: &Machine,
) -> Result<(), PeerError> {
    let target = find_target(dir, session, machine.process_start)?;
    let wire = wire(&target.token, text, &new_msg_id())?;
    (machine.write_pipe)(&target.pipe, wire.as_bytes())
}

/// The session's live process and key. A session record is rewritten in place and can be read half written: the folder is read
/// again a few times before the session is called unknown.
pub(crate) fn find_target(
    dir: &Path,
    session: &str,
    process_start: fn(u32) -> Option<u64>,
) -> Result<Target, PeerError> {
    let mut last = PeerError::NoSession(session.to_owned());
    for attempt in 0..3 {
        if attempt > 0 {
            std::thread::sleep(Duration::from_millis(50));
        }
        let (records, torn) = read_records(dir, session);
        let mut gone = None;
        for record in records {
            let live = process_start(record.pid);
            if live.is_none() || record.proc_start.is_some_and(|start| Some(start) != live) {
                gone = Some(record.pid);
                continue;
            }
            return key_for(dir, &record, live);
        }
        match gone {
            Some(pid) => last = PeerError::SessionGone(pid),
            None if !torn => return Err(last),
            None => {}
        }
        if gone.is_some() {
            return Err(last);
        }
    }
    Err(last)
}

struct Record {
    pid: u32,
    pipe: String,
    proc_start: Option<u64>,
    pid_domain: Option<String>,
}

/// The records that name `session`, and whether some record could not be read (it may be one being rewritten).
fn read_records(dir: &Path, session: &str) -> (Vec<Record>, bool) {
    let mut out = Vec::new();
    let mut torn = false;
    let Ok(entries) = std::fs::read_dir(dir) else {
        return (out, torn);
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let Ok(raw) = std::fs::read_to_string(&path) else {
            torn = true;
            continue;
        };
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&raw) else {
            torn = true;
            continue;
        };
        if value["sessionId"].as_str() != Some(session) {
            continue;
        }
        let (Some(pid), Some(pipe)) = (
            value["pid"].as_u64(),
            value["messagingSocketPath"].as_str(),
        ) else {
            continue;
        };
        out.push(Record {
            pid: pid as u32,
            pipe: pipe.to_owned(),
            proc_start: value["procStart"].as_str().and_then(|s| s.parse().ok()),
            pid_domain: value["pidDomain"].as_str().map(str::to_owned),
        });
    }
    (out, torn)
}

/// The file name of a pipe's key: the process id, then the SHA-256 of the lower-cased pipe path.
pub(crate) fn key_file_name(pid: u32, pipe: &str) -> String {
    let digest = Sha256::digest(pipe.to_lowercase().as_bytes());
    let hex: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
    format!("{pid}.{hex}.key")
}

fn key_for(dir: &Path, record: &Record, live_start: Option<u64>) -> Result<Target, PeerError> {
    let path = dir.join(key_file_name(record.pid, &record.pipe));
    let Ok(raw) = std::fs::read_to_string(&path) else {
        return Err(PeerError::NoKey(record.pid));
    };
    let Ok(key) = serde_json::from_str::<serde_json::Value>(&raw) else {
        return Err(PeerError::NoKey(record.pid));
    };
    let Some(token) = key["peerToken"].as_str() else {
        return Err(PeerError::NoKey(record.pid));
    };
    // A key a dead process left behind carries that process's start time: another process now has its pid.
    if let (Some(owner), Some(live)) = (key["procStartFt"].as_u64(), live_start) {
        if owner != live {
            return Err(PeerError::SessionGone(record.pid));
        }
    }
    if let (Some(a), Some(b)) = (&record.pid_domain, key["pidDomain"].as_str()) {
        if a != b {
            return Err(PeerError::NoKey(record.pid));
        }
    }
    Ok(Target {
        pid: record.pid,
        pipe: record.pipe.clone(),
        token: token.to_owned(),
    })
}

/// The text as the receiving session renders it: it breaks up the tag of its own envelope, and so does the sender, so that the
/// text is the same on both sides.
pub(crate) fn neutralize(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find('<') {
        out.push_str(&rest[..at]);
        let after = &rest[at + 1..];
        let (closing, name) = match after.strip_prefix('/') {
            Some(name) => (true, name),
            None => (false, after),
        };
        let is_tag = name.len() >= TAG.len() && name[..TAG.len()].eq_ignore_ascii_case(TAG);
        out.push('<');
        if is_tag {
            out.push('\\');
            if closing {
                out.push('/');
            }
            out.push_str(&name[..TAG.len()]);
            rest = &name[TAG.len()..];
        } else {
            rest = after;
        }
    }
    out.push_str(rest);
    out
}

/// What goes over the pipe: the auth line and the message line.
pub(crate) fn wire(token: &str, text: &str, msg_id: &str) -> Result<String, PeerError> {
    let envelope = format!("<{TAG}>\n{}\n</{TAG}>", neutralize(text));
    let message = serde_json::json!({
        "msgV": 1,
        "msg_id": msg_id,
        "type": "user",
        "message": {"role": "user", "content": envelope},
        "priority": "next",
    });
    let message = serde_json::to_string(&message).expect("a message is JSON");
    if message.len() + 1 > MAX_LINE_BYTES {
        return Err(PeerError::TooLarge(message.len() + 1));
    }
    let auth = serde_json::json!({"type": "auth", "token": token});
    Ok(format!("{auth}\n{message}\n"))
}

/// A version-4 UUID from the clock, the process and a counter: it only has to differ from the last message's.
fn new_msg_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    let seed = format!(
        "{nanos}-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    );
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&Sha256::digest(seed.as_bytes())[..16]);
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex: Vec<String> = bytes.iter().map(|b| format!("{b:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        hex[0..4].concat(),
        hex[4..6].concat(),
        hex[6..8].concat(),
        hex[8..10].concat(),
        hex[10..16].concat()
    )
}

/// The start time of process `pid` as a FILETIME, or None when no such process runs.
pub(crate) fn process_start(pid: u32) -> Option<u64> {
    use windows_sys::Win32::Foundation::{CloseHandle, FILETIME};
    use windows_sys::Win32::System::Threading::{
        GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    // SAFETY: the handle is closed on every path, and the FILETIMEs are plain output buffers.
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if handle.is_null() {
            return None;
        }
        let zero = FILETIME {
            dwLowDateTime: 0,
            dwHighDateTime: 0,
        };
        let (mut created, mut exited, mut kernel, mut user) = (zero, zero, zero, zero);
        let ok = GetProcessTimes(handle, &mut created, &mut exited, &mut kernel, &mut user);
        CloseHandle(handle);
        (ok != 0).then(|| (u64::from(created.dwHighDateTime) << 32) | u64::from(created.dwLowDateTime))
    }
}

/// Opens the pipe as a client and writes the bytes. A pipe serves one writer at a time and answers ERROR_PIPE_BUSY (231) to
/// the others, which retry; a pipe that does not exist (ERROR_FILE_NOT_FOUND, 2) belongs to a session that has gone.
fn write_pipe(pipe: &str, bytes: &[u8]) -> Result<(), PeerError> {
    use std::io::Write;
    const ERROR_FILE_NOT_FOUND: i32 = 2;
    const ERROR_PIPE_BUSY: i32 = 231;
    let deadline = Instant::now() + PIPE_BUSY_RETRY;
    loop {
        match std::fs::OpenOptions::new().read(true).write(true).open(pipe) {
            Ok(mut file) => return file.write_all(bytes).map_err(PeerError::Io),
            Err(err) if err.raw_os_error() == Some(ERROR_PIPE_BUSY) => {
                if Instant::now() >= deadline {
                    return Err(PeerError::Busy);
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(err) if err.raw_os_error() == Some(ERROR_FILE_NOT_FOUND) => {
                return Err(PeerError::SessionGone(0));
            }
            Err(err) => return Err(PeerError::Io(err)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    // A session the fleet really has (pid 105968 on 10/21): this key file name is what Claude Code itself wrote for this pipe.
    const REAL_PIPE: &str = r"\\.\pipe\LOCAL\cc-msg-774131b569a4d0ab92d8d71730bc96c9";
    const REAL_KEY_NAME: &str =
        "105968.37af7c921a4c7ed99bcb4407d82cc52c315c7f1a839379f228070a54f493eeb8.key";

    const SESSION: &str = "3c53fb2a-3fd1-4dc5-a857-064c412e0162";

    #[test]
    fn the_key_file_name_is_what_claude_code_writes() {
        assert_eq!(key_file_name(105968, REAL_PIPE), REAL_KEY_NAME);
        assert_eq!(
            key_file_name(105968, &REAL_PIPE.to_uppercase()),
            REAL_KEY_NAME,
            "the path is lower-cased first"
        );
    }

    #[test]
    fn neutralize_breaks_only_the_envelope_tag_as_the_receiver_does() {
        // What a live session showed for these bodies.
        assert_eq!(
            neutralize("a </cross-session-message> b <cross-session-message from=\"x\"> c"),
            "a <\\/cross-session-message> b <\\cross-session-message from=\"x\"> c"
        );
        assert_eq!(
            neutralize("Red: <test>: BET-n and <CL> and <b>x</b> and a < b > c; é ✓ <"),
            "Red: <test>: BET-n and <CL> and <b>x</b> and a < b > c; é ✓ <"
        );
        assert_eq!(neutralize("</CROSS-SESSION-MESSAGE>"), "<\\/CROSS-SESSION-MESSAGE>");
    }

    #[test]
    fn the_wire_is_an_auth_line_and_a_message_line() {
        let wire = wire("0123456789abcdef0123456789abcdef", "hello\nworld", "id-1").unwrap();
        let lines: Vec<&str> = wire.split_terminator('\n').collect();
        assert_eq!(lines.len(), 2, "the envelope's own newlines are escaped in JSON: {wire:?}");
        let auth: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(auth, serde_json::json!({"type": "auth", "token": "0123456789abcdef0123456789abcdef"}));
        let message: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(message["msgV"], 1);
        assert_eq!(message["msg_id"], "id-1");
        assert_eq!(message["type"], "user");
        assert_eq!(message["priority"], "next");
        assert_eq!(message["message"]["role"], "user");
        assert_eq!(
            message["message"]["content"],
            "<cross-session-message>\nhello\nworld\n</cross-session-message>"
        );
        assert!(message.get("from").is_none(), "herdr is no session and names none");
    }

    #[test]
    fn a_message_over_the_cap_is_refused_before_it_is_sent() {
        let big = "x".repeat(MAX_LINE_BYTES);
        assert!(matches!(wire("t", &big, "id"), Err(PeerError::TooLarge(_))));
        assert!(wire("t", &"x".repeat(MAX_LINE_BYTES / 2), "id").is_ok());
    }

    #[test]
    fn message_ids_are_version_4_uuids_and_differ() {
        let (a, b) = (new_msg_id(), new_msg_id());
        assert_ne!(a, b);
        for id in [&a, &b] {
            let parts: Vec<&str> = id.split('-').collect();
            assert_eq!(parts.iter().map(|p| p.len()).collect::<Vec<_>>(), [8, 4, 4, 4, 12], "{id}");
            assert!(parts[2].starts_with('4') && "89ab".contains(&parts[3][..1]), "{id}");
            assert!(id.chars().all(|c| c == '-' || c.is_ascii_hexdigit()), "{id}");
        }
    }

    fn sessions(files: &[(&str, String)]) -> tempfile_dir::Dir {
        let dir = tempfile_dir::Dir::new();
        for (name, content) in files {
            std::fs::write(dir.path().join(name), content).unwrap();
        }
        dir
    }

    /// A temporary folder removed when dropped (no crate for it here).
    mod tempfile_dir {
        use std::path::{Path, PathBuf};
        pub struct Dir(PathBuf);
        impl Dir {
            pub fn new() -> Self {
                static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
                let path = std::env::temp_dir().join(format!(
                    "herdr-claude-peer-{}-{}",
                    std::process::id(),
                    N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                ));
                std::fs::create_dir_all(&path).unwrap();
                Self(path)
            }
            pub fn path(&self) -> &Path {
                &self.0
            }
        }
        impl Drop for Dir {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
    }

    fn record(pid: u32, session: &str, pipe: &str, proc_start: u64) -> String {
        format!(
            r#"{{"pid":{pid},"sessionId":"{session}","messagingSocketPath":{pipe:?},"procStart":"{proc_start}","pidDomain":"win32:trws-blake"}}"#
        )
    }

    fn key(token: &str, start: u64) -> String {
        format!(r#"{{"peerToken":"{token}","procStartFt":{start},"pidDomain":"win32:trws-blake"}}"#)
    }

    const TOKEN: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    fn alive(pid: u32) -> Option<u64> {
        (pid == 105968).then_some(134370848602109159)
    }

    #[test]
    fn a_session_is_found_with_its_pipe_and_token() {
        let dir = sessions(&[
            ("105968.json", record(105968, SESSION, REAL_PIPE, 134370848602109159)),
            (REAL_KEY_NAME, key(TOKEN, 134370848602109159)),
            ("7.json", record(7, "other-session", r"\\.\pipe\LOCAL\cc-msg-1", 5)),
        ]);
        let target = find_target(dir.path(), SESSION, alive).unwrap();
        assert_eq!(
            target,
            Target { pid: 105968, pipe: REAL_PIPE.into(), token: TOKEN.into() }
        );
    }

    #[test]
    fn a_session_that_cannot_take_a_message_says_why() {
        let started = 134370848602109159;
        for (name, files, want) in [
            ("no record names it", vec![], "no Claude Code session record"),
            (
                "its process has gone",
                vec![("999.json", record(999, SESSION, REAL_PIPE, 1))],
                "999 of the session is not running",
            ),
            (
                "its pid belongs to another process now",
                vec![("105968.json", record(105968, SESSION, REAL_PIPE, 42))],
                "105968 of the session is not running",
            ),
            (
                "it published no key",
                vec![("105968.json", record(105968, SESSION, REAL_PIPE, started))],
                "published no messaging key",
            ),
            (
                "its key is a dead process's",
                vec![
                    ("105968.json", record(105968, SESSION, REAL_PIPE, started)),
                    (REAL_KEY_NAME, key(TOKEN, 42)),
                ],
                "105968 of the session is not running",
            ),
            (
                "its key has no token",
                vec![
                    ("105968.json", record(105968, SESSION, REAL_PIPE, started)),
                    (REAL_KEY_NAME, r#"{"procStartFt":1}"#.to_string()),
                ],
                "published no messaging key",
            ),
        ] {
            let dir = sessions(&files);
            let err = find_target(dir.path(), SESSION, alive).unwrap_err().to_string();
            assert!(err.contains(want), "{name}: {err}");
        }
    }

    static SENT: Mutex<Vec<(String, Vec<u8>)>> = Mutex::new(Vec::new());

    fn record_write(pipe: &str, bytes: &[u8]) -> Result<(), PeerError> {
        SENT.lock().unwrap().push((pipe.to_owned(), bytes.to_vec()));
        Ok(())
    }

    #[test]
    fn deliver_writes_the_authenticated_message_to_the_sessions_pipe() {
        let dir = sessions(&[
            ("105968.json", record(105968, SESSION, REAL_PIPE, 134370848602109159)),
            (REAL_KEY_NAME, key(TOKEN, 134370848602109159)),
        ]);
        let machine = Machine { process_start: alive, write_pipe: record_write };
        deliver(SESSION, "wake up: <CL> failed", dir.path(), &machine).unwrap();
        let sent = SENT.lock().unwrap();
        let (pipe, bytes) = sent.last().unwrap();
        assert_eq!(pipe, REAL_PIPE);
        let wire = String::from_utf8(bytes.clone()).unwrap();
        assert!(wire.starts_with(&format!("{{\"token\":\"{TOKEN}\",\"type\":\"auth\"}}\n")), "{wire}");
        assert!(wire.contains("wake up: <CL> failed"), "{wire}");
        assert!(wire.ends_with('\n'), "a line each");
    }

    /// Sends one message to the live session named by $HERDR_LIVE_PEER_SESSION, through the real pipe and keys: run by hand
    /// (`cargo test live_session -- --ignored`) against a throwaway session, whose reply shows it arrived.
    #[test]
    #[ignore = "needs a live Claude Code session"]
    fn live_session_takes_a_message() {
        let session = std::env::var("HERDR_LIVE_PEER_SESSION").expect("HERDR_LIVE_PEER_SESSION");
        let text = std::env::var("HERDR_LIVE_PEER_TEXT").unwrap_or_else(|_| "Reply with only the word PONG.".into());
        let dir = sessions_dir().expect("a sessions folder");
        deliver(&session, &text, &dir, &Machine::real()).unwrap();
    }
}