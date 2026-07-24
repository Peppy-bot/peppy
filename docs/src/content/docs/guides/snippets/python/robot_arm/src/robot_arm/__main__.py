import asyncio
import sys
import time

from peppygen import NodeBuilder, NodeRunner
from peppygen.parameters import Parameters
from peppygen.paired_topics.controller import joint_commands, joint_states

# `robot_arm` plays the `arm` role of the `arm_link` pairing. Both
# directions of its `controller` slot live under
# `peppygen.paired_topics.controller`: it consumes `joint_commands` from and emits
# `joint_states` to whichever single controller instance is currently
# paired on the slot. Unpaired, the subscription stays silent and
# publishes go nowhere; the code does not change either way.


async def handle_commands(node_runner: NodeRunner):
    # Declare the publisher once, then publish each state on it.
    try:
        publisher = await joint_states.declare_publisher(node_runner)
    except Exception as e:
        print(f"Failed to declare joint_states publisher: {e}", file=sys.stderr)
        return

    # Subscribing while unpaired is legal: the held subscription yields
    # nothing until a controller pairs, then only that controller's messages.
    try:
        subscription = await joint_commands.subscribe(node_runner)
    except Exception as e:
        print(f"Failed to subscribe to joint_commands: {e}", file=sys.stderr)
        return

    # Optional: block until a controller is paired and log who it is.
    try:
        peer = await joint_commands.wait_paired(node_runner)
        print(f"paired with controller {peer.producer.core_node}/{peer.producer.instance_id}")
    except Exception as e:
        print(f"Failed to wait for a paired controller: {e}", file=sys.stderr)
        return

    while True:
        try:
            received = await subscription.next()
        except Exception as e:
            # Log the failure, then pause before retrying so a persistent
            # receive error does not spin the loop at full speed.
            print(f"Error receiving joint command: {e}", file=sys.stderr)
            await asyncio.sleep(1.0)
            continue

        if received is None:
            break  # subscription closed
        producer, command = received

        # `producer` is always the paired controller's identity.
        print(
            f"command from {producer.core_node}/{producer.instance_id}: "
            f"target={command.target_positions} max_vel={command.max_velocity}"
        )

        # Drive the joints, then report the resulting state back to the
        # paired controller.
        try:
            await publisher.publish(
                joint_states.build_message(
                    command.target_positions,
                    [0.0, 0.0, 0.0],
                    time.time(),
                )
            )
        except Exception as e:
            print(f"Failed to publish joint state: {e}", file=sys.stderr)


async def setup(_params: Parameters, node_runner: NodeRunner) -> list[asyncio.Task]:
    return [asyncio.create_task(handle_commands(node_runner))]


def main():
    NodeBuilder().run(setup)


if __name__ == "__main__":
    main()
