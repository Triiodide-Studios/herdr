use std::collections::VecDeque;
use std::fmt;
use std::fs::File;
use std::io::{self, BufRead, BufReader, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use regex::Regex;
use serde::de::{Deserializer, SeqAccess, Visitor};
use serde::Deserialize;

use super::keywords::{add_terms, merge_terms, prune_terms, TermCounts, MAX_TERMS_PER_CHAT};

/// The newest text messages of a chat are left out of its summary: they change on every turn and
/// say what the chat is doing now, not what it is about.
pub(super) const TAIL_MESSAGES: usize = 8;
/// A single line this large is a pasted log or an embedded file, not conversation.
const MAX_LINE_BYTES: usize = 4 * 1024 * 1024;
const MAX_MESSAGE_CHARS: usize = 20_000;
const USER_WEIGHT: f32 = 2.0;
const ASSISTANT_WEIGHT: f32 = 1.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Role {
    User,
    Assistant,
}

impl Role {
    fn weight(self) -> f32 {
        match self {
            Role::User => USER_WEIGHT,
            Role::Assistant => ASSISTANT_WEIGHT,
        }
    }
}

/// What has been read of one Claude Code transcript (an append-only JSON-lines file).
#[derive(Clone, Debug, Default)]
pub(super) struct TranscriptSummary {
    /// Bytes of the transcript already folded into `terms`; the next read starts here.
    pub(super) offset: u64,
    /// Weighted term counts of every text message before the held-back tail.
    pub(super) terms: TermCounts,
}

#[derive(Deserialize)]
struct Entry {
    #[serde(rename = "type")]
    kind: String,
    #[serde(rename = "isMeta", default)]
    is_meta: bool,
    #[serde(default)]
    message: Option<Message>,
}

#[derive(Deserialize)]
struct Message {
    #[serde(default, deserialize_with = "text_pieces")]
    content: Vec<String>,
}

#[derive(Deserialize)]
struct Block {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    text: Option<String>,
}

/// Message content is a string (a typed prompt) or a list of blocks. Only text blocks are kept;
/// tool results and tool inputs, the bulk of a transcript, are skipped without being copied.
fn text_pieces<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<String>, D::Error> {
    struct Pieces;

    impl<'de> Visitor<'de> for Pieces {
        type Value = Vec<String>;

        fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
            formatter.write_str("message content")
        }

        fn visit_str<E: serde::de::Error>(self, value: &str) -> Result<Self::Value, E> {
            Ok(vec![value.to_owned()])
        }

        fn visit_unit<E: serde::de::Error>(self) -> Result<Self::Value, E> {
            Ok(Vec::new())
        }

        fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
            let mut pieces = Vec::new();
            while let Some(block) = seq.next_element::<Block>()? {
                if block.kind == "text" {
                    pieces.extend(block.text);
                }
            }
            Ok(pieces)
        }
    }

    deserializer.deserialize_any(Pieces)
}

/// The conversation text of one transcript line, or `None` for lines that are not a user or
/// assistant text message (tool results, thinking, system entries, injected context).
pub(super) fn message_text(line: &str) -> Option<(Role, String)> {
    if !line.contains("\"type\":\"user\"") && !line.contains("\"type\":\"assistant\"") {
        return None;
    }
    let entry: Entry = serde_json::from_str(line).ok()?;
    let role = match entry.kind.as_str() {
        "user" => Role::User,
        "assistant" => Role::Assistant,
        _ => return None,
    };
    if entry.is_meta {
        return None;
    }
    let mut text = String::new();
    for piece in entry.message?.content {
        let piece = conversation_text(&piece);
        if !piece.is_empty() {
            text.push_str(&piece);
            text.push('\n');
        }
    }
    if let Some((end, _)) = text.char_indices().nth(MAX_MESSAGE_CHARS) {
        text.truncate(end);
    }
    (!text.trim().is_empty()).then_some((role, text))
}

/// Text the person or the model wrote, without the wrappers Claude Code puts around it.
fn conversation_text(piece: &str) -> String {
    static MARKUP: OnceLock<(Regex, Regex)> = OnceLock::new();
    let (reminders, tags) = MARKUP.get_or_init(|| {
        (
            Regex::new(r"(?s)<system-reminder>.*?</system-reminder>").expect("static pattern"),
            Regex::new(r"</?[A-Za-z][^>\n]{0,120}>").expect("static pattern"),
        )
    });
    let trimmed = piece.trim_start();
    if [
        "<local-command",
        "<command-",
        "<task-notification",
        "[Request interrupted",
    ]
    .iter()
    .any(|prefix| trimmed.starts_with(prefix))
    {
        return String::new();
    }
    let without_reminders = reminders.replace_all(piece, " ");
    tags.replace_all(&without_reminders, " ").trim().to_owned()
}

