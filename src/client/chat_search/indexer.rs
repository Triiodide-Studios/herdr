use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tracing::{debug, warn};

use super::embed::{model_dir, Embedder, StaticEmbedder};
use super::index::{ChatSearchIndex, ChatText, IndexSnapshot, IndexStatus};
use super::keywords::{document_frequency, top_keywords};
use super::transcript::{
    advance, claude_config_dir, find_transcript, TranscriptSummary, TAIL_MESSAGES,
};

/// How often the background thread looks at the chats again when nothing asks it to sooner.
const REFRESH_INTERVAL: Duration = Duration::from_secs(20);
const KEYWORDS_PER_CHAT: usize = 24;

/// A chat as the server lists it.
#[derive(Clone, Debug)]
pub(super) struct AgentFacts {
    pub(super) pane_id: String,
    pub(super) names: Vec<String>,
    pub(super) title: String,
    /// The Claude Code session id, for chats whose transcript can be read.
    pub(super) session: Option<String>,
}

/// The text to index for each chat: its names, its title, and the keywords that set its
/// conversation apart from the other chats' (so a word every chat uses is not a keyword).
pub(super) fn chat_texts(
    agents: &[AgentFacts],
    summaries: &HashMap<String, TranscriptSummary>,
) -> Vec<ChatText> {
    let summary_of = |agent: &AgentFacts| agent.session.as_ref().and_then(|id| summaries.get(id));
    let chats = agents.iter().filter_map(summary_of).count();
    let frequency = document_frequency(agents.iter().filter_map(summary_of).map(|s| &s.terms));
    agents
        .iter()
        .map(|agent| ChatText {
            endpoint_id: crate::client::endpoint::ClientEndpointId::Local,
            pane_id: agent.pane_id.clone(),
            names: agent.names.clone(),
            title: agent.title.clone(),
            keywords: summary_of(agent)
                .map(|summary| top_keywords(&summary.terms, &frequency, chats, KEYWORDS_PER_CHAT))
                .unwrap_or_default(),
        })
        .collect()
}

enum ModelState {
    NotLoaded,
    Loaded(Arc<dyn Embedder>),
    Failed(String),
}

/// What the background thread keeps between refreshes: how far each transcript has been read.
pub(super) struct IndexerState {
    claude_dir: Option<PathBuf>,
    model_dir: PathBuf,
    model: ModelState,
    summaries: HashMap<String, TranscriptSummary>,
    transcripts: HashMap<String, PathBuf>,
}

impl IndexerState {
    pub(super) fn new() -> Self {
        Self {
            claude_dir: claude_config_dir(),
            model_dir: model_dir(),
            model: ModelState::NotLoaded,
            summaries: HashMap::new(),
            transcripts: HashMap::new(),
        }
    }

    #[cfg(test)]
    pub(super) fn for_test(claude_dir: PathBuf, embedder: Option<Arc<dyn Embedder>>) -> Self {
        Self {
            claude_dir: Some(claude_dir),
            model_dir: PathBuf::from("no-model"),
            model: embedder.map_or(ModelState::NotLoaded, ModelState::Loaded),
            summaries: HashMap::new(),
            transcripts: HashMap::new(),
        }
    }

    /// Reads the new conversation of `agents` and returns the next snapshot, or `None` when
    /// nothing changed since `previous`.
    #[cfg(test)]
    pub(super) fn refresh(
        &mut self,
        previous: &IndexSnapshot,
        agents: &[AgentFacts],
    ) -> Option<IndexSnapshot> {
        self.refresh_with(previous, agents, Vec::new())
    }

    /// [`Self::refresh`], also indexing `pushed`: chats of other machines, known by their names
    /// and titles from the machines' snapshots.
    pub(super) fn refresh_with(
        &mut self,
        previous: &IndexSnapshot,
        agents: &[AgentFacts],
        pushed: Vec<ChatText>,
    ) -> Option<IndexSnapshot> {
        let embedder = self.embedder();
        let status = match &self.model {
            ModelState::Loaded(_) => IndexStatus::Ready,
            ModelState::Failed(error) => IndexStatus::ModelUnavailable(error.clone()),
            ModelState::NotLoaded => {
                IndexStatus::ModelUnavailable(format!("no model in {}", self.model_dir.display()))
            }
        };
        for session in agents.iter().filter_map(|agent| agent.session.as_deref()) {
            self.read_transcript(session);
        }
        self.summaries.retain(|session, _| {
            agents
                .iter()
                .any(|agent| agent.session.as_deref() == Some(session.as_str()))
        });
        let mut texts = chat_texts(agents, &self.summaries);
        texts.extend(pushed);
        let unchanged = previous.generation() > 0
            && previous.status() == &status
            && previous.has_embedder() == embedder.is_some()
            && previous.texts() == texts;
        (!unchanged).then(|| IndexSnapshot::build(previous, status, texts, embedder))
    }

