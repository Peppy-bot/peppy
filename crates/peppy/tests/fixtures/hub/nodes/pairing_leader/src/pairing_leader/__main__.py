"""A leader that sends its own value as every setpoint and logs what it
hears back on its pair. A hub answers each leader on that leader's pair,
so every value this log carries is this leader's own.
"""

import asyncio
import time
import traceback

from peppygen import NodeBuilder, NodeRunner
from peppygen.paired_topics.hub import joint_setpoints, joint_states
from peppygen.parameters import Parameters

LOG = "[pairing-leader]"
JOINTS = 7


async def send_setpoints(node_runner: NodeRunner, params: Parameters) -> None:
    publisher = await joint_setpoints.declare_publisher(node_runner)
    token = node_runner.cancellation_token()
    interval = 1.0 / params.publish_rate_hz
    while not token.is_cancelled():
        await publisher.publish(
            joint_setpoints.build_message(time.time(), [params.value] * JOINTS)
        )
        await asyncio.sleep(interval)


async def hear_states(node_runner: NodeRunner, me: str) -> None:
    subscription = await joint_states.subscribe(node_runner)
    while True:
        pair = await subscription.next()
        if pair is None:
            return
        peer, message = pair
        print(
            f"{LOG} {me} heard positions={list(message.positions)} "
            f"from {peer.producer.instance_id}",
            flush=True,
        )


async def setup(params: Parameters, node_runner: NodeRunner) -> list:
    me = node_runner.copy() or "outside-a-copy"
    print(f"{LOG} {me} up with value {params.value}", flush=True)
    return [
        asyncio.create_task(send_setpoints(node_runner, params)),
        asyncio.create_task(hear_states(node_runner, me)),
    ]


def main() -> None:
    try:
        NodeBuilder().run(setup)
    except Exception:
        traceback.print_exc()
        raise


if __name__ == "__main__":
    main()
