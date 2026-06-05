use peppygen::consumed_topics::arm_joint_states;
use peppygen::emitted_topics::joint_command_source::v1::joint_commands;
use peppygen::{NodeBuilder, Parameters, Result};

// `arm_controller` plans trajectories and sends joint commands. It emits
// `joint_commands` by conforming to `joint_command_source`, and consumes
// `joint_states` from any conforming arm through a `from_any` interface
// slot. With no binding required it accepts state from whichever arms are
// present and tells them apart by the instance_id returned per message.
fn main() -> Result<()> {
    NodeBuilder::new().run(|_args: Parameters, node_runner| async move {
        tokio::spawn(async move {
            loop {
                let (instance_id, state) =
                    arm_joint_states::on_next_message_received(&node_runner, None)
                        .await
                        .expect("failed to receive joint state");

                println!("state from {instance_id}: positions={:?}", state.positions);

                // Compute the next target from the reported state, then command it.
                let target = compute_next_target(&state.positions);
                let _ = joint_commands::emit(&node_runner, target, 1.0).await;
            }
        });

        Ok(())
    })
}

fn compute_next_target(current: &[f64; 3]) -> [f64; 3] {
    // Trajectory planning logic
    [current[0] + 0.1, current[1], current[2]]
}
