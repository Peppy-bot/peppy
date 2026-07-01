import asyncio
import sys
import time

from peppygen import NodeBuilder, NodeRunner
from peppygen.parameters import Parameters
from peppygen.consumed_topics import controller_joint_commands
from peppygen.emitted_topics.joint_state_source.v1 import joint_states


async def handle_commands(node_runner: NodeRunner):
    # Declare the publisher once, then publish each state on it.
    try:
        publisher = await joint_states.declare_publisher(node_runner)
    except Exception as e:
        print(f"Failed to declare joint_states publisher: {e}", file=sys.stderr)
        return

    # Subscribe once; the held subscription buffers commands in order, so
    # iterating never drops one published between iterations.
    try:
        subscription = await controller_joint_commands.subscribe(node_runner)
    except Exception as e:
        print(f"Failed to subscribe to controller_joint_commands: {e}", file=sys.stderr)
        return

    async for producer, command in subscription:
        print(
            f"received from {producer.core_node}/{producer.instance_id}: "
            f"target={command.target_positions} max_vel={command.max_velocity}"
        )

        # Drive the joints, then report the resulting state.
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
