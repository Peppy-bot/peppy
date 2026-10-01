//! `peppy platform configure`: select the workspace and the project the
//! platform commands act on, and store them as the context. The flags name
//! them; what no flag names is the only candidate, or the person selects it
//! from a numbered list. `peppy platform login` runs the same selection after
//! a sign-in.

use std::sync::Arc;

use daemon_config::consts::PeppyDirs;

use crate::commands::Command;
use crate::commands::platform::PlatformSession;
use crate::commands::platform::context::enrollment_note;
use crate::commands::platform::select::{self, Ask};
use crate::context::AppContext;
use crate::error::Result;
use auth::{PlatformApi, PlatformContext};

pub struct ConfigureCommand {
    pub api_url: Option<String>,
    pub workspace: Option<String>,
    pub project: Option<String>,
    /// How the selection asks the person: on the terminal, from a supplied
    /// reader, or not at all.
    pub ask: Ask,
    /// Test seam: override the peppy data dirs.
    pub peppy_dirs: Option<PeppyDirs>,
}

impl Command for ConfigureCommand {
    fn execute(mut self, _ctx: &Arc<AppContext>) -> Result<()> {
        let session = PlatformSession::resolve(self.peppy_dirs, self.api_url.as_deref())?;
        let mut api = session.api()?;
        let context = select_and_save(
            &session,
            &mut api,
            self.workspace.as_deref(),
            self.project.as_deref(),
            &mut self.ask,
        )?;
        print!("{}", selected_report(&session, &context));
        Ok(())
    }
}

/// Runs the selection and stores the context it makes. The one path that
/// `login`, `configure` and `context use` share.
pub(crate) fn select_and_save(
    session: &PlatformSession,
    api: &mut PlatformApi,
    workspace_flag: Option<&str>,
    project_flag: Option<&str>,
    ask: &mut Ask,
) -> Result<PlatformContext> {
    let context = select::select_context(session, api, workspace_flag, project_flag, ask)?;
    auth::context::save(&session.dirs, &context)?;
    Ok(context)
}

/// What the person reads after a selection: the context, and a note when this
/// machine is enrolled in a different project.
pub(crate) fn selected_report(session: &PlatformSession, context: &PlatformContext) -> String {
    let mut out = format!("Context: {}\n", context.label());
    let enrollment = auth::enrollment::load(&session.dirs).ok().flatten();
    if let Some(note) = enrollment_note(context, enrollment.as_ref().map(|e| &e.document)) {
        out.push_str(&note);
        out.push('\n');
    }
    out
}
