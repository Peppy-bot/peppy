use peppygen::consumed_topics::controller_joint_commands;
use peppygen::emitted_topics::joint_state_source::v1::joint_states;
use peppygen::{NodeBuilder, Parameters, Result};

// `robot_arm` drives the physical joints and reports their state. It emits
// `joint_states` by conforming to `joint_state_source`, and consumes
// `joint_commands` from any conforming controller through a `from_any`
// interface slot. With no binding required it boots with zero controllers
// and picks up whichever ones publish, identified per message by instance_id.
fn main() -> Result<()> {
    NodeBuilder::new().run(|_args: Parameters, node_runner| async move {
        tokio::spawn(async move {
            loop {
                let (instance_id, command) =
                    controller_joint_commands::on_next_message_received(&node_runner, None)
                        .await
                        .expect("failed to receive joint command");

                println!(
                    "received from {instance_id}: target={:?} max_vel={}",
                    command.target_positions, command.max_velocity
                );

                // Drive the joints, then report the resulting state.
                let _ = joint_states::emit(
                    &node_runner,
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
