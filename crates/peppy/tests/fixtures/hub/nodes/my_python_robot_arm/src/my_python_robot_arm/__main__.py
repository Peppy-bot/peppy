"""An arm that publishes joint states without moving anything.

The positions it reports walk a fixed ramp rather than tracking a command, which
is all a consumer of this stream needs: what the fixture proves is that a native
Python node builds with `uv` and carries messages, not that a controller
converges.
"""

import asyncio
import time

from peppygen import NodeBuilder, NodeRunner
from peppygen.emitted_topics import joint_states
from peppygen.parameters import Parameters

# How far each published sample advances every joint. Small enough that the
# reported positions stay readable over a whole test run.
STEP = 0.01

VELOCITIES = [0.0, 0.0, 0.0]


async def publish_joint_states(node_runner: NodeRunner, params: Parameters) -> None:
    publisher = await joint_states.declare_publisher(node_runner)
    token = node_runner.cancellation_token()
    interval = 1.0 / params.publish_rate_hz

    positions = [0.0, 0.0, 0.0]
    while not token.is_cancelled():
        positions = [round(position + STEP, 3) for position in positions]
        await publisher.publish(
            joint_states.build_message(positions, VELOCITIES, time.time())
        )
        print(f"[arm] published joint_states: positions={positions}", flush=True)
        await asyncio.sleep(interval)


async def setup(params: Parameters, node_runner: NodeRunner) -> list[asyncio.Task]:
    async def announce_shutdown():
        print("[arm] Shutdown signal received", flush=True)

    node_runner.on_shutdown(announce_shutdown)

    return [asyncio.create_task(publish_joint_states(node_runner, params))]


def main():
    NodeBuilder().run(setup)


if __name__ == "__main__":
    main()