/// Reads the transcript from where the last read stopped and folds the messages older than the
/// newest `tail` into `summary`. The held-back messages are read again next time, so each
/// message is counted exactly once, and a transcript that shrank starts over.
pub(super) fn advance(path: &Path, summary: &mut TranscriptSummary, tail: usize) -> io::Result<()> {
    let mut file = File::open(path)?;
    if file.metadata()?.len() < summary.offset {
        *summary = TranscriptSummary::default();
    }
    file.seek(SeekFrom::Start(summary.offset))?;
    let mut reader = BufReader::with_capacity(1 << 20, file);
    let mut position = summary.offset;
    let mut line = Vec::new();
    let mut held_back = VecDeque::<(u64, TermCounts)>::new();
    loop {
        line.clear();
        if reader.read_until(b'\n', &mut line)? == 0 || line.last() != Some(&b'\n') {
            // End of file, or a last line the writer has not finished.
            break;
        }
        position += line.len() as u64;
        if line.len() > MAX_LINE_BYTES {
            continue;
        }
        let Some((role, text)) = std::str::from_utf8(&line).ok().and_then(message_text) else {
            continue;
        };
        let mut counts = TermCounts::new();
        add_terms(&mut counts, &text, role.weight());
        held_back.push_back((position, counts));
        if held_back.len() > tail {
            if let Some((end, counts)) = held_back.pop_front() {
                merge_terms(&mut summary.terms, counts);
                summary.offset = end;
            }
        }
        if summary.terms.len() > 2 * MAX_TERMS_PER_CHAT {
            prune_terms(&mut summary.terms, MAX_TERMS_PER_CHAT);
        }
    }
    prune_terms(&mut summary.terms, MAX_TERMS_PER_CHAT);
    Ok(())
}

/// Claude Code's configuration directory: `CLAUDE_CONFIG_DIR`, else `~/.claude`.
pub(super) fn claude_config_dir() -> Option<PathBuf> {
    let variable = |name: &str| std::env::var_os(name).filter(|value| !value.is_empty());
    variable("CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .or_else(|| {
            variable("USERPROFILE")
                .or_else(|| variable("HOME"))
                .map(|home| PathBuf::from(home).join(".claude"))
        })
}

