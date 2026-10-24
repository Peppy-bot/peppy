//! The bridge of `stack_copies:v1`: `list`, `join` and `remove` of the
//! copies of the running launch, within the scope of a target.
//!
//! - `list` answers the scope's options with their models, their
//!   descriptions and their features, the machines of the federation now (this machine and every
//!   machine presence lists, read on each call), this machine, the copies
//!   whose option is in the scope, in name order, each with its machine and
//!   the features its selections hold with the machine of each, `max_copies`,
//!   and the `change` that runs now when it adds or removes a copy of a
//!   scoped option. Each copy has the shape of a `join` goal.
//! - `join` parses the goal into a typed join first: the name as a copy
//!   name, the option as a scoped option, each machine as a core node name,
//!   and each feature as a feature of the option, named once. Then it takes
//!   the bridge's lock, reads the copies of the stack, refuses a name a copy
//!   has and a stack that holds `max_copies` copies of the scope's options,
//!   and sends the join of the option under the name on its machine, with
//!   the word of each feature on its machine, no argument and no
//!   environment, under the default budgets. A machine equal to this one
//!   travels as the coordinator, and a feature on the copy's machine names
//!   no machine. The daemon refuses a machine that is not live.
//! - `remove` refuses a name that is not a copy of a scoped option, then
//!   sends the removal.
//!
//! The bridge runs one `join` at a time. A `join` takes the lock before it
//! reads the stack and holds it until the daemon's work ends, also after its
//! call ended; a `join` that finds it held is refused with the daemon's busy
//! text. A `remove` takes no lock: the daemon refuses it while another stack
//! change runs. How a call follows the change it sent is in [`change`].
//!
//! The daemon reports a join in its list from the moment it admits the
//! goal. A `join` also holds the admission lock of the bridge until the
//! daemon admitted or refused its goal, and `list` waits for that lock
//! before it reads the stack, so `list` reports each addition of the bridge
//! under `change` from the moment the bridge takes it until it ends.

mod change;

use super::OwnDaemon;
use change::{Change, JoinLocks, start_change, wait_for_change};
use config::failure_kind::PeppyFailureKind;
use config::runtime::{CopyName, CoreNodeName, CoreNodeTarget};
use core_node_api::encoding::{
    CopyChange, CopyInfo, CopyReference, SelectionWord, StackJoinGoal, StackRemoveGoal,
};
use daemon_config::daemon_interface::{ScopedOption, StackCopiesScope};
use peppy_mcp_runtime::{ActionExit, ProviderFailure, ToolCallError};
use peppylib::CoreNodePresenceMessenger;
use serde::Deserialize;
use serde_json::{Value, json};
use stack_goal::DEFAULT_BUDGETS;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;

use crate::bridges::TaskSurface;

/// How long a `join` or a `remove` waits for the daemon's list of the stack
/// before it refuses. The daemon answers at once.
const STACK_READ_TIMEOUT: Duration = Duration::from_secs(10);

/// The reason the bridge gives when it refuses a `join` while one it started
/// is still in progress.
pub(crate) const STACK_BUSY_REASON: &str =
    "a `join` started through this server is still in progress; wait for it to finish";

/// The bridge of `stack_copies:v1` to the daemon that started the server.
pub(crate) struct StackCopies {
    daemon: OwnDaemon,
    /// Held by a `join` from before it reads the stack until the daemon's
    /// work on it ends, so the bridge runs one `join` at a time.
    joins: Arc<Mutex<()>>,
    /// Held by a `join` from before it reads the stack until the daemon
    /// admitted or refused its goal. `list` waits for it.
    admission: Arc<Mutex<()>>,
}

impl StackCopies {
    pub(crate) fn new(daemon: OwnDaemon) -> Self {
        Self {
            daemon,
            joins: Arc::new(Mutex::new(())),
            admission: Arc::new(Mutex::new(())),
        }
    }

    /// The bridge under `scope`, the scope of one daemon target.
    pub(crate) fn scoped(self: &Arc<Self>, scope: StackCopiesScope) -> ScopedStackCopies {
        ScopedStackCopies {
            bridge: Arc::clone(self),
            scope: Arc::new(scope),
        }
    }
}

/// The bridge of one target of `stack_copies`, under the target's scope:
/// the provider of the target's tools.
#[derive(Clone)]
pub(crate) struct ScopedStackCopies {
    bridge: Arc<StackCopies>,
    scope: Arc<StackCopiesScope>,
}

/// The goal of `join`, as the narrowed schema admits it.
#[derive(Deserialize)]
struct JoinInput {
    name: String,
    option: String,
    core_node: String,
    features: Vec<FeatureInput>,
}

/// One feature of a `join` goal: the feature and the machine of its part.
#[derive(Deserialize)]
struct FeatureInput {
    feature: String,
    core_node: String,
}

