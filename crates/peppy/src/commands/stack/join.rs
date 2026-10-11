//! `peppy stack join`: one more copy of an option onto the running stack.

use std::sync::Arc;

use core_node_api::encoding::{ArgumentOverride, CopyReference, SelectionWord, StackJoinGoal};

use super::StackTimeouts;
use super::goal::drive_stack_goal;
use crate::commands::node::caller_env_overrides;
use crate::context::AppContext;
use crate::error::Result;

/// Joins `copy` onto the running stack: its base on the copy's machine, and
/// each part a word selects on the machine the word names.
pub(super) fn join(
    ctx: &Arc<AppContext>,
    copy: CopyReference,
    words: Vec<SelectionWord>,
    arguments: Vec<ArgumentOverride>,
    timeouts: StackTimeouts,
) -> Result<()> {
    let budgets = timeouts.budgets().with_env_vars(caller_env_overrides());
    let goal = StackJoinGoal {
        selections: words,
        arguments,
        ..StackJoinGoal::new(copy, budgets)
    };
    crate::commands::block_on(async {
        let conn = ctx.connect_to_daemon().await?;
        drive_stack_goal(&conn, &goal, &goal.budgets).await
    })
}