/// The transcript of session `session`, which lives under one of the per-project directories.
pub(super) fn find_transcript(config_dir: &Path, session: &str) -> Option<PathBuf> {
    if session.is_empty()
        || !session
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-')
    {
        return None;
    }
    std::fs::read_dir(config_dir.join("projects"))
        .ok()?
        .flatten()
        .map(|project| project.path().join(format!("{session}.jsonl")))
        .find(|candidate| candidate.is_file())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn user(text: &str) -> String {
        format!(
            r#"{{"type":"user","message":{{"role":"user","content":{}}}}}"#,
            serde_json::json!(text)
        )
    }

    fn assistant(text: &str) -> String {
        format!(
            r#"{{"type":"assistant","message":{{"role":"assistant","content":[{{"type":"thinking","thinking":"hmm"}},{{"type":"text","text":{}}},{{"type":"tool_use","name":"Read","input":{{"file_path":"zebra.rs"}}}}]}}}}"#,
            serde_json::json!(text)
        )
    }

    #[test]
    fn typed_prompts_and_assistant_text_are_conversation() {
        assert_eq!(
            message_text(&user("fix the sunlight leak")),
            Some((Role::User, "fix the sunlight leak\n".to_owned()))
        );
        assert_eq!(
            message_text(&assistant("The sunlight leak is in lumen")),
            Some((
                Role::Assistant,
                "The sunlight leak is in lumen\n".to_owned()
            ))
        );
    }

    #[test]
    fn tool_traffic_and_injected_context_are_not() {
        let tool_result = r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t","content":"zebra zebra zebra"}]}}"#;
        assert_eq!(message_text(tool_result), None);
        let meta = r#"{"type":"user","isMeta":true,"message":{"role":"user","content":"zebra"}}"#;
        assert_eq!(message_text(meta), None);
        let system = r#"{"type":"system","content":"zebra"}"#;
        assert_eq!(message_text(system), None);
        assert_eq!(
            message_text(&user("<command-name>/model</command-name>")),
            None
        );
        assert_eq!(
            message_text(&user(
                "<system-reminder>zebra</system-reminder>real <b>words</b>"
            )),
            Some((Role::User, "real  words\n".to_owned()))
        );
    }

    struct TempTranscript(PathBuf);

    impl TempTranscript {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir()
                .join(format!("herdr-chat-search-{name}-{}", std::process::id()));
            std::fs::create_dir_all(&dir).expect("temp dir");
            Self(dir.join("session.jsonl"))
        }

        fn append(&self, lines: &[String]) {
            let mut file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.0)
                .expect("open transcript");
            for line in lines {
                writeln!(file, "{line}").expect("write line");
            }
        }

        fn append_partial(&self, text: &str) {
            let mut file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.0)
                .expect("open transcript");
            write!(file, "{text}").expect("write text");
        }
    }

    impl Drop for TempTranscript {
        fn drop(&mut self) {
            if let Some(dir) = self.0.parent() {
                let _ = std::fs::remove_dir_all(dir);
            }
        }
    }

    fn weight(summary: &TranscriptSummary, term: &str) -> f32 {
        summary.terms.get(term).copied().unwrap_or(0.0)
    }

    #[test]
    fn the_newest_messages_stay_out_and_nothing_is_counted_twice() {
        let transcript = TempTranscript::new("tail");
        let mut lines = vec![user("alpha")];
        for _ in 0..3 {
            lines.push(r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","content":"noise"}]}}"#.to_owned());
        }
        lines.extend(["beta", "gamma", "delta"].map(user));
        transcript.append(&lines);

        let mut summary = TranscriptSummary::default();
        advance(&transcript.0, &mut summary, 2).expect("advance");
        assert_eq!(weight(&summary, "alpha"), USER_WEIGHT);
        assert_eq!(weight(&summary, "beta"), USER_WEIGHT);
        assert_eq!(weight(&summary, "gamma"), 0.0);
        assert_eq!(weight(&summary, "delta"), 0.0);

        advance(&transcript.0, &mut summary, 2).expect("advance again");
        assert_eq!(
            weight(&summary, "alpha"),
            USER_WEIGHT,
            "re-reading must not double count"
        );
        assert_eq!(weight(&summary, "beta"), USER_WEIGHT);

        transcript.append(&[user("epsilon"), assistant("zeta eta")]);
        advance(&transcript.0, &mut summary, 2).expect("advance after append");
        assert_eq!(weight(&summary, "gamma"), USER_WEIGHT);
        assert_eq!(weight(&summary, "delta"), USER_WEIGHT);
        assert_eq!(weight(&summary, "epsilon"), 0.0);
        assert_eq!(weight(&summary, "zeta"), 0.0);
        assert_eq!(
            weight(&summary, "zebra"),
            0.0,
            "tool inputs are not conversation"
        );
    }

    #[test]
    fn a_line_still_being_written_waits() {
        let transcript = TempTranscript::new("partial");
        transcript.append(&[user("alpha"), user("bravo")]);
        transcript.append_partial(&user("charlie")[..20]);
        let mut summary = TranscriptSummary::default();
        advance(&transcript.0, &mut summary, 0).expect("advance");
        assert_eq!(weight(&summary, "alpha"), USER_WEIGHT);
        assert_eq!(weight(&summary, "bravo"), USER_WEIGHT);
        assert_eq!(weight(&summary, "charlie"), 0.0);

        transcript.append_partial(&user("charlie")[20..]);
        transcript.append_partial("\n");
        advance(&transcript.0, &mut summary, 0).expect("advance the finished line");
        assert_eq!(weight(&summary, "charlie"), USER_WEIGHT);
    }

    #[test]
    fn a_replaced_shorter_transcript_starts_over() {
        let transcript = TempTranscript::new("shrink");
        transcript.append(&[user("long"), user("longer"), user("longest")]);
        let mut summary = TranscriptSummary::default();
        advance(&transcript.0, &mut summary, 0).expect("advance");
        assert!(summary.offset > 0);
        std::fs::write(&transcript.0, format!("{}\n", user("fresh"))).expect("replace");
        advance(&transcript.0, &mut summary, 0).expect("advance replaced");
        assert_eq!(weight(&summary, "fresh"), USER_WEIGHT);
        assert_eq!(weight(&summary, "longest"), 0.0);
    }

    #[test]
    fn session_ids_cannot_escape_the_projects_directory() {
        let dir = std::env::temp_dir();
        assert_eq!(find_transcript(&dir, "../secret"), None);
        assert_eq!(find_transcript(&dir, ""), None);
    }
}