    fn embedder(&mut self) -> Option<Arc<dyn Embedder>> {
        if matches!(self.model, ModelState::NotLoaded) && self.model_dir.is_dir() {
            self.model = match StaticEmbedder::load(&self.model_dir) {
                Ok(embedder) => ModelState::Loaded(Arc::new(embedder)),
                Err(error) => {
                    warn!(%error, dir = %self.model_dir.display(), "chat search model failed to load");
                    ModelState::Failed(error)
                }
            };
        }
        match &self.model {
            ModelState::Loaded(embedder) => Some(embedder.clone()),
            ModelState::NotLoaded | ModelState::Failed(_) => None,
        }
    }

    fn read_transcript(&mut self, session: &str) {
        let Some(claude_dir) = self.claude_dir.as_deref() else {
            return;
        };
        let path = match self.transcripts.get(session) {
            Some(path) => path.clone(),
            None => {
                let Some(path) = find_transcript(claude_dir, session) else {
                    return;
                };
                self.transcripts.insert(session.to_owned(), path.clone());
                path
            }
        };
        let summary = self.summaries.entry(session.to_owned()).or_default();
        if let Err(error) = advance(&path, summary, TAIL_MESSAGES) {
            debug!(%error, path = %path.display(), "chat search could not read a transcript");
            self.transcripts.remove(session);
        }
    }
}

/// Every chat on the local server, with the Claude session behind it when there is one.
pub(super) fn list_agents() -> Result<Vec<AgentFacts>, String> {
    use crate::api::schema::{EmptyParams, Method, Request, ResponseResult};

    let reply = crate::api::client::ApiClient::local()
        .request(Request {
            id: "client:chat-search:agents".into(),
            method: Method::AgentList(EmptyParams {}),
        })
        .map_err(|error| error.to_string())?;
    let ResponseResult::AgentList { agents } = reply.result else {
        return Err("agent.list did not return agents".into());
    };
    Ok(agents
        .into_iter()
        .map(|agent| AgentFacts {
            session: agent
                .agent_session
                .filter(|session| {
                    session.agent == "claude"
                        && session.kind == crate::agent_resume::AgentSessionRefKind::Id
                })
                .map(|session| session.value),
            names: [agent.display_agent, agent.name]
                .into_iter()
                .flatten()
                .collect(),
            title: agent
                .title
                .or(agent.terminal_title_stripped)
                .unwrap_or_default(),
            pane_id: agent.pane_id,
        })
        .collect())
}

impl ChatSearchIndex {
    /// An index kept current by a background thread for as long as the process lives. With
    /// `indexes_local`, the thread lists this machine's chats and reads their transcripts; either
    /// way it indexes the chats the window pushes from the other machines' snapshots.
    pub(crate) fn start(indexes_local: bool) -> Self {
        let index = Self::with_snapshot_indexing_local(
            IndexSnapshot::empty(IndexStatus::Starting),
            indexes_local,
        );
        let worker = index.clone();
        let spawned = std::thread::Builder::new()
            .name("chat-search-indexer".into())
            .spawn(move || {
                let mut state = IndexerState::new();
                loop {
                    let agents = if indexes_local {
                        list_agents()
                    } else {
                        Ok(Vec::new())
                    };
                    match agents {
                        Ok(agents) => {
                            let previous = worker.snapshot();
                            let pushed = worker.pushed_chats();
                            if let Some(next) = state.refresh_with(&previous, &agents, pushed) {
                                worker.publish(next);
                            }
                        }
                        Err(error) => debug!(%error, "chat search could not list the chats"),
                    }
                    worker.wait_for_refresh(REFRESH_INTERVAL);
                }
            });
        if let Err(error) = spawned {
            warn!(%error, "chat search indexer did not start");
        }
        index
    }
}

#[cfg(test)]
mod tests {
    use super::super::index::test_support::OneHotWords;
    use super::*;
    use std::io::Write;

    struct TempClaudeDir(PathBuf);

    impl TempClaudeDir {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir()
                .join(format!("herdr-chat-indexer-{name}-{}", std::process::id()));
            std::fs::create_dir_all(dir.join("projects").join("repo")).expect("temp dir");
            Self(dir)
        }