/// A `join` goal parsed: the copy and the word of each of its features.
#[derive(Debug, Clone, PartialEq, Eq)]
struct TypedJoin {
    copy: CopyReference,
    words: Vec<SelectionWord>,
}

/// The goal of `remove`, as the narrowed schema admits it.
#[derive(Deserialize)]
struct RemoveInput {
    name: String,
}

impl ScopedStackCopies {
    /// Resolves once no `join` of the bridge holds the lock.
    #[cfg(test)]
    pub(crate) async fn idle(&self) {
        drop(self.bridge.joins.lock().await);
    }

    /// Whether no `join` of the bridge holds the lock at this moment.
    #[cfg(test)]
    pub(crate) fn is_idle(&self) -> bool {
        self.bridge.joins.try_lock().is_ok()
    }

    /// `list`: the scope's options, the copies of them on the stack in name
    /// order, `max_copies`, and the change of a copy of them that runs now.
    /// It first waits until no `join` of the bridge waits for the daemon's
    /// admission. The daemon has `deadline`, the tool's own, to answer.
    pub(crate) async fn list(&self, deadline: Duration) -> Result<Value, ToolCallError> {
        drop(self.bridge.admission.lock().await);
        let daemon = &self.bridge.daemon;
        let (listed, machines) = tokio::join!(
            stack_goal::list_copies(daemon.route(), deadline),
            CoreNodePresenceMessenger::live_core_node_names(
                &daemon.messenger,
                &daemon.core_node,
                deadline,
            ),
        );
        let listed = listed.map_err(|error| {
            ToolCallError::Error(format!("the daemon did not list the stack: {error}"))
        })?;
        let machines = live_machines_or_refusal(machines)?;
        let options: Vec<Value> = self
            .scope
            .options()
            .iter()
            .map(|entry| {
                let features: Vec<Value> = entry
                    .features()
                    .iter()
                    .map(|feature| {
                        json!({
                            "feature": feature.feature().as_str(),
                            "description": feature.description().as_str(),
                        })
                    })
                    .collect();
                json!({
                    "option": entry.option().as_str(),
                    "model": entry.model().as_str(),
                    "description": entry.description().as_str(),
                    "features": features,
                })
            })
            .collect();
        let copies: Vec<Value> = scoped_copies(&self.scope, &listed.copies)
            .into_iter()
            .map(|copy| listed_copy(&self.scope, copy))
            .collect();
        let mut answer = json!({
            "options": options,
            "core_nodes": machines,
            "this_core_node": daemon.core_node,
            "copies": copies,
            "max_copies": self.scope.max_copies().get(),
        });
        if let Some(change) = listed
            .copy_change
            .filter(|change| in_scope(&self.scope, &change.option))
        {
            answer["change"] = change_answer(&change);
        }
        Ok(answer)
    }

    /// `join`: adds a copy of a scoped option, and follows the addition for
    /// the call through `surface`, at most `window` of silence at a time
    /// (see [`wait_for_change`]).
    pub(crate) async fn join(
        &self,
        input: &Value,
        surface: &impl TaskSurface,
        window: Option<Duration>,
    ) -> Result<Value, ActionExit> {
        let input: JoinInput = parse_input(input)?;
        let TypedJoin { copy, words } = self
            .typed_join(&input)
            .map_err(|reason| refusal(PeppyFailureKind::Rejected, reason))?;
        let join = Arc::clone(&self.bridge.joins)
            .try_lock_owned()
            .map_err(|_| refusal(PeppyFailureKind::PreconditionFailed, STACK_BUSY_REASON))?;
        let admission = Arc::clone(&self.bridge.admission).lock_owned().await;
        let change = Change::Addition {
            name: copy.name.clone(),
            option: copy.option.clone(),
        };
        let daemon = self.bridge.daemon.clone();
        let scope = Arc::clone(&self.scope);
        let goal = async move {
            let copies = read_copies(&daemon).await.map_err(ActionExit::Error)?;
            refuse_join(&scope, &copies, &copy.name)
                .map_err(|reason| refusal(PeppyFailureKind::PreconditionFailed, reason))?;
            Ok(StackJoinGoal {
                selections: words,
                ..StackJoinGoal::new(copy, DEFAULT_BUDGETS)
            })
        };
        let locks = JoinLocks { join, admission };
        let started = start_change(
            self.bridge.daemon.clone(),
            Some(locks),
            change.clone(),
            goal,
        );
        wait_for_change(started, &change, surface, window).await
    }

