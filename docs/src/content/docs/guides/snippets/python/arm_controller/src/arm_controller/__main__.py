import asyncio
import sys

from peppygen import NodeBuilder, NodeRunner
from peppygen.parameters import Parameters
from peppygen.paired_topics.arm import joint_commands, joint_states

# `arm_controller` plays the `controller` role of the `arm_link` pairing.
# Both directions of its `arm` slot live under `peppygen.paired_topics.arm`: it
# emits `joint_commands` to and consumes `joint_states` from the single
# arm instance currently paired on the slot. If that arm dies, the slot
# unpairs and the loop simply stops receiving until a new arm is paired.


def compute_next_target(current: list[float]) -> list[float]:
    # Trajectory planning logic
    return [current[0] + 0.1, current[1], current[2]]


async def control_loop(node_runner: NodeRunner):
    # Declare the publisher once, then publish each command on it.
    try:
        publisher = await joint_commands.declare_publisher(node_runner)
    except Exception as e:
        print(f"Failed to declare joint_commands publisher: {e}", file=sys.stderr)
        return

    # Subscribing while unpaired is legal: the subscription follows the
    # slot's live pin, silent until an arm is paired.
    try:
        subscription = await joint_states.subscribe(node_runner)
    except Exception as e:
        print(f"Failed to subscribe to joint_states: {e}", file=sys.stderr)
        return

    # Optional: block until an arm is paired and log who it is.
    try:
        peer = await joint_states.wait_paired(node_runner)
        print(f"paired with arm {peer.producer.core_node}/{peer.producer.instance_id}")
    except Exception as e:
        print(f"Failed to wait for a paired arm: {e}", file=sys.stderr)
        return

    while True:
        try:
            received = await subscription.next()
        except Exception as e:
            # Log the failure, then pause before retrying so a persistent
            # receive error does not spin the loop at full speed.
            print(f"Error receiving joint state: {e}", file=sys.stderr)
            await asyncio.sleep(1.0)
            continue

        if received is None:
            break  # subscription closed
        producer, state = received

        # `producer` is always the paired arm's identity.
        print(
            f"state from {producer.core_node}/{producer.instance_id}: "
            f"positions={state.positions}"
        )

        # Compute the next target from the reported state, then command it.
        target = compute_next_target(state.positions)
        try:
            await publisher.publish(
                joint_commands.build_message(
                    target,
                    1.0,  # max_velocity
                )
            )
        except Exception as e:
            print(f"Failed to publish joint command: {e}", file=sys.stderr)


async def setup(_params: Parameters, node_runner: NodeRunner) -> list[asyncio.Task]:
    return [asyncio.create_task(control_loop(node_runner))]


def main():
    NodeBuilder().run(setup)


if __name__ == "__main__":
    main()
