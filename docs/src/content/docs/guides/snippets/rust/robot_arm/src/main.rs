use peppygen::consumed_topics::controller_joint_commands;
use peppygen::emitted_topics::joint_state_source::v1::joint_states;
use peppygen::{NodeBuilder, Parameters, Result};

// `robot_arm` drives the physical joints and reports their state. It emits
// `joint_states` by conforming to `joint_state_source`, and consumes
// `joint_commands` from any conforming controller through a `from_any`
// interface slot. With no binding required it boots with zero controllers
// and picks up whichever ones publish, identified per message by the
// producer's full (core_node, instance_id).
fn main() -> Result<()> {
    NodeBuilder::new().run(|_args: Parameters, node_runner| async move {
        tokio::spawn(async move {
            // Declare the publisher once; every publish below is then lock-free.
            let publisher = match joint_states::declare_publisher(&node_runner).await {
                Ok(publisher) => publisher,
                Err(e) => {
                    eprintln!("Failed to declare joint_states publisher: {e}");
                    return;
                }
            };
            // Subscribe once; the held subscription buffers commands in order,
            // so the loop never misses one published between iterations.
            let mut subscription = controller_joint_commands::subscribe(&node_runner)
                .await
                .expect("failed to subscribe to joint commands");
            loop {
                let (producer, command) = match subscription.next().await {
                    Ok(Some(received)) => received,
                    Ok(None) => break,
                    Err(e) => {
                        eprintln!("Error receiving joint command: {e}");
                        continue;
                    }
                };

                println!(
                    "received from {}/{}: target={:?} max_vel={}",
                    producer.core_node,
                    producer.instance_id,
                    command.target_positions,
                    command.max_velocity
                );

                // Drive the joints, then report the resulting state.
                match joint_states::build_message(
                    command.target_positions,
                    [0.0, 0.0, 0.0],
                    std::time::SystemTime::now(),
                ) {
                    Ok(payload) => {
                        if let Err(e) = publisher.publish(payload).await {
                            eprintln!("Failed to publish joint state: {e}");
                        }
                    }
                    Err(e) => eprintln!("Failed to build joint_states message: {e}"),
                }
            }
        });

        Ok(())
    })
}
