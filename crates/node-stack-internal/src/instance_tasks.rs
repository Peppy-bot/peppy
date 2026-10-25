//! The background tasks that supervise the instances of a stack: the exit
//! watcher of each process, the wait of a restart and the health monitor.
//! The stack counts the live ones of each instance id, so a test can wait
//! until nothing more acts on an instance before it reads what the instance
//! became.

use std::collections::BTreeMap;
use std::future::Future;
use std::sync::Arc;

use config::runtime::Name;
use tokio::sync::watch;
use tokio::task::JoinHandle;

/// The live background tasks of each instance id. Clones share the count.
#[derive(Clone, Debug)]
pub struct InstanceTasks {
    live: Arc<watch::Sender<BTreeMap<Name, usize>>>,
}

impl Default for InstanceTasks {
    fn default() -> Self {
        Self {
            live: Arc::new(watch::Sender::new(BTreeMap::new())),
        }
    }
}

impl InstanceTasks {
    /// Spawns `task` as a background task of `instance_id`, which counts as
    /// live until it ends or is aborted.
    pub fn spawn<F>(&self, instance_id: &Name, task: F) -> JoinHandle<F::Output>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        let live = LiveTask::begin(&self.live, instance_id);
        tokio::spawn(async move {
            let _live = live;
            task.await
        })
    }

    /// How many background tasks of `instance_id` are live.
    pub fn live(&self, instance_id: &Name) -> usize {
        self.live.borrow().get(instance_id).copied().unwrap_or(0)
    }

    /// Waits until no background task of `instance_id` is live.
    #[cfg(any(test, feature = "test-support"))]
    pub async fn ended(&self, instance_id: &Name) {
        let mut live = self.live.subscribe();
        let _ = live.wait_for(|live| !live.contains_key(instance_id)).await;
    }
}

/// One live task in the count of its instance, from its spawn until its
/// future is dropped.
struct LiveTask {
    live: Arc<watch::Sender<BTreeMap<Name, usize>>>,
    instance_id: Name,
}

impl LiveTask {
    fn begin(live: &Arc<watch::Sender<BTreeMap<Name, usize>>>, instance_id: &Name) -> Self {
        live.send_modify(|live| *live.entry(instance_id.clone()).or_default() += 1);
        Self {
            live: Arc::clone(live),
            instance_id: instance_id.clone(),
        }
    }
}

impl Drop for LiveTask {
    fn drop(&mut self) {
        self.live.send_modify(|live| {
            if let Some(count) = live.get_mut(&self.instance_id) {
                *count -= 1;
                if *count == 0 {
                    live.remove(&self.instance_id);
                }
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::InstanceTasks;
    use config::runtime::Name;

    fn id(name: &str) -> Name {
        Name::new(name).expect("a valid instance id")
    }

    #[tokio::test]
    async fn a_task_is_live_until_it_ends() {
        let tasks = InstanceTasks::default();
        let (release, released) = tokio::sync::oneshot::channel::<()>();
        let task = tasks.spawn(&id("a"), async move {
            let _ = released.await;
        });
        assert_eq!(tasks.live(&id("a")), 1);
        assert_eq!(tasks.live(&id("b")), 0, "another instance has none");

        release.send(()).expect("the task waits");
        tasks.ended(&id("a")).await;
        assert_eq!(tasks.live(&id("a")), 0);
        task.await.expect("the task ended");
    }

    #[tokio::test]
    async fn the_tasks_of_an_instance_end_with_its_last_one() {
        let tasks = InstanceTasks::default();
        let (first_release, first_released) = tokio::sync::oneshot::channel::<()>();
        let (second_release, second_released) = tokio::sync::oneshot::channel::<()>();
        tasks.spawn(&id("a"), async move {
            let _ = first_released.await;
        });
        let second = tasks.spawn(&id("a"), async move {
            let _ = second_released.await;
        });
        assert_eq!(tasks.live(&id("a")), 2);

        first_release.send(()).expect("the first task waits");
        let mut live = tasks.live.subscribe();
        let _ = live.wait_for(|live| live.get(&id("a")) == Some(&1)).await;
        second_release.send(()).expect("the second task waits");
        tasks.ended(&id("a")).await;
        second.await.expect("the second task ended");
    }

    #[tokio::test]
    async fn an_aborted_task_is_not_live() {
        let tasks = InstanceTasks::default();
        let task = tasks.spawn(&id("a"), std::future::pending::<()>());
        task.abort();
        tasks.ended(&id("a")).await;
        assert!(task.await.expect_err("aborted").is_cancelled());
    }
}
