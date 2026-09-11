//! `peppy stack join`: one more copy of an option onto the running stack.

use std::sync::Arc;

use config::runtime::{CoreNodeName, Name};
use core_node_api::encoding::{ArgumentOverride, JoinPlacement, StackJoinGoal};

use super::StackTimeouts;
use super::goal::drive_stack_goal;
use crate::commands::node::caller_env_overrides;
use crate::context::AppContext;
use crate::error::{Error, Result};

pub(super) fn join(
    ctx: &Arc<AppContext>,
    option: String,
    name: Name,
    words: Vec<String>,
    arguments: Vec<ArgumentOverride>,
    place: Option<JoinPlacement>,
    timeouts: StackTimeouts,
) -> Result<()> {
    check_join_words(&words).map_err(Error::ExecutionFailed)?;
    let budgets = timeouts.budgets().with_env_vars(caller_env_overrides());
    let goal = StackJoinGoal {
        selections: words,
        arguments,
        placement: place.unwrap_or(JoinPlacement::Local),
        ..StackJoinGoal::new(name, option, budgets)
    };
    crate::commands::block_on(async {
        let conn = ctx.connect_to_daemon().await?;
        drive_stack_goal(&conn, &goal, &goal.budgets).await
    })
}

/// A join's `--with` words name options of the copied option's own axes;
/// a word scoped to a copy, `NAME.option`, belongs to `stack launch`.
fn check_join_words(words: &[String]) -> std::result::Result<(), String> {
    match words.iter().find(|word| word.contains('.')) {
        Some(word) => Err(format!(
            "`--with {word}`: a join's words name options of the copied option's own axes, \
             as `option` or `axis=option`; `NAME.option` selects a file copy's axis on \
             `peppy stack launch`"
        )),
        None => Ok(()),
    }
}

/// `--place CORE_NODE`: the machine the whole copy runs on, `self` naming
/// the coordinator.
pub(super) fn parse_placement(raw: &str) -> std::result::Result<JoinPlacement, String> {
    if let Some((_, machine)) = raw.split_once('@') {
        return Err(format!(
            "a join places the whole copy on one machine: `--place {machine}`"
        ));
    }
    if CoreNodeName::is_self_keyword(raw) {
        return Ok(JoinPlacement::Local);
    }
    CoreNodeName::new(raw)
        .map(JoinPlacement::CoreNode)
        .map_err(|error| format!("invalid --place target `{raw}`: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_joins_words_are_never_scoped_to_a_copy() {
        assert!(check_join_words(&["xr".into(), "recorder=lerobot".into()]).is_ok());
        let refusal = check_join_words(&["alpha.xr".into()]).unwrap_err();
        assert!(refusal.contains("`peppy stack launch`"), "{refusal}");
    }

    #[test]
    fn a_placement_names_a_core_node_or_the_coordinator() {
        assert_eq!(parse_placement("self").unwrap(), JoinPlacement::Local);
        assert_eq!(
            parse_placement("jetson-1").unwrap(),
            JoinPlacement::CoreNode(CoreNodeName::new("jetson-1").unwrap())
        );
        assert!(
            parse_placement("not a core node")
                .unwrap_err()
                .contains("invalid --place target")
        );
        assert!(
            parse_placement("alpha@jetson-1")
                .unwrap_err()
                .contains("`--place jetson-1`")
        );
    }
}
