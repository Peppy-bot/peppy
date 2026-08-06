"""Watches the executor side of a `deliberation` pairing without joining it.

An observer claims no endpoint and holds no peer, and the executor is not told
this node exists, so however this node behaves it cannot perturb what it is
recording. It observes across a machine boundary in the split-compute launcher,
and nothing in this file reflects that: what differs is invisible from this side,
because the source's lifecycle transitions arrive as notifications from the
source's own daemon.

It writes no dataset. The samples it counts are the evidence that the chain is
live end to end.
"""

import asyncio

from peppygen import NodeBuilder, NodeRunner
from peppygen.paired_topics.observed_execution import situation
from peppygen.parameters import Parameters


async def record(params: Parameters, node_runner: NodeRunner) -> None:
    """Records every situation the observed executor publishes.

    Subscribing before the source resolves is legal: the subscription is pinned
    to the source instance and stays silent until it emits.
    """
    subscription = await situation.subscribe(node_runner)

    captured = 0
    escalated = 0
    while True:
        received = await subscription.next()
        if received is None:
            break
        source, message = received

        captured += 1
        if message.escalated:
            escalated += 1

        if captured == 1:
            print(
                f"[episode_recorder] observing execution on "
                f"{source.producer.core_node}/{source.producer.instance_id} "
                f"via {source.source_link_id}",
                flush=True,
            )
        elif captured % params.report_every == 0:
            # `active_subgoal_id` is the field that proves the whole chain: it
            # originated on the planner, was adopted by the executor, and is
            # read here by a node that is party to neither side.
            print(
                f"[episode_recorder] captured {captured} samples "
                f"({escalated} escalated), servoing on subgoal "
                f"{message.active_subgoal_id or 'none'}, last positions="
                f"{[round(p, 3) for p in message.joint_positions]}",
                flush=True,
            )


async def setup(params: Parameters, node_runner: NodeRunner) -> list[asyncio.Task]:
    async def announce_shutdown():
        print("[episode_recorder] Shutdown signal received", flush=True)

    node_runner.on_shutdown(announce_shutdown)

    return [asyncio.create_task(record(params, node_runner))]


def main():
    NodeBuilder().run(setup)


if __name__ == "__main__":
    main()
