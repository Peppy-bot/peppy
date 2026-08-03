//! Persistent on-disk caches shared by the batch node-add pipeline.
//!
//! `git` caches checkouts keyed by `(repo_url, commit)`, using `key`
//! (slug + short hash) to produce deterministic directory names under
//! [`daemon_config::consts::PeppyDirs`]. A commit names one tree, so a
//! populated checkout is reused without touching the network.

pub(crate) mod git;
mod key;
mod keyed_lock;
pub(super) mod materialize;

pub(crate) use materialize::{MaterializeFeedback, materialize_entry, silent_feedback};
