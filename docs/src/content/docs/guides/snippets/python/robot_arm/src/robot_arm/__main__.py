import asyncio
import time

from peppygen import NodeBuilder, NodeRunner
from peppygen.parameters import Parameters
from peppygen.consumed_topics import controller_joint_commands
from peppygen.emitted_topics.joint_state_source.v1 import joint_states


async def handle_commands(node_runner: NodeRunner):
    while True:
        instance_id, command = await controller_joint_commands.on_next_message_received(
            node_runner,
        )

        print(
            f"received from {instance_id}: "
            f"target={command.target_positions} max_vel={command.max_velocity}"
        )

        # Drive the joints, then report the resulting state.
        await joint_states.emit(
            node_runner,
            command.target_positions,
            [0.0, 0.0, 0.0],
            time.time(),
        )


async def setup(_params: Parameters, node_runner: NodeRunner) -> list[asyncio.Task]:
    return [asyncio.create_task(handle_commands(node_runner))]


def main():
    NodeBuilder().run(setup)


if __name__ == "__main__":
    main()
