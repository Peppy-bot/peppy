"""The launch's scripted source of simulated time.

Publishes a fixed ramp of ticks to every machine of the launch, then
republishes the final instant at the same cadence for the life of the node:
time tops out rather than going silent, so a machine whose subscription came
up after the ramp still converges on the same final instant. The values are
scripted rather than read from any clock, so a test can assert exact instants
and a capped tail with no tolerance for host speed.
"""

import asyncio

from peppygen import NodeBuilder, NodeRunner
from peppygen.parameters import Parameters
from peppylib.clock import SimTimePublisher


async def publish_scripted_ticks(node_runner: NodeRunner, params: Parameters) -> None:
    publisher = await SimTimePublisher.for_node(node_runner)
    if publisher is None:
        raise RuntimeError("the launch did not declare this instance its time source")
    print(
        f"[clock-source] publishing to {', '.join(publisher.participants)}",
        flush=True,
    )
    interval = params.tick_interval_ms / 1000.0
    for tick in range(1, params.tick_count + 1):
        await publisher.publish(params.tick_step_ns * tick)
        await asyncio.sleep(interval)
    final_ns = params.tick_step_ns * params.tick_count
    print(f"[clock-source] final tick {final_ns} published; holding", flush=True)

    token = node_runner.cancellation_token()
    while not token.is_cancelled():
        await publisher.publish(final_ns)
        await asyncio.sleep(interval)


async def setup(params: Parameters, node_runner: NodeRunner) -> list[asyncio.Task]:
    return [asyncio.create_task(publish_scripted_ticks(node_runner, params))]


def main():
    NodeBuilder().run(setup)


if __name__ == "__main__":
    main()
