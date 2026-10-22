use std::collections::HashSet;
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::time::Duration;

use super::embed::Embedder;
use super::keywords::is_stopword;

/// Most chats a search returns.
const MAX_RESULTS: usize = 24;
/// A chat scoring below this does not match.
const MIN_SCORE: f32 = 0.25;
/// A chat scoring below this share of the best chat's score is a weaker side match and is dropped.
const MIN_SHARE_OF_BEST: f32 = 0.4;
/// Word vectors less alike than this are noise. Measured on the model against this machine's own
/// chats: unrelated words score 0.0 to 0.1 (95th percentile 0.10), related ones 0.4 and up
/// (audio~speaker 0.44, gpu~pci 0.43, profiling~profilers 0.57, crash~crashes 0.66).
const SIMILARITY_FLOOR: f32 = 0.2;
/// A query word that begins a longer word ("sunl" for "sunlight") scores this; a whole word, 1.0.
const PREFIX_SCORE: f32 = 0.85;
/// Shorter query words only match whole words, or "gi" would match "gifts".
const PREFIX_MIN_CHARS: usize = 3;
/// Keywords are inferred from the conversation, so they count for a little less than the words of
/// the name and title.
const KEYWORD_WEIGHT: f32 = 0.9;

/// What the search knows about one chat.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ChatText {
    pub(crate) pane_id: String,
    /// Names the chat goes by: the integration's name and the plugin's.
    pub(crate) names: Vec<String>,
    pub(crate) title: String,
    pub(crate) keywords: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum IndexStatus {
    Starting,
    Ready,
    ModelUnavailable(String),
}

/// One word a chat can be found by, with its vector.
struct IndexedTerm {
    word: String,
    weight: f32,
    vector: Option<Vec<f32>>,
}

/// One word of a query, with its vector.
struct QueryWord {
    word: String,
    vector: Option<Vec<f32>>,
}

impl IndexedTerm {
    /// How well this term answers `query`: 1.0 for the same word, [`PREFIX_SCORE`] for a word it
    /// begins, otherwise how much closer in meaning than noise (0.0 to 1.0); times the term's weight.
    fn match_score(&self, query: &QueryWord) -> f32 {
        let letters = if self.word == query.word {
            1.0
        } else if query.word.chars().count() >= PREFIX_MIN_CHARS
            && self.word.starts_with(&query.word)
        {
            PREFIX_SCORE
        } else {
            0.0
        };
        let meaning = match (&query.vector, &self.vector) {
            (Some(query), Some(term)) => {
                ((cosine(query, term) - SIMILARITY_FLOOR) / (1.0 - SIMILARITY_FLOOR)).max(0.0)
            }
            _ => 0.0,
        };
        letters.max(meaning) * self.weight
    }
}

struct IndexedChat {
    text: ChatText,
    terms: Vec<IndexedTerm>,
}

impl IndexedChat {
    /// The words of the names and title (weight 1.0) and the keywords, without stop words.
    fn new(text: ChatText, embedder: Option<&dyn Embedder>) -> Self {
        let named = words(&text.names.join(" "))
            .into_iter()
            .chain(words(&text.title))
            .map(|word| (word, 1.0));
        let keywords = text
            .keywords
            .iter()
            .map(|keyword| (keyword.clone(), KEYWORD_WEIGHT));
        let mut seen = HashSet::new();
        let terms = named
            .chain(keywords)
            .filter(|(word, _)| word.chars().count() >= 2 && !is_stopword(word))
            .filter(|(word, _)| seen.insert(word.clone()))
            .map(|(word, weight)| IndexedTerm {
                vector: embedder.and_then(|embedder| embedder.embed(&word)),
                word,
                weight,
            })
            .collect();
        Self { text, terms }
    }

