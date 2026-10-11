//! The bridge of `stack_copies:v1`: `list`, `join` and `remove` of the
//! copies of the running launch, within the scope of a target.
//!
//! - `list` answers the scope's options with their descriptions, the copies
//!   whose option is in the scope, in name order, `max_copies`, and the
//!   `change` that runs now when it adds or removes a copy of a scoped
//!   option.
//! - `join` parses the name as a copy name, takes the bridge's lock, reads
//!   the copies of the stack, refuses a name a copy has and a stack that
//!   holds `max_copies` copies of the scope's options, and sends the join of
//!   the option under the name, with no selection, no argument and no
//!   environment, placed on the coordinator, under the default budgets.
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
use core_node_api::encoding::{
    CopyChange, CopyInfo, STACK_BUSY_REASON, StackJoinGoal, StackRemoveGoal,
};
use daemon_config::daemon_interface::{ScopedOption, StackCopiesScope};
use daemon_config::launcher::parse_copy_name;
use peppy_mcp_runtime::{ActionExit, ToolCallError};
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
        let listed = stack_goal::list_copies(self.bridge.daemon.route(), deadline)
            .await
            .map_err(|error| {
                ToolCallError::Failed(format!("the daemon did not list the stack: {error}"))
            })?;
        let options: Vec<Value> = self
            .scope
            .options()
            .iter()
            .map(|entry| json!({ "option": entry.option.as_str(), "description": entry.description }))
            .collect();
        let copies: Vec<Value> = scoped_copies(&self.scope, &listed.copies)
            .into_iter()
            .map(|copy| json!({ "name": copy.name.as_str(), "option": copy.option }))
            .collect();
        let mut answer = json!({
            "options": options,
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
        let name = parse_copy_name(&input.name).map_err(ActionExit::Failed)?;
        let option = self.scoped_option(&input.option)?.option.clone();
        let join = Arc::clone(&self.bridge.joins)
            .try_lock_owned()
            .map_err(|_| ActionExit::Failed(STACK_BUSY_REASON.to_owned()))?;
        let admission = Arc::clone(&self.bridge.admission).lock_owned().await;
        let change = Change::Addition {
            name: name.clone(),
            option: option.clone(),
        };
        let daemon = self.bridge.daemon.clone();
        let scope = Arc::clone(&self.scope);
        let goal = async move {
            let copies = read_copies(&daemon).await?;
            refuse_join(&scope, &copies, &name)?;
            Ok(StackJoinGoal::new(name, option.as_str(), DEFAULT_BUDGETS))
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
        let name = parse_copy_name(&input.name).map_err(ActionExit::Failed)?;
        let change = Change::Removal { name: name.clone() };
        let daemon = self.bridge.daemon.clone();
        let scope = Arc::clone(&self.scope);
        let goal = async move {
            let copies = read_copies(&daemon).await?;
            refuse_removal(&scope, &copies, &name)?;
            Ok(StackRemoveGoal::new(name))
        };
        let started = start_change(self.bridge.daemon.clone(), None, change.clone(), goal);
        wait_for_change(started, &change, surface, window).await
    }

    /// The scoped option `option` names. The narrowed schema admits only
    /// the scope's options.
    fn scoped_option(&self, option: &str) -> Result<&ScopedOption, ActionExit> {
        self.scope
            .options()
            .iter()
            .find(|entry| entry.option.as_str() == option)
            .ok_or_else(|| {
                ActionExit::Failed(format!(
                    "`{option}` is not an option of this endpoint; it adds copies of {}",
                    options_phrase(&self.scope)
                ))
            })
    }
}

/// The goal of a call, which the narrowed schema already checked.
fn parse_input<T: serde::de::DeserializeOwned>(input: &Value) -> Result<T, ActionExit> {
    T::deserialize(input)
        .map_err(|error| ActionExit::Failed(format!("the goal does not parse: {error}")))
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
    scope
        .options()
        .iter()
        .any(|entry| entry.option.as_str() == option)
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
    name: &config::runtime::Name,
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
    name: &config::runtime::Name,
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
        .map(|entry| entry.option.as_str())
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
