use peppygen::consumed_topics::joint_commands;
use peppygen::emitted_topics::joint_states;
use peppygen::{NodeBuilder, Parameters, Result};

fn main() -> Result<()> {
    NodeBuilder::new().run(|_args: Parameters, node_runner| async move {
        let node_runner_clone = node_runner.clone();
        tokio::spawn(async move {
            loop {
                let (instance_id, command) = joint_commands::on_next_message_received(
                    &node_runner_clone,
                    None, // from_core_node (None = any)
                    None, // from_instance_id (None = any)
                )
                .await
                .expect("failed to receive joint command");

                println!(
                    "received from {}: target={:?} max_vel={}",
                    instance_id, command.target_positions, command.max_velocity
                );

                // Drive the joints, then report state
                let _ = joint_states::emit(
                    &node_runner_clone,
                    command.target_positions,
                    [0.0, 0.0, 0.0],
                    std::time::SystemTime::now(),
                )
                .await;
            }
        });

        Ok(())
    })
}
