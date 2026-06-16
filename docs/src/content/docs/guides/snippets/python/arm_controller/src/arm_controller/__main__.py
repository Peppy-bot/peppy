import asyncio
import sys

from peppygen import NodeBuilder, NodeRunner
from peppygen.parameters import Parameters
from peppygen.consumed_topics import arm_joint_states
from peppygen.emitted_topics.joint_command_source.v1 import joint_commands


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

    while True:
        try:
            producer, state = await arm_joint_states.on_next_message_received(
                node_runner,
            )
        except Exception as e:
            # Log the failure, then pause before retrying so a persistent
            # receive error does not spin the loop at full speed.
            print(f"Error receiving joint state: {e}", file=sys.stderr)
            await asyncio.sleep(1.0)
            continue

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
