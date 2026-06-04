import asyncio

from peppygen import NodeBuilder, NodeRunner
from peppygen.parameters import Parameters
from peppygen.consumed_topics import arm_joint_states
from peppygen.emitted_topics.joint_command_source.v1 import joint_commands


def compute_next_target(current: list[float]) -> list[float]:
    # Trajectory planning logic
    return [current[0] + 0.1, current[1], current[2]]


async def control_loop(node_runner: NodeRunner):
    while True:
        instance_id, state = await arm_joint_states.on_next_message_received(
            node_runner,
        )

        print(f"state from {instance_id}: positions={state.positions}")

        # Compute the next target from the reported state, then command it.
        target = compute_next_target(state.positions)
        await joint_commands.emit(
            node_runner,
            target,
            1.0,  # max_velocity
        )


async def setup(_params: Parameters, node_runner: NodeRunner) -> list[asyncio.Task]:
    return [asyncio.create_task(control_loop(node_runner))]


def main():
    NodeBuilder().run(setup)


if __name__ == "__main__":
    main()