    /// `remove`: removes a copy of a scoped option, and follows the removal
    /// for the call as `join` does.
    pub(crate) async fn remove(
        &self,
        input: &Value,
        surface: &impl TaskSurface,
        window: Option<Duration>,
    ) -> Result<Value, ActionExit> {
        let input: RemoveInput = parse_input(input)?;
        let name = CopyName::new(input.name.as_str())
            .map_err(|error| refusal(PeppyFailureKind::Rejected, error.to_string()))?;
        let change = Change::Removal { name: name.clone() };
        let daemon = self.bridge.daemon.clone();
        let scope = Arc::clone(&self.scope);
        let goal = async move {
            let copies = read_copies(&daemon).await.map_err(ActionExit::Error)?;
            refuse_removal(&scope, &copies, &name)
                .map_err(|reason| refusal(PeppyFailureKind::Rejected, reason))?;
            Ok(StackRemoveGoal::new(name))
        };
        let started = start_change(self.bridge.daemon.clone(), None, change.clone(), goal);
        wait_for_change(started, &change, surface, window).await
    }

    /// The scoped option `option` names. The narrowed schema admits only
    /// the scope's options.
    fn scoped_option(&self, option: &str) -> Result<&ScopedOption, String> {
        self.scope.option(option).ok_or_else(|| {
            format!(
                "`{option}` is not an option of this endpoint; it adds copies of {}",
                options_phrase(&self.scope)
            )
        })
    }

    /// The `join` goal as the daemon takes it: the copy on its machine, and
    /// the word of each feature on the machine of its part. A machine equal
    /// to this one is the coordinator, and a feature on the copy's machine
    /// names no machine, so its part runs with the copy's base.
    fn typed_join(&self, input: &JoinInput) -> Result<TypedJoin, String> {
        let name = CopyName::new(input.name.as_str()).map_err(|error| error.to_string())?;
        let entry = self.scoped_option(&input.option)?;
        let this = self.bridge.daemon.core_node.as_str();
        let machine = |value: &str| {
            CoreNodeName::new(value).map_err(|error| format!("machine `{value}` {error}"))
        };
        let copy_machine = machine(&input.core_node)?;
        let target_of = |machine: CoreNodeName| {
            if machine.as_str() == this {
                CoreNodeTarget::Coordinator
            } else {
                CoreNodeTarget::Named(machine)
            }
        };
        let mut words = Vec::with_capacity(input.features.len());
        let mut named: Vec<&str> = Vec::with_capacity(input.features.len());
        for item in &input.features {
            let Some(feature) = entry.feature(&item.feature) else {
                return Err(unknown_feature(entry, &item.feature));
            };
            if named.contains(&item.feature.as_str()) {
                return Err(format!(
                    "`features` names feature `{}` twice; name each feature once",
                    item.feature
                ));
            }
            named.push(item.feature.as_str());
            let part_machine = machine(&item.core_node)?;
            let core_node = (part_machine != copy_machine).then(|| target_of(part_machine));
            words.push(feature.word(core_node));
        }
        Ok(TypedJoin {
            copy: CopyReference {
                option: entry.option().clone(),
                name,
                core_node: target_of(copy_machine),
            },
            words,
        })
    }
}

/// The refusal of a change that the bridge gives before any goal reaches the
/// daemon, with `kind`: `rejected` for an input that names what the scope
/// does not hold, `precondition_failed` for a stack or a bridge that does not
/// allow the change now.
fn refusal(kind: PeppyFailureKind, reason: impl Into<String>) -> ActionExit {
    ActionExit::Refused(ProviderFailure::new(kind, reason))
}

/// The goal of a call, which the narrowed schema already checked.
fn parse_input<T: serde::de::DeserializeOwned>(input: &Value) -> Result<T, ActionExit> {
    T::deserialize(input)
        .map_err(|error| ActionExit::Error(format!("the goal does not parse: {error}")))
}

/// The copies on the stack, or the refusal of a change that cannot read
/// them.
async fn read_copies(daemon: &OwnDaemon) -> Result<Vec<CopyInfo>, String> {
    stack_goal::list_copies(daemon.route(), STACK_READ_TIMEOUT)
        .await
        .map(|listed| listed.copies)
        .map_err(|error| format!("the daemon did not list the stack: {error}"))
}

/// Whether `option` is an option of the scope.
fn in_scope(scope: &StackCopiesScope, option: &str) -> bool {
    scope.option(option).is_some()
}

/// The machines presence read, or the failure of `list` when it could not
/// read them.
pub(super) fn live_machines_or_refusal(
    read: peppylib::PeppyResult<std::collections::BTreeSet<String>>,
) -> Result<std::collections::BTreeSet<String>, ToolCallError> {
    read.map_err(|error| {
        ToolCallError::Error(format!(
            "the live machines of the federation could not be read: {error}"
        ))
    })
}

