use peppygen::paired_topics::controller::{joint_commands, joint_states};
use peppygen::{NodeBuilder, Parameters, Result};

// `robot_arm` plays the `arm` role of the `arm_link` pairing. Both
// directions of its `controller` slot live under
// `peppygen::paired_topics::controller`: it consumes `joint_commands` from and
// emits `joint_states` to whichever single controller instance is
// currently paired on the slot. Unpaired, the subscription stays silent
// and publishes go nowhere; the code does not change either way.
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
            // Subscribing while unpaired is legal: the held subscription
            // yields nothing until a controller pairs, then only that
            // controller's messages.
            let mut subscription = match joint_commands::subscribe(&node_runner).await {
                Ok(subscription) => subscription,
                Err(e) => {
                    eprintln!("Failed to subscribe to joint_commands: {e}");
                    return;
                }
            };

            // Optional: block until a controller is paired and log who it is.
            match joint_commands::wait_paired(&node_runner).await {
                Ok(peer) => println!(
                    "paired with controller {}/{}",
                    peer.producer.core_node, peer.producer.instance_id
                ),
                Err(e) => {
                    eprintln!("Failed to wait for a paired controller: {e}");
                    return;
                }
            }

            loop {
                let (producer, command) = match subscription.next().await {
                    Ok(Some(received)) => received,
                    Ok(None) => break,
                    Err(e) => {
                        eprintln!("Error receiving joint command: {e}");
                        continue;
                    }
                };

                // `producer` is always the paired controller's identity.
                println!(
                    "command from {}/{}: target={:?} max_vel={}",
                    producer.core_node,
                    producer.instance_id,
                    command.target_positions,
                    command.max_velocity
                );

                // Drive the joints, then report the resulting state back to
                // the paired controller.
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
