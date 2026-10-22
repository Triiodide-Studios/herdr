//! Checks against this machine's own chats, for tuning the ranking constants and for confirming
//! an install. They read the running herdr server's chats, their Claude Code transcripts and the
//! installed model, so they are ignored by default:
//!
//! `CHAT_SEARCH_QUERIES="sunlight;subbass solver" cargo test --release real_data -- --ignored --nocapture`

use std::time::Instant;

use super::index::{IndexSnapshot, IndexStatus};
use super::indexer::{list_agents, IndexerState};

/// How similar the model finds single words: for each word of each query, the closest words among
/// every chat's title words and keywords, and how the similarity to all of them is spread. Prints
/// what the scoring thresholds were chosen from.
#[test]
#[ignore = "reads the live herdr server, Claude transcripts and the installed model"]
fn real_data_word_similarity() {
    use super::embed::{model_dir, Embedder, StaticEmbedder};
    use super::index::words;

    let agents = list_agents().expect("a running herdr server");
    let snapshot = IndexerState::new()
        .refresh(&IndexSnapshot::empty(IndexStatus::Starting), &agents)
        .expect("a first snapshot");
    let embedder = StaticEmbedder::load(&model_dir()).expect("the installed model");
    let mut terms = std::collections::BTreeSet::new();
    for text in snapshot.texts() {
        terms.extend(words(&text.names.join(" ")));
        terms.extend(words(&text.title));
        terms.extend(text.keywords);
    }
    let vectors = terms
        .iter()
        .filter_map(|term| embedder.embed(term).map(|vector| (term.clone(), vector)))
        .collect::<Vec<_>>();
    let cosine = |a: &[f32], b: &[f32]| a.iter().zip(b).map(|(x, y)| x * y).sum::<f32>();
    println!("{} distinct terms", vectors.len());
    for query in std::env::var("CHAT_SEARCH_QUERIES")
        .unwrap_or_default()
        .split(';')
        .flat_map(words)
        .collect::<std::collections::BTreeSet<_>>()
    {
        let Some(query_vector) = embedder.embed(&query) else {
            println!("{query}: unknown to the model");
            continue;
        };
        let mut scored = vectors
            .iter()
            .map(|(term, vector)| (cosine(&query_vector, vector), term.as_str()))
            .collect::<Vec<_>>();
        scored.sort_by(|a, b| b.0.total_cmp(&a.0));
        let mean = scored.iter().map(|s| s.0).sum::<f32>() / scored.len() as f32;
        let p95 = scored[scored.len() / 20].0;
        println!(
            "{query:<12} mean {mean:.2} p95 {p95:.2} | {}",
            scored
                .iter()
                .take(6)
                .map(|(score, term)| format!("{term} {score:.2}"))
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
}

#[test]
#[ignore = "reads the live herdr server, Claude transcripts and the installed model"]
fn real_data_ranks_this_machines_chats() {
    use super::embed::{model_dir, StaticEmbedder};

    let agents = list_agents().expect("a running herdr server");
    let began = Instant::now();
    StaticEmbedder::load(&model_dir()).expect("the installed model");
    println!("model load alone: {:?}", began.elapsed());

    let mut state = IndexerState::new();
    let started = Instant::now();
    let snapshot = state
        .refresh(&IndexSnapshot::empty(IndexStatus::Starting), &agents)
        .expect("a first snapshot");
    println!(
        "first refresh (model, every transcript, every embedding): {} chats in {:?}, status {:?}",
        snapshot.texts().len(),
        started.elapsed(),
        snapshot.status()
    );
    let started = Instant::now();
    let again = state.refresh(&snapshot, &agents);
    println!(
        "second refresh with nothing new: {:?}, republished: {}",
        started.elapsed(),
        again.is_some()
    );
    for text in snapshot.texts() {
        println!(
            "{:>8}  {:<24} {:<56} {}",
            text.pane_id,
            text.names.join("/"),
            text.title,
            text.keywords.join(" ")
        );
    }
    let titles = snapshot
        .texts()
        .into_iter()
        .map(|text| (text.pane_id, text.title))
        .collect::<std::collections::HashMap<_, _>>();
    for query in std::env::var("CHAT_SEARCH_QUERIES")
        .unwrap_or_default()
        .split(';')
        .map(str::trim)
        .filter(|query| !query.is_empty())
    {
        let began = Instant::now();
        let hits = snapshot.search(query);
        println!("\n{query:?} ({:?}):", began.elapsed());
        for hit in hits.iter().take(6) {
            println!(
                "  {:.3}  {:>8}  {}",
                hit.score,
                hit.pane_id,
                titles.get(&hit.pane_id).map_or("", String::as_str)
            );
        }
        if hits.is_empty() {
            println!("  (no match)");
        }
    }
    assert_eq!(
        *snapshot.status(),
        IndexStatus::Ready,
        "the model is installed"
    );
}
