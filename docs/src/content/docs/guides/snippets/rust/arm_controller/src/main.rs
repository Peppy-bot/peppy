use peppygen::peers::arm::{joint_commands, joint_states};
use peppygen::{NodeBuilder, Parameters, Result};

// `arm_controller` plays the `controller` role of the `arm_link` pairing.
// Both directions of its `arm` slot live under `peppygen::peers::arm`: it
// emits `joint_commands` to and consumes `joint_states` from the single
// arm instance currently paired on the slot. If that arm dies, the slot
// unpairs and the loop simply stops receiving until a new arm is paired.
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
            // Subscribing while unpaired is legal: the subscription follows
            // the slot's live pin, silent until an arm is paired.
            let mut subscription = match joint_states::subscribe(&node_runner).await {
                Ok(subscription) => subscription,
                Err(e) => {
                    eprintln!("Failed to subscribe to joint_states: {e}");
                    return;
                }
            };

            // Optional: block until an arm is paired and log who it is.
            match joint_states::wait_paired(&node_runner).await {
                Ok(peer) => println!(
                    "paired with arm {}/{}",
                    peer.producer.core_node, peer.producer.instance_id
                ),
                Err(e) => {
                    eprintln!("Failed to wait for a paired arm: {e}");
                    return;
                }
            }

            loop {
                let (producer, state) = match subscription.next().await {
                    Ok(Some(received)) => received,
                    Ok(None) => break,
                    Err(e) => {
                        eprintln!("Error receiving joint state: {e}");
                        continue;
                    }
                };

                // `producer` is always the paired arm's identity.
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
