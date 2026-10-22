use std::collections::{HashMap, HashSet};
use std::sync::OnceLock;

/// Weighted occurrence counts of the terms in one chat.
pub(super) type TermCounts = HashMap<String, f32>;

/// A term seen less often than this (weighted) is incidental and never becomes a keyword.
const MIN_TERM_WEIGHT: f32 = 2.0;
/// Longer runs are paths or encoded blobs, not words.
const MAX_TOKEN_CHARS: usize = 24;
/// Terms one chat keeps counting; the rarest are dropped when it has more, so a long
/// conversation's one-off identifiers do not accumulate.
pub(super) const MAX_TERMS_PER_CHAT: usize = 8_000;
/// A bare word this long with fewer vowels than this share is a random identifier.
const LONG_WORD_CHARS: usize = 7;
const LONG_WORD_MIN_VOWEL_SHARE: f32 = 0.2;

/// Lowercase word tokens of `text`: runs of letters and digits, split at camelCase boundaries
/// (`BETRealtimeAudio` gives `bet`, `realtime`, `audio`). Only word-shaped tokens are kept: a bare
/// word of three letters or more, or a word of two letters or more with up to three trailing
/// digits (`ps5`, `win32`, `level37`). Hashes, ids, numbers and `a1b2` codes are dropped. Stop
/// words are kept (callers that rank terms filter them with [`is_stopword`]).
pub(super) fn tokens(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut word = Vec::<char>::new();
    for character in text.chars().chain(std::iter::once(' ')) {
        if character.is_alphanumeric() {
            word.push(character);
        } else if !word.is_empty() {
            push_word_pieces(&word, &mut out);
            word.clear();
        }
    }
    out
}

fn push_word_pieces(word: &[char], out: &mut Vec<String>) {
    let mut start = 0;
    for index in 1..word.len() {
        let previous = word[index - 1];
        let current = word[index];
        let next = word.get(index + 1).copied();
        let lower_to_upper =
            (previous.is_lowercase() || previous.is_ascii_digit()) && current.is_uppercase();
        let acronym_end = previous.is_uppercase()
            && current.is_uppercase()
            && next.is_some_and(char::is_lowercase);
        if lower_to_upper || acronym_end {
            push_piece(&word[start..index], out);
            start = index;
        }
    }
    push_piece(&word[start..], out);
}

fn push_piece(piece: &[char], out: &mut Vec<String>) {
    if piece.len() < 2 || piece.len() > MAX_TOKEN_CHARS {
        return;
    }
    let lowered = piece
        .iter()
        .flat_map(|character| character.to_lowercase())
        .collect::<String>();
    if is_word_shaped(&lowered) {
        out.push(lowered);
    }
}

fn is_word_shaped(token: &str) -> bool {
    let stem_len = token.trim_end_matches(|c: char| c.is_ascii_digit()).len();
    let (stem, digits) = token.split_at(stem_len);
    let letters = stem.chars().count();
    if digits.len() > 3 || !stem.chars().all(char::is_alphabetic) {
        return false;
    }
    if !digits.is_empty() {
        return letters >= 2;
    }
    let vowels = stem.chars().filter(|c| "aeiouy".contains(*c)).count();
    letters >= 3
        && (letters < LONG_WORD_CHARS
            || vowels as f32 >= LONG_WORD_MIN_VOWEL_SHARE * letters as f32)
}

/// Drops the least used terms when `counts` holds more than `limit`: first the incidental ones
/// (the bulk of a long conversation's vocabulary), then, if that is not enough, the rarer half.
/// Terms tied at the cutoff are kept, so pruning never empties a chat.
pub(super) fn prune_terms(counts: &mut TermCounts, limit: usize) {
    if counts.len() <= limit {
        return;
    }
    counts.retain(|_, weight| *weight >= MIN_TERM_WEIGHT);
    if counts.len() <= limit {
        return;
    }
    let mut weights = counts.values().copied().collect::<Vec<_>>();
    let keep = limit / 2;
    let (_, cutoff, _) = weights.select_nth_unstable_by(keep, |a, b| b.total_cmp(a));
    let cutoff = *cutoff;
    counts.retain(|_, weight| *weight >= cutoff);
}

pub(super) fn is_stopword(token: &str) -> bool {
    static STOPWORDS: OnceLock<HashSet<&'static str>> = OnceLock::new();
    STOPWORDS
        .get_or_init(|| STOPWORD_LIST.iter().copied().collect())
        .contains(token)
}

