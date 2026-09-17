"""A follower that holds one pair per leader on one slot.

Every setpoint arrives tagged with the pair it came on; the hub logs the
leader's instance and the copy the pair belongs to, and answers on that
pair alone with the positions it was sent, so a leader hears its own
values back and never another leader's. The pairs the slot holds are
logged whenever the set changes, in the order they were established,
which is what the cross-daemon pairing test reads.
"""

import asyncio
import time
import traceback

from peppygen import NodeBuilder, NodeRunner
from peppygen.paired_topics.limbs import joint_setpoints, joint_states
from peppygen.parameters import Parameters

LOG = "[pairing-hub]"


def held_pairs(node_runner: NodeRunner) -> str:
    """The pairs the slot holds, as `instance/copy` in establishment order."""
    return ",".join(
        f"{member.info.producer.instance_id}/{member.copy or '-'}"
        for member in joint_setpoints.peers(node_runner)
    )


async def report_pairs(node_runner: NodeRunner, params: Parameters) -> None:
    token = node_runner.cancellation_token()
    interval = 1.0 / params.report_rate_hz
    last = None
    while not token.is_cancelled():
        held = held_pairs(node_runner)
        if held != last:
            print(f"{LOG} peers={held}", flush=True)
            last = held
        await asyncio.sleep(interval)


async def answer_setpoints(node_runner: NodeRunner) -> None:
    publisher = await joint_states.declare_publisher(node_runner)
    subscription = await joint_setpoints.subscribe(node_runner)
    while True:
        pair = await subscription.next()
        if pair is None:
            return
        peer, message = pair
        copy = next(
            (member.copy for member in joint_setpoints.peers(node_runner) if member.info == peer),
            None,
        )
        print(
            f"{LOG} setpoint from {peer.producer.instance_id} copy={copy or '-'} "
            f"positions={list(message.positions)}",
            flush=True,
        )
        await publisher.publish_to(
            peer, joint_states.build_message(time.time(), list(message.positions), True)
        )


async def setup(params: Parameters, node_runner: NodeRunner) -> list:
    print(f"{LOG} up, peers={held_pairs(node_runner)}", flush=True)
    return [
        asyncio.create_task(report_pairs(node_runner, params)),
        asyncio.create_task(answer_setpoints(node_runner)),
    ]


def main() -> None:
    try:
        NodeBuilder().run(setup)
    except Exception:
        traceback.print_exc()
        raise


if __name__ == "__main__":
    main()