/// The refusal of a feature `feature` that `entry` does not offer, with the
/// features it offers, or that it offers none.
fn unknown_feature(entry: &ScopedOption, feature: &str) -> String {
    let option = entry.option();
    if entry.features().is_empty() {
        return format!("`{feature}` is not a feature of option `{option}`, which has no feature");
    }
    let features: Vec<&str> = entry
        .features()
        .iter()
        .map(|feature| feature.feature().as_str())
        .collect();
    format!(
        "`{feature}` is not a feature of option `{option}`; its features: {}",
        and_list(&features)
    )
}

/// One copy as `list` gives it, in the shape of a `join` goal: its name, its
/// option, its machine, and the features of its option its selections hold,
/// each with the machine of its part, the copy's when the part runs with it.
fn listed_copy(scope: &StackCopiesScope, copy: &CopyInfo) -> Value {
    let features: Vec<Value> = scope
        .option(&copy.option)
        .map(|entry| entry.features_in(&copy.selections))
        .unwrap_or_default()
        .into_iter()
        .map(|placed| {
            json!({
                "feature": placed.feature.feature().as_str(),
                "core_node": placed.core_node.unwrap_or(&copy.core_node).as_str(),
            })
        })
        .collect();
    json!({
        "name": copy.name.as_str(),
        "option": copy.option,
        "core_node": copy.core_node.as_str(),
        "features": features,
    })
}

/// The copies of the scope's options, in name order.
fn scoped_copies<'a>(scope: &StackCopiesScope, copies: &'a [CopyInfo]) -> Vec<&'a CopyInfo> {
    let mut scoped: Vec<&CopyInfo> = copies
        .iter()
        .filter(|copy| in_scope(scope, &copy.option))
        .collect();
    scoped.sort_by(|left, right| left.name.as_str().cmp(right.name.as_str()));
    scoped
}

/// The `change` field of `list`: the action, the copy's name and its
/// option.
fn change_answer(change: &CopyChange) -> Value {
    json!({
        "action": change.action.as_str(),
        "name": change.name.as_str(),
        "option": change.option,
    })
}

/// Step 2 of `join`: a name a copy on the stack has, of any option, and a
/// stack that holds `max_copies` copies of the scope's options or more, are
/// refused.
fn refuse_join(
    scope: &StackCopiesScope,
    copies: &[CopyInfo],
    name: &CopyName,
) -> Result<(), String> {
    if copies.iter().any(|copy| copy.name == *name) {
        return Err(format!(
            "a copy `{name}` is already on the stack; choose another name"
        ));
    }
    let scoped = scoped_copies(scope, copies);
    if scoped.len() < usize::from(scope.max_copies().get()) {
        return Ok(());
    }
    let names: Vec<&str> = scoped.iter().map(|copy| copy.name.as_str()).collect();
    Err(format!(
        "the stack holds {} of the {} ({}), the most the scope of this endpoint allows; remove \
         one first",
        counted(scoped.len(), "copy", "copies"),
        options_phrase(scope),
        names.join(", ")
    ))
}

/// `remove` removes a copy of a scoped option only.
fn refuse_removal(
    scope: &StackCopiesScope,
    copies: &[CopyInfo],
    name: &CopyName,
) -> Result<(), String> {
    if scoped_copies(scope, copies)
        .iter()
        .any(|copy| copy.name == *name)
    {
        return Ok(());
    }
    Err(format!(
        "no copy `{name}` of the {} on the stack",
        options_phrase(scope)
    ))
}

/// The scope's options as a message names them: "option so101_sim",
/// "options openarm_sim and so101_sim", "options a, b and c".
fn options_phrase(scope: &StackCopiesScope) -> String {
    let options: Vec<&str> = scope
        .options()
        .iter()
        .map(|entry| entry.option().as_str())
        .collect();
    let noun = if options.len() == 1 {
        "option"
    } else {
        "options"
    };
    format!("{noun} {}", and_list(&options))
}

/// `items` as a sentence lists them: "a", "a and b", "a, b and c".
fn and_list(items: &[&str]) -> String {
    match items {
        [] => String::new(),
        [only] => (*only).to_owned(),
        [init @ .., last] => format!("{} and {last}", init.join(", ")),
    }
}

/// "1 copy", "4 copies".
fn counted(count: usize, one: &str, many: &str) -> String {
    format!("{count} {}", if count == 1 { one } else { many })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_list_reads_as_a_sentence() {
        assert_eq!(and_list(&[]), "");
        assert_eq!(and_list(&["so101_sim"]), "so101_sim");
        assert_eq!(
            and_list(&["openarm_sim", "so101_sim"]),
            "openarm_sim and so101_sim"
        );
        assert_eq!(and_list(&["a", "b", "c"]), "a, b and c");
    }

    #[test]
    fn a_count_takes_the_noun_of_its_number() {
        assert_eq!(counted(1, "copy", "copies"), "1 copy");
        assert_eq!(counted(4, "copy", "copies"), "4 copies");
        assert_eq!(counted(0, "copy", "copies"), "0 copies");
    }
}
