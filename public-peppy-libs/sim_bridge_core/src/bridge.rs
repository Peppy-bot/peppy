use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::pipeline::{BoxFuture, run_os_to_sim, run_sim_to_os};
use crate::transport::RawTransport;

type Pipeline = Pin<Box<dyn Future<Output = ()> + Send + 'static>>;

#[derive(Debug, Clone)]
pub struct ArmMergeState {
    inner: Arc<std::sync::Mutex<Vec<f64>>>,
    total_joints: usize,
}

impl ArmMergeState {
    pub fn new(total_joints: usize) -> Self {
        Self {
            inner: Arc::new(std::sync::Mutex::new(vec![0.0; total_joints])),
            total_joints,
        }
    }

    pub fn update_and_merge(&self, indices: &[usize], positions: &[f64]) -> Vec<f64> {
        debug_assert_eq!(
            indices.len(),
            positions.len(),
            "indices and positions length mismatch"
        );
        let mut state = self.inner.lock().expect("arm merge state poisoned");
        for (i, &idx) in indices.iter().enumerate() {
            if idx < self.total_joints {
                state[idx] = positions[i];
            }
        }
        state.clone()
    }
}

pub struct SimBridge<Runner, T: RawTransport> {
    runner: Arc<Runner>,
    transport: Arc<T>,
    token: CancellationToken,
    sim_node: Arc<str>,
    pipelines: Vec<Pipeline>,
}

impl<Runner: Send + Sync + 'static, T: RawTransport> SimBridge<Runner, T> {
    pub fn new(
        runner: Arc<Runner>,
        transport: Arc<T>,
        token: CancellationToken,
        sim_node: Arc<str>,
    ) -> Self {
        Self {
            runner,
            transport,
            token,
            sim_node,
            pipelines: Vec::new(),
        }
    }

    pub fn sim_to_os<Msg, EmitFn>(mut self, topic: Arc<str>, emit_fn: EmitFn) -> Self
    where
        Msg: for<'de> Deserialize<'de> + Send + 'static,
        EmitFn: Fn(Arc<Runner>, Msg) -> BoxFuture<std::result::Result<(), String>> + Send + 'static,
    {
        self.pipelines.push(Box::pin(run_sim_to_os(
            self.transport.clone(),
            self.runner.clone(),
            self.token.clone(),
            self.sim_node.clone(),
            topic,
            emit_fn,
        )));
        self
    }

    pub fn os_to_sim<Msg, RecvFn>(mut self, topic: Arc<str>, recv_fn: RecvFn) -> Self
    where
        Msg: Serialize + Send + 'static,
        RecvFn: Fn(Arc<Runner>) -> BoxFuture<std::result::Result<(String, Msg), String>>
            + Send
            + 'static,
    {
        self.pipelines.push(Box::pin(run_os_to_sim(
            self.transport.clone(),
            self.runner.clone(),
            self.token.clone(),
            topic,
            recv_fn,
        )));
        self
    }

    pub async fn run(self) {
        let handles: Vec<_> = self.pipelines.into_iter().map(tokio::spawn).collect();
        for handle in handles {
            // Surface panics + cancellations from spawned pipelines so a failed
            // pipeline doesn't silently degrade telemetry/command flow.
            if let Err(e) = handle.await {
                tracing::warn!(error = %e, "sim bridge pipeline task failed");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arm_merge_state_updates_correct_indices() {
        let state = ArmMergeState::new(4);
        let merged = state.update_and_merge(&[1, 3], &[1.1, 3.3]);
        assert_eq!(merged, vec![0.0, 1.1, 0.0, 3.3]);
    }

    #[test]
    fn arm_merge_state_skips_out_of_range() {
        let state = ArmMergeState::new(2);
        let merged = state.update_and_merge(&[0, 5], &[9.9, 1.0]);
        assert_eq!(merged, vec![9.9, 0.0]);
    }

    #[test]
    fn arm_merge_state_accumulates_across_calls() {
        let state = ArmMergeState::new(4);
        state.update_and_merge(&[0, 1], &[1.0, 2.0]);
        let merged = state.update_and_merge(&[2, 3], &[3.0, 4.0]);
        assert_eq!(merged, vec![1.0, 2.0, 3.0, 4.0]);
    }
}