/// Adds the content tokens of `text` to `counts`, each occurrence worth `weight`.
pub(super) fn add_terms(counts: &mut TermCounts, text: &str, weight: f32) {
    for token in tokens(text) {
        if !is_stopword(&token) {
            *counts.entry(token).or_insert(0.0) += weight;
        }
    }
}

pub(super) fn merge_terms(into: &mut TermCounts, from: TermCounts) {
    for (term, weight) in from {
        *into.entry(term).or_insert(0.0) += weight;
    }
}

/// How many of `chats` use each term more than incidentally.
pub(super) fn document_frequency<'a>(
    chats: impl IntoIterator<Item = &'a TermCounts>,
) -> HashMap<String, usize> {
    let mut frequency = HashMap::new();
    for counts in chats {
        for (term, weight) in counts {
            if *weight >= MIN_TERM_WEIGHT {
                *frequency.entry(term.clone()).or_insert(0) += 1;
            }
        }
    }
    frequency
}

/// The `limit` terms that set this chat apart from the others: log-scaled frequency in the chat
/// times how few chats use the term. A term every chat uses scores nearly zero.
pub(super) fn top_keywords(
    counts: &TermCounts,
    frequency: &HashMap<String, usize>,
    chats: usize,
    limit: usize,
) -> Vec<String> {
    let mut scored = counts
        .iter()
        .filter(|(_, weight)| **weight >= MIN_TERM_WEIGHT)
        .filter_map(|(term, weight)| {
            let used_by = frequency.get(term).copied().unwrap_or(1);
            let rarity = ((chats as f32 + 1.0) / (used_by as f32 + 0.5)).ln();
            let score = (1.0 + weight.ln()) * rarity;
            (score > 0.0).then_some((term, score))
        })
        .collect::<Vec<_>>();
    scored.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(b.0)));
    scored
        .into_iter()
        .take(limit)
        .map(|(term, _)| term.clone())
        .collect()
}

/// English function words and chat boilerplate that say nothing about what a chat is about.
const STOPWORD_LIST: &[&str] = &[
    "am",
    "an",
    "as",
    "at",
    "be",
    "by",
    "do",
    "he",
    "if",
    "in",
    "is",
    "it",
    "me",
    "my",
    "no",
    "of",
    "on",
    "or",
    "so",
    "to",
    "up",
    "us",
    "we",
    "about",
    "above",
    "actually",
    "add",
    "added",
    "after",
    "again",
    "all",
    "also",
    "always",
    "and",
    "another",
    "any",
    "anything",
    "are",
    "around",
    "assistant",
    "back",
    "because",
    "been",
    "before",
    "being",
    "below",
    "better",
    "between",
    "both",
    "but",
    "call",
    "called",
    "can",
    "cannot",
    "case",
    "change",
    "changes",
    "check",
    "claude",
    "code",
    "come",
    "command",
    "could",
    "create",
    "created",
    "currently",
    "did",
    "does",
    "doing",
    "don",
    "done",
    "down",
    "during",
    "each",
    "edit",
    "edited",
    "else",
    "enough",
    "even",
    "every",
    "everything",
    "exactly",
    "few",
    "file",
    "files",
    "first",
    "following",
    "for",
    "found",
    "from",
    "further",
    "get",
    "gets",
    "getting",
    "give",
    "given",
    "goes",
    "going",
    "good",
    "got",
    "great",
    "had",
    "has",
    "have",
    "having",
    "her",
    "here",
    "him",
    "his",
    "how",
    "however",
    "into",
    "its",
    "itself",
    "just",
    "keep",
    "know",
    "last",
    "later",
    "least",
    "less",
    "let",
    "like",
    "line",
    "lines",
    "look",
    "looks",
    "make",
    "many",
    "maybe",
    "might",
    "more",
    "most",
    "much",
    "must",
    "need",
    "needs",
    "new",
    "next",
    "nor",
    "not",
    "now",
    "off",
    "okay",
    "once",
    "one",
    "only",
    "other",
    "our",
    "out",
    "output",
    "over",
    "own",
    "path",
    "please",
    "probably",
    "put",
    "ran",
    "read",
    "really",
    "remove",
    "removed",
    "result",
    "results",
    "right",
    "run",
    "running",
    "same",
    "say",
    "see",
    "seems",
    "set",
    "she",
    "should",
    "since",
    "some",
    "something",
    "step",
    "steps",
    "such",
    "sure",
    "take",
    "task",
    "tell",
    "than",
    "thanks",
    "that",
    "the",
    "their",
    "them",
    "then",
    "there",
    "these",
    "they",
    "thing",
    "things",
    "think",
    "this",
    "those",
    "though",
    "thought",
    "through",
    "time",
    "too",
    "tool",
    "try",
    "trying",
    "under",
    "until",
    "update",
    "updated",
    "use",
    "used",
    "user",
    "using",
    "very",
    "want",
    "wants",
    "was",
    "way",
    "well",
    "were",
    "what",
    "when",
    "where",
    "which",
    "while",
    "who",
    "why",
    "will",
    "with",
    "work",
    "works",
    "would",
    "write",
    "yeah",
    "yes",
    "yet",
    "you",
    "your",
];

