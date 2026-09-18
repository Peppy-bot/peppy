//! Test-support fixtures shared across the Peppy workspaces, extracted from
//! `config`'s former `test_helpers` feature.
//!
//! The pure-`std` helpers ([`assert_contains_all`], [`test_tmp_root`]) are
//! always available. The git-repo and node-config-template fixtures need
//! libgit2 + askama and so live behind the `git_fixtures` feature, so consumers
//! that only need a scratch dir (e.g. `containers`, `generator`) don't link
//! libgit2.

#![forbid(unsafe_code)]

mod assertions;
mod scratch;

pub use assertions::*;
pub use scratch::*;

#[cfg(feature = "git_fixtures")]
mod fixtures;
#[cfg(feature = "git_fixtures")]
pub use fixtures::*;
