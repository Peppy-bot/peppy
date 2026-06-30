use peppygen::consumed_topics::arm_joint_states;
use peppygen::emitted_topics::joint_command_source::v1::joint_commands;
use peppygen::{NodeBuilder, Parameters, Result};

// `arm_controller` plans trajectories and sends joint commands. It emits
// `joint_commands` by conforming to `joint_command_source`, and consumes
// `joint_states` from any conforming arm through a `from_any` interface
// slot. With no binding required it accepts state from whichever arms are
// present and tells them apart by the (core_node, instance_id) returned
// per message.
fn main() -> Result<()> {
    NodeBuilder::new().run(|_args: Parameters, node_runner| async move {
        tokio::spawn(async move {
            // Declare the publisher once; every publish below is then lock-free.
            let publisher = match joint_commands::declare_publisher(&node_runner).await {
                Ok(publisher) => publisher,
                Err(e) => {
                    eprintln!("Failed to declare joint_commands publisher: {e}");
                    return;
                }
            };
            // Subscribe once; the held subscription buffers state messages in
            // order, so the loop never misses one published between iterations.
            let mut subscription = match arm_joint_states::subscribe(&node_runner).await {
                Ok(subscription) => subscription,
                Err(e) => {
                    eprintln!("Failed to subscribe to joint_states: {e}");
                    return;
                }
            };
            loop {
                let (producer, state) = match subscription.next().await {
                    Ok(Some(received)) => received,
                    Ok(None) => break,
                    Err(e) => {
                        eprintln!("Error receiving joint state: {e}");
                        continue;
                    }
                };

                println!(
                    "state from {}/{}: positions={:?}",
                    producer.core_node, producer.instance_id, state.positions
                );

                // Compute the next target from the reported state, then command it.
                let target = compute_next_target(&state.positions);
                match joint_commands::build_message(target, 1.0) {
                    Ok(payload) => {
                        if let Err(e) = publisher.publish(payload).await {
                            eprintln!("Failed to publish joint command: {e}");
                        }
                    }
                    Err(e) => eprintln!("Failed to build joint_commands message: {e}"),
                }
            }
        });

        Ok(())
    })
}

fn compute_next_target(current: &[f64; 3]) -> [f64; 3] {
    // Trajectory planning logic
    [current[0] + 0.1, current[1], current[2]]
}