        fn write_session(&self, session: &str, user_messages: &[&str]) {
            let mut file = std::fs::File::create(
                self.0
                    .join("projects")
                    .join("repo")
                    .join(format!("{session}.jsonl")),
            )
            .expect("create transcript");
            for message in user_messages {
                writeln!(
                    file,
                    r#"{{"type":"user","message":{{"role":"user","content":{}}}}}"#,
                    serde_json::json!(message)
                )
                .expect("write line");
            }
        }
    }

    impl Drop for TempClaudeDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn facts(pane_id: &str, session: Option<&str>, title: &str) -> AgentFacts {
        AgentFacts {
            pane_id: pane_id.into(),
            names: vec![format!("name-{pane_id}")],
            title: title.into(),
            session: session.map(String::from),
        }
    }

    #[test]
    fn keywords_come_from_the_conversation_without_its_newest_messages() {
        let claude = TempClaudeDir::new("keywords");
        let newest = ["zebra stripes"; TAIL_MESSAGES];
        let audio = ["subbass solver rumble"; 2]
            .into_iter()
            .chain(newest)
            .collect::<Vec<_>>();
        let render = ["lumen sunlight"; 2]
            .into_iter()
            .chain(newest)
            .collect::<Vec<_>>();
        claude.write_session("aaaa-1111", &audio);
        claude.write_session("bbbb-2222", &render);

        let mut state = IndexerState::for_test(claude.0.clone(), Some(Arc::new(OneHotWords)));
        let snapshot = state
            .refresh(
                &IndexSnapshot::empty(IndexStatus::Starting),
                &[
                    facts("w1:p1", Some("aaaa-1111"), "audio"),
                    facts("w1:p2", Some("bbbb-2222"), "render"),
                    facts("w1:p3", None, "no transcript"),
                ],
            )
            .expect("first refresh publishes");
        let texts = snapshot.texts();
        for word in ["subbass", "solver", "rumble"] {
            assert!(texts[0].keywords.contains(&word.to_owned()), "{word}");
        }
        assert!(texts[1].keywords.contains(&"sunlight".to_owned()));
        assert!(!texts[0].keywords.contains(&"sunlight".to_owned()));
        // What both chats' newest messages say is held back, so it is nobody's keyword.
        assert!(texts
            .iter()
            .all(|text| !text.keywords.contains(&"zebra".to_owned())));
        assert!(texts[2].keywords.is_empty());
        assert_eq!(*snapshot.status(), IndexStatus::Ready);
        assert_eq!(
            snapshot
                .search("subbass")
                .first()
                .map(|hit| hit.pane_id.as_str()),
            Some("w1:p1")
        );
    }

    #[test]
    fn a_refresh_that_changes_nothing_publishes_nothing() {
        let claude = TempClaudeDir::new("steady");
        claude.write_session("aaaa-1111", &["subbass solver rumble"]);
        let mut state = IndexerState::for_test(claude.0.clone(), Some(Arc::new(OneHotWords)));
        let agents = [facts("w1:p1", Some("aaaa-1111"), "audio")];
        let first = state
            .refresh(&IndexSnapshot::empty(IndexStatus::Starting), &agents)
            .expect("first refresh publishes");
        assert!(state.refresh(&first, &agents).is_none());
        let renamed = [facts("w1:p1", Some("aaaa-1111"), "audio renamed")];
        assert!(state.refresh(&first, &renamed).is_some());
    }

    #[test]
    fn a_missing_model_is_reported_and_letters_still_search() {
        let claude = TempClaudeDir::new("nomodel");
        let mut state = IndexerState::for_test(claude.0.clone(), None);
        let snapshot = state
            .refresh(
                &IndexSnapshot::empty(IndexStatus::Starting),
                &[facts("w1:p1", None, "audio solver")],
            )
            .expect("publishes the missing model");
        assert!(matches!(
            snapshot.status(),
            IndexStatus::ModelUnavailable(_)
        ));
        assert_eq!(snapshot.search("solver").len(), 1);
    }

    #[test]
    fn pushed_chats_of_other_machines_are_searched_with_this_machines() {
        let claude = TempClaudeDir::new("pushed");
        let mut state = IndexerState::for_test(claude.0.clone(), Some(Arc::new(OneHotWords)));
        let remote = crate::client::endpoint::ClientEndpointId::Ssh(
            crate::client::endpoint::ProfileId::parse("0123456789abcdef0123456789abcdef").unwrap(),
        );
        let pushed = ChatText {
            endpoint_id: remote.clone(),
            pane_id: "w1:p1".into(),
            names: vec!["forge".into()],
            title: "warm the shader cache".into(),
            keywords: Vec::new(),
        };
        let previous = IndexSnapshot::empty(IndexStatus::Starting);
        let snapshot = state
            .refresh_with(
                &previous,
                &[facts("w1:p1", None, "audio solver")],
                vec![pushed.clone()],
            )
            .expect("publishes both machines");
        let shader = snapshot.search("shader");
        assert_eq!(shader.len(), 1);
        assert_eq!(shader[0].endpoint_id, remote, "same pane id, other machine");
        assert!(snapshot.search("solver")[0].endpoint_id.is_local());
        assert!(
            state
                .refresh_with(
                    &snapshot,
                    &[facts("w1:p1", None, "audio solver")],
                    vec![pushed]
                )
                .is_none(),
            "unchanged pushed chats publish nothing"
        );
    }

    #[test]
    fn chats_that_close_are_forgotten() {
        let claude = TempClaudeDir::new("forget");
        claude.write_session("aaaa-1111", &["subbass solver"]);
        let mut state = IndexerState::for_test(claude.0.clone(), Some(Arc::new(OneHotWords)));
        let first = state
            .refresh(
                &IndexSnapshot::empty(IndexStatus::Starting),
                &[facts("w1:p1", Some("aaaa-1111"), "audio")],
            )
            .expect("publishes");
        let second = state
            .refresh(&first, &[])
            .expect("a closed chat is a change");
        assert!(second.texts().is_empty());
        assert!(state.summaries.is_empty());
    }
}
