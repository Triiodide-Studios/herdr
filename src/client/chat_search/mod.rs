//! Semantic search over the local chats shown in the sidebar.
//!
//! Every chat is indexed by two short texts, never by its conversation: its names and title, and
//! the handful of keywords that set its conversation apart from the other chats'. Both are turned
//! into vectors by a small static embedding model (see [`embed`]); a query is matched against them
//! by meaning and by the letters typed. The keywords are counted from the chat's Claude Code
//! transcript (see [`transcript`]) without its newest messages, which change on every turn and say
//! what the chat is doing now rather than what it is about.
//!
//! A background thread ([`indexer`]) keeps the index current; the window only reads immutable
//! snapshots of it ([`index`]).

mod embed;
mod index;
mod indexer;
mod keywords;
mod transcript;

pub(crate) use index::{ChatHit, ChatSearchIndex, ChatText, IndexSnapshot, IndexStatus};

#[cfg(test)]
pub(in crate::client) use index::test_support;
#[cfg(test)]
mod real_data_tests;
