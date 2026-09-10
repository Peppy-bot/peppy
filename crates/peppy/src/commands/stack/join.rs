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
    places: Vec<(String, String)>,
    timeouts: StackTimeouts,
) -> Result<()> {
    let placement = copy_placement(&name, &places).map_err(Error::ExecutionFailed)?;
    let budgets = timeouts.budgets().with_env_vars(caller_env_overrides());
    let goal = StackJoinGoal {
        selections: words,
        arguments,
        placement,
        ..StackJoinGoal::new(name, option, budgets)
    };
    crate::commands::block_on(async {
        let conn = ctx.connect_to_daemon().await?;
        drive_stack_goal(&conn, &goal, &goal.budgets).await
    })
}

/// Where a copy runs: the coordinator, or the one machine
/// `--place NAME@CORE_NODE` wires its name to, `self` naming the
/// coordinator.
fn copy_placement(
    name: &Name,
    places: &[(String, String)],
) -> std::result::Result<JoinPlacement, String> {
    match places {
        [] => Ok(JoinPlacement::Local),
        [(link, target)] if link == name.as_str() => {
            if CoreNodeName::is_self_keyword(target) {
                return Ok(JoinPlacement::Local);
            }
            CoreNodeName::new(target)
                .map(JoinPlacement::CoreNode)
                .map_err(|error| format!("invalid --place target `{target}`: {error}"))
        }
        _ => Err(format!(
            "a copy has one placement link, its name; use --place {name}@CORE_NODE"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_copy_has_exactly_one_placement() {
        let name = Name::new("alpha").unwrap();
        let place = |link: &str, target: &str| (link.to_owned(), target.to_owned());
        assert_eq!(copy_placement(&name, &[]).unwrap(), JoinPlacement::Local);
        assert_eq!(
            copy_placement(&name, &[place("alpha", "self")]).unwrap(),
            JoinPlacement::Local
        );
        assert_eq!(
            copy_placement(&name, &[place("alpha", "jetson-1")]).unwrap(),
            JoinPlacement::CoreNode(CoreNodeName::new("jetson-1").unwrap())
        );
        assert!(
            copy_placement(&name, &[place("alpha", "not a core node")])
                .unwrap_err()
                .contains("invalid --place target")
        );
        for places in [
            vec![place("bravo", "jetson-1")],
            vec![place("alpha", "jetson-1"), place("bravo", "jetson-2")],
        ] {
            assert!(
                copy_placement(&name, &places)
                    .unwrap_err()
                    .contains("--place alpha@CORE_NODE")
            );
        }
    }
}
