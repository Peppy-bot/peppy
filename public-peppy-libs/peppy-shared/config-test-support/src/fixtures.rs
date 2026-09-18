//! Git-repo and node-config-template fixtures, gated behind `git_fixtures` so
//! the heavy libgit2 + askama scaffolding stays out of consumers that only need
//! the pure-`std` helpers.

mod git;
mod templates;

pub use git::*;
pub use templates::*;