    /// The average over the query's words of the best answer any term gives.
    fn score(&self, query: &[QueryWord]) -> f32 {
        let total: f32 = query
            .iter()
            .map(|word| {
                self.terms
                    .iter()
                    .map(|term| term.match_score(word))
                    .fold(0.0, f32::max)
            })
            .sum();
        total / query.len() as f32
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ChatHit {
    pub(crate) pane_id: String,
    pub(crate) score: f32,
}

/// An immutable view of the index: the chats as of one refresh, with their word vectors.
pub(crate) struct IndexSnapshot {
    generation: u64,
    status: IndexStatus,
    chats: Vec<IndexedChat>,
    embedder: Option<Arc<dyn Embedder>>,
}

impl IndexSnapshot {
    pub(crate) fn empty(status: IndexStatus) -> Self {
        Self {
            generation: 0,
            status,
            chats: Vec::new(),
            embedder: None,
        }
    }

    /// Indexes `texts`, keeping the vectors of chats that did not change since `previous`.
    pub(super) fn build(
        previous: &IndexSnapshot,
        status: IndexStatus,
        texts: Vec<ChatText>,
        embedder: Option<Arc<dyn Embedder>>,
    ) -> Self {
        let same_embedder = match (&previous.embedder, &embedder) {
            (Some(old), Some(new)) => Arc::ptr_eq(old, new),
            (None, None) => true,
            _ => false,
        };
        let mut reusable = previous
            .chats
            .iter()
            .filter(|_| same_embedder)
            .map(|chat| (chat.text.clone(), chat))
            .collect::<Vec<_>>();
        let chats = texts
            .into_iter()
            .map(
                |text| match reusable.iter().position(|(old, _)| *old == text) {
                    Some(index) => {
                        let (_, chat) = reusable.swap_remove(index);
                        IndexedChat {
                            text,
                            terms: chat
                                .terms
                                .iter()
                                .map(|term| IndexedTerm {
                                    word: term.word.clone(),
                                    weight: term.weight,
                                    vector: term.vector.clone(),
                                })
                                .collect(),
                        }
                    }
                    None => IndexedChat::new(text, embedder.as_deref()),
                },
            )
            .collect();
        Self {
            generation: previous.generation + 1,
            status,
            chats,
            embedder,
        }
    }

    pub(crate) fn generation(&self) -> u64 {
        self.generation
    }

    pub(crate) fn status(&self) -> &IndexStatus {
        &self.status
    }

    pub(super) fn texts(&self) -> Vec<ChatText> {
        self.chats.iter().map(|chat| chat.text.clone()).collect()
    }

    pub(super) fn has_embedder(&self) -> bool {
        self.embedder.is_some()
    }

    /// The chats that match `query`, best first. Each query word is answered by the chat's word
    /// closest to it, by letters (the same word, or one it begins) or by meaning (a word whose
    /// vector is close), and a chat scores the average over the query's words.
    pub(crate) fn search(&self, query: &str) -> Vec<ChatHit> {
        let mut query_words = words(query)
            .into_iter()
            .filter(|word| word.chars().count() >= 2)
            .collect::<Vec<_>>();
        let content_words = query_words
            .iter()
            .filter(|word| !is_stopword(word))
            .cloned()
            .collect::<Vec<_>>();
        if !content_words.is_empty() {
            query_words = content_words;
        }
        if query_words.is_empty() {
            return Vec::new();
        }
        let query = query_words
            .into_iter()
            .map(|word| QueryWord {
                vector: self
                    .embedder
                    .as_ref()
                    .and_then(|embedder| embedder.embed(&word)),
                word,
            })
            .collect::<Vec<_>>();
        let mut hits = self
            .chats
            .iter()
            .filter_map(|chat| {
                let score = chat.score(&query);
                (score >= MIN_SCORE).then(|| ChatHit {
                    pane_id: chat.text.pane_id.clone(),
                    score,
                })
            })
            .collect::<Vec<_>>();
        hits.sort_by(|a, b| {
            b.score
                .total_cmp(&a.score)
                .then_with(|| a.pane_id.cmp(&b.pane_id))
        });
        if let Some(best) = hits.first().map(|hit| hit.score) {
            hits.retain(|hit| hit.score >= MIN_SHARE_OF_BEST * best);
        }
        hits.truncate(MAX_RESULTS);
        hits
    }
}

/// Lowercase alphanumeric words of `text`.
pub(super) fn words(text: &str) -> Vec<String> {
    text.split(|character: char| !character.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .map(str::to_lowercase)
        .collect()
}

fn cosine(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() {
        return 0.0;
    }
    let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    let norm_a = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let norm_b = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm_a == 0.0 || norm_b == 0.0 {
        0.0
    } else {
        dot / (norm_a * norm_b)
    }
}

/// The index shared between the window, which searches it, and the background thread that keeps
/// it current.
#[derive(Clone)]
pub(crate) struct ChatSearchIndex {
    shared: Arc<Shared>,
}

struct Shared {
    snapshot: Mutex<Arc<IndexSnapshot>>,
    refresh_requested: Mutex<bool>,
    wake: Condvar,
}

impl ChatSearchIndex {
    /// An index nothing keeps current: always empty.
    pub(crate) fn inert() -> Self {
        Self::with_snapshot(IndexSnapshot::empty(IndexStatus::Starting))
    }

    pub(crate) fn with_snapshot(snapshot: IndexSnapshot) -> Self {
        Self {
            shared: Arc::new(Shared {
                snapshot: Mutex::new(Arc::new(snapshot)),
                refresh_requested: Mutex::new(false),
                wake: Condvar::new(),
            }),
        }
    }

    pub(crate) fn snapshot(&self) -> Arc<IndexSnapshot> {
        self.shared
            .snapshot
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    pub(super) fn publish(&self, snapshot: IndexSnapshot) {
        *self
            .shared
            .snapshot
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Arc::new(snapshot);
    }

    /// Asks the background thread to refresh now instead of at its next interval.
    pub(crate) fn request_refresh(&self) {
        *self
            .shared
            .refresh_requested
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = true;
        self.shared.wake.notify_one();
    }

    /// Sleeps until a refresh is requested or `interval` passes.
    pub(super) fn wait_for_refresh(&self, interval: Duration) {
        let requested = self
            .shared
            .refresh_requested
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let (mut requested, _) = self
            .shared
            .wake
            .wait_timeout_while(requested, interval, |requested| !*requested)
            .unwrap_or_else(PoisonError::into_inner);
        *requested = false;
    }
}

#[cfg(test)]
pub(in crate::client) mod test_support {
    use super::*;

    const DIMENSIONS: usize = 1024;

    /// A bag-of-words embedder: every distinct word gets a dimension of its own, so a word is
    /// identical to itself and exactly orthogonal to every other word.
    pub(in crate::client) struct OneHotWords;

    fn dimension_of(word: &str) -> usize {
        static DIMENSIONS_BY_WORD: Mutex<Vec<String>> = Mutex::new(Vec::new());
        let mut known = DIMENSIONS_BY_WORD
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let slot = known
            .iter()
            .position(|known| known == word)
            .unwrap_or_else(|| {
                known.push(word.to_owned());
                known.len() - 1
            });
        assert!(
            slot < DIMENSIONS,
            "the test vocabulary outgrew the toy embedder"
        );
        slot
    }

    impl Embedder for OneHotWords {
        fn embed(&self, text: &str) -> Option<Vec<f32>> {
            let mut vector = vec![0.0_f32; DIMENSIONS];
            let mut any = false;
            for word in words(text) {
                vector[dimension_of(&word)] += 1.0;
                any = true;
            }
            any.then_some(vector)
        }
    }

    pub(in crate::client) fn chat(
        pane_id: &str,
        names: &[&str],
        title: &str,
        keywords: &[&str],
    ) -> ChatText {
        ChatText {
            pane_id: pane_id.to_owned(),
            names: names.iter().map(|name| (*name).to_owned()).collect(),
            title: title.to_owned(),
            keywords: keywords.iter().map(|word| (*word).to_owned()).collect(),
        }
    }

    pub(in crate::client) fn snapshot(texts: Vec<ChatText>) -> IndexSnapshot {
        IndexSnapshot::build(
            &IndexSnapshot::empty(IndexStatus::Starting),
            IndexStatus::Ready,
            texts,
            Some(Arc::new(OneHotWords)),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::{chat, snapshot, OneHotWords};
    use super::*;
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn audio_and_render() -> IndexSnapshot {
        snapshot(vec![
            chat(
                "w1:p1",
                &["solver-realism"],
                "Diagnose and Fix Sub-Bass Realism",
                &["solver", "subbass", "wave"],
            ),
            chat(
                "w1:p2",
                &["lumen-gi"],
                "Find why sunlight is blocked in Low GI",
                &["lumen", "sunlight", "probe"],
            ),
        ])
    }

    fn ranked(snapshot: &IndexSnapshot, query: &str) -> Vec<String> {
        snapshot
            .search(query)
            .into_iter()
            .map(|hit| hit.pane_id)
            .collect()
    }

    #[test]
    fn a_title_word_finds_its_chat() {
        assert_eq!(ranked(&audio_and_render(), "sunlight"), ["w1:p2"]);
    }

    #[test]
    fn a_keyword_that_is_not_in_the_title_finds_its_chat() {
        assert_eq!(ranked(&audio_and_render(), "wave"), ["w1:p1"]);
    }

    #[test]
    fn a_half_typed_word_matches_by_prefix_but_a_short_one_only_whole() {
        assert_eq!(ranked(&audio_and_render(), "sunl"), ["w1:p2"]);
        assert_eq!(ranked(&audio_and_render(), "SUB"), ["w1:p1"]);
        assert!(ranked(&audio_and_render(), "su").is_empty());
        assert_eq!(ranked(&audio_and_render(), "gi"), ["w1:p2"]);
    }

    #[test]
    fn a_name_finds_its_chat() {
        assert_eq!(ranked(&audio_and_render(), "lumen gi"), ["w1:p2"]);
    }

    #[test]
    fn stop_words_in_the_query_do_not_count_against_a_chat() {
        assert_eq!(
            ranked(&audio_and_render(), "why the sunlight is"),
            ["w1:p2"]
        );
        assert!(ranked(&audio_and_render(), "the of and").is_empty());
    }

    #[test]
    fn unrelated_queries_find_nothing() {
        assert!(ranked(&audio_and_render(), "kubernetes").is_empty());
        assert!(ranked(&audio_and_render(), "   ").is_empty());
    }

    #[test]
    fn better_matches_rank_first_and_weak_side_matches_are_dropped() {
        let index = snapshot(vec![
            chat("w1:p1", &["a1"], "shader compile stalls", &["shader"]),
            chat(
                "w1:p2",
                &["b2"],
                "shader cache",
                &["compile", "stalls", "shader"],
            ),
            chat("w1:p3", &["c3"], "shader notes", &[]),
        ]);
        assert_eq!(ranked(&index, "shader compile stalls"), ["w1:p1", "w1:p2"]);
        let hits = index.search("shader compile stalls");
        assert!(hits[0].score > hits[1].score);
    }

    /// Words with fixed vectors: "audio" and "speaker" are 0.6 alike, "kitchen" is unrelated.
    struct Meanings(HashMap<&'static str, Vec<f32>>);

    impl Embedder for Meanings {
        fn embed(&self, text: &str) -> Option<Vec<f32>> {
            self.0.get(text).cloned()
        }
    }

    #[test]
    fn a_word_close_in_meaning_finds_a_chat_that_never_says_it() {
        let embedder = Arc::new(Meanings(HashMap::from([
            ("audio", vec![1.0, 0.0, 0.0]),
            ("speaker", vec![0.6, 0.8, 0.0]),
            ("kitchen", vec![0.0, 0.0, 1.0]),
        ])));
        let index = IndexSnapshot::build(
            &IndexSnapshot::empty(IndexStatus::Starting),
            IndexStatus::Ready,
            vec![
                chat("w1:p1", &["x1"], "playback", &["speaker"]),
                chat("w1:p2", &["y2"], "cooking", &["kitchen"]),
            ],
            Some(embedder),
        );
        let hits = index.search("audio");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].pane_id, "w1:p1");
        // (0.6 - 0.2) / 0.8 = 0.5 of the way from noise to identical, times the keyword weight.
        assert!(
            (hits[0].score - 0.5 * KEYWORD_WEIGHT).abs() < 1e-5,
            "{}",
            hits[0].score
        );
    }

    struct Counting(AtomicUsize);

    impl Embedder for Counting {
        fn embed(&self, text: &str) -> Option<Vec<f32>> {
            self.0.fetch_add(1, Ordering::SeqCst);
            OneHotWords.embed(text)
        }
    }

    #[test]
    fn unchanged_chats_keep_their_vectors_and_changed_ones_are_embedded_again() {
        let embedder = Arc::new(Counting(AtomicUsize::new(0)));
        let calls = || embedder.0.load(Ordering::SeqCst);
        let build = |previous: &IndexSnapshot, texts: Vec<ChatText>| {
            IndexSnapshot::build(previous, IndexStatus::Ready, texts, Some(embedder.clone()))
        };
        let one = chat("w1:p1", &["alpha1"], "one title", &["keyword"]);
        let two = chat("w1:p2", &["beta2"], "other words", &["another"]);

        let first = build(
            &IndexSnapshot::empty(IndexStatus::Starting),
            vec![one.clone()],
        );
        let embedded_for_one = calls();
        assert!(embedded_for_one > 0);

        let second = build(&first, vec![one.clone(), two]);
        let embedded_for_two = calls() - embedded_for_one;
        assert!(embedded_for_two > 0, "the new chat is embedded");
        assert_eq!(second.generation(), first.generation() + 1);

        let before = calls();
        let third = build(&second, second.texts());
        assert_eq!(calls(), before, "nothing changed, nothing is embedded");
        assert_eq!(third.texts().len(), 2);

        let renamed = chat("w1:p1", &["alpha1"], "renamed title", &["keyword"]);
        let before = calls();
        build(&third, vec![renamed]);
        assert!(calls() > before, "a retitled chat is embedded again");
    }

    #[test]
    fn without_a_model_letters_still_find_chats() {
        let index = IndexSnapshot::build(
            &IndexSnapshot::empty(IndexStatus::Starting),
            IndexStatus::ModelUnavailable("missing".into()),
            audio_and_render().texts(),
            None,
        );
        assert_eq!(ranked(&index, "sunlight"), ["w1:p2"]);
        assert!(!index.has_embedder());
    }

    #[test]
    fn a_refresh_request_wakes_the_waiting_thread() {
        let index = ChatSearchIndex::inert();
        let waiter = index.clone();
        let started = std::time::Instant::now();
        let handle = std::thread::spawn(move || waiter.wait_for_refresh(Duration::from_secs(30)));
        std::thread::sleep(Duration::from_millis(50));
        index.request_refresh();
        handle.join().expect("waiter");
        assert!(started.elapsed() < Duration::from_secs(10));
    }
}
