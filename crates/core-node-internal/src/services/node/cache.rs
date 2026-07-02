//! Persistent on-disk caches shared by the batch node-add pipeline.
//!
//! `git` caches fully-cloned checkouts keyed by `(repo_url, ref)`;
//! `bundle` caches extracted HTTP archives keyed by `(url, sha256)`.
//! Both use `key` (slug + short hash) to produce deterministic directory
//! names under [`daemon_config::consts::PeppyDirs`].

pub(super) mod bundle;
pub(crate) mod git;
mod key;
pub(super) mod materialize;

pub(super) use bundle::ensure_bundle;
pub(super) use git::ensure_checkout;
pub(super) use materialize::{MaterializeFeedback, materialize_entry, silent_feedback};