#[cfg(test)]
mod tests {
    use super::*;

    fn counts(text: &str, weight: f32) -> TermCounts {
        let mut counts = TermCounts::new();
        add_terms(&mut counts, text, weight);
        counts
    }

    #[test]
    fn tokens_split_identifiers_and_keep_short_alphanumerics() {
        // "in" is a bare two-letter word and goes; "and" stays (stop words are filtered later);
        // "L37" and "v2" are a single letter with digits, a code rather than a word.
        assert_eq!(
            tokens("BETRealtimeAudio in ps5-perf_hillclimb, L37 and v2"),
            [
                "bet",
                "realtime",
                "audio",
                "ps5",
                "perf",
                "hillclimb",
                "and"
            ]
            .map(String::from)
            .to_vec()
        );
    }

    #[test]
    fn tokens_drop_numbers_hashes_and_blobs() {
        let found = tokens("20261020 0xdeadbeef a1b2c3d4e5f6 the average");
        assert_eq!(found, vec!["the".to_owned(), "average".to_owned()]);
    }

    #[test]
    fn tokens_drop_identifiers_and_codes_but_keep_acronyms_and_versions() {
        let found = tokens(
            "bj85kkihz ppsa29906 4f5b analyzer62d w04 s3020 btwtqfhnd bmc dlss hdr ps5 win32 level37 strengths",
        );
        assert_eq!(
            found,
            ["bmc", "dlss", "hdr", "ps5", "win32", "level37"].map(String::from)
        );
    }

    #[test]
    fn pruning_drops_incidental_terms_first_and_never_empties_a_chat() {
        let mut counts = TermCounts::new();
        for index in 0..100 {
            counts.insert(format!("once{index}"), 1.0);
        }
        counts.insert("topic".into(), 9.0);
        counts.insert("other".into(), 2.0);
        prune_terms(&mut counts, 10);
        assert_eq!(counts.len(), 2);

        let mut tied = (0..50)
            .map(|index| (format!("term{index}"), 2.0))
            .collect::<TermCounts>();
        prune_terms(&mut tied, 10);
        assert_eq!(tied.len(), 50, "ties at the cutoff are kept");

        let mut small = TermCounts::from([("one".to_owned(), 1.0)]);
        prune_terms(&mut small, 10);
        assert_eq!(small.len(), 1, "under the limit nothing is dropped");
    }

    #[test]
    fn stopwords_never_become_terms() {
        let found = counts("the file and the command we ran", 1.0);
        assert!(found.is_empty(), "{found:?}");
    }

    #[test]
    fn stopword_list_has_no_duplicates() {
        let unique = STOPWORD_LIST.iter().collect::<HashSet<_>>();
        assert_eq!(unique.len(), STOPWORD_LIST.len());
    }

    #[test]
    fn keywords_prefer_terms_few_chats_share() {
        let audio = counts("solver subbass solver subbass shared shared", 2.0);
        let render = counts("lumen sunlight lumen sunlight shared shared", 2.0);
        let frequency = document_frequency([&audio, &render]);
        let keywords = top_keywords(&audio, &frequency, 2, 3);
        assert_eq!(keywords.len(), 3);
        assert_eq!(keywords.last().map(String::as_str), Some("shared"));
        assert!(keywords.contains(&"solver".to_owned()));
        assert!(keywords.contains(&"subbass".to_owned()));
    }

    #[test]
    fn incidental_terms_are_not_keywords() {
        let mut once = TermCounts::new();
        add_terms(&mut once, "lonely", 1.0);
        let frequency = document_frequency([&once]);
        assert!(top_keywords(&once, &frequency, 1, 5).is_empty());
    }
}
