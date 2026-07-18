//! Thin CLI wrapper over [`daemon::serve`]: the daemon process body (the
//! supervised generation loop, router host, core-node runner, and federation)
//! lives in the `daemon` workspace crate; this command only assembles its
//! options from clap, the CLI context, and the process environment (the data
//! root, the compile-time git hash).

use std::sync::Arc;

use super::Command;
use crate::context::AppContext;
use crate::error::Result;
use daemon_config::consts::PeppyDirs;

pub use tokio_util::sync::CancellationToken;

/// The git hash embedded at compile time by build.rs. The daemon library takes
/// it as data ([`daemon::ServeOptions::git_hash`]), so only this binary reads
/// the build-time env.
const GIT_HASH: &str = env!("PEPPY_GIT_HASH");

pub struct ServeCommand {
    pub messaging_engine: String,
    pub core_node_name: Option<String>,
    pub clock_source: super::ClockSource,
    pub shutdown_token: Option<CancellationToken>,
    /// The peppy data root the daemon runs under (config, state, singleton
    /// lock). The CLI passes [`PeppyDirs::default`]; the serve integration
    /// test passes a per-test temp root so it never reads the machine's real
    /// peppy home.
    pub peppy_dirs: PeppyDirs,
}

impl Command for ServeCommand {
    fn execute(self, ctx: &Arc<AppContext>) -> Result<()> {
        daemon::serve(daemon::ServeOptions {
            root_dir: ctx.root_dir.clone(),
            messaging_engine: self.messaging_engine,
            core_node_name: self.core_node_name,
            clock_source: self.clock_source.into(),
            git_hash: GIT_HASH.to_string(),
            peppy_dirs: self.peppy_dirs,
            shutdown_token: self.shutdown_token,
        })?;
        Ok(())
    }
}
