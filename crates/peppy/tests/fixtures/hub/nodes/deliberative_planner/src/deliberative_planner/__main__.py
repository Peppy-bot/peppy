"""The planner half of a `deliberation` pairing.

Reads a camera and an executor that the split-compute launcher places on another
machine, and publishes a subgoal per planning cycle. It plans nothing: each
subgoal is the executor's last reported position nudged by a fixed step, which
is enough to make the round trip visible from both ends.

The one line worth reading in the output is the first frame it receives, because
that frame crossed a machine boundary through a producer link bound to a camera
instance this node never names a machine for.
"""

import asyncio
import time

from peppygen import NodeBuilder, NodeRunner
from peppygen.consumed_topics.scene import video_stream
from peppygen.paired_topics.deliberation import situation, subgoal
from peppygen.parameters import Parameters

# How far each planning step nudges the executor. Only has to produce subgoals
# that move, and move visibly.
STEP = 0.25


class Scene:
    """The planner's view of the world, written by its two subscriptions."""

    def __init__(self) -> None:
        self.frames_seen = 0
        self.executor_positions = [0.0, 0.0, 0.0]
        self.escalations = 0
        # Which subgoal the executor last reported servoing on. Read from its
        # own echo rather than assumed from what was published: a subgoal that
        # was delivered is not necessarily one that was adopted, and one that
        # was adopted can still lapse on the executor's staleness bound.
        self.adopted_subgoal_id = 0


async def watch_scene(node_runner: NodeRunner, scene: Scene) -> None:
    subscription = await video_stream.subscribe(node_runner)

    while True:
        received = await subscription.next()
        if received is None:
            break
        producer, message = received
        scene.frames_seen += 1
        if scene.frames_seen == 1:
            print(
                f"[deliberative_planner] first frame received across the boundary "
                f"from {producer.core_node}/{producer.instance_id}: "
                f"{message.width}x{message.height} {message.encoding}",
                flush=True,
            )


async def watch_executor(node_runner: NodeRunner, scene: Scene) -> None:
    subscription = await situation.subscribe(node_runner)

    peer = await situation.wait_paired(node_runner)
    print(
        f"[deliberative_planner] paired with executor "
        f"{peer.producer.core_node}/{peer.producer.instance_id}",
        flush=True,
    )

    while True:
        received = await subscription.next()
        if received is None:
            break
        _peer, message = received
        scene.executor_positions = list(message.joint_positions)
        scene.adopted_subgoal_id = message.active_subgoal_id
        if message.escalated:
            scene.escalations += 1


async def plan(
    params: Parameters, node_runner: NodeRunner, scene: Scene
) -> None:
    """Publishes a subgoal per planning cycle.

    Planning starts before the first frame arrives and before the pair is
    established. Publishing on an unpaired slot is a legal no-op, so there is
    nothing to synchronize here: the executor receives every subgoal published
    while the pair is live, and none published before it.
    """
    publisher = await subgoal.declare_publisher(node_runner)
    token = node_runner.cancellation_token()
    interval = 1.0 / params.plan_rate_hz
    print(
        f"[deliberative_planner] planning at {params.plan_rate_hz} Hz "
        f"over a {params.horizon_s}s horizon",
        flush=True,
    )

    # Numbered from 1 so that 0 stays available to the executor as "nothing is
    # authoritative" in `situation.active_subgoal_id`. Consecutive subgoals
    # always carry different ids, which is what lets a reader tell a newly
    # adopted subgoal from a redelivery of the previous one.
    subgoal_id = 0
    while not token.is_cancelled():
        subgoal_id += 1
        target = [round(position + STEP, 3) for position in scene.executor_positions]
        await publisher.publish(
            subgoal.build_message(time.time(), subgoal_id, target, params.horizon_s)
        )
        print(
            f"[deliberative_planner] subgoal {subgoal_id} target={target} "
            f"(frames={scene.frames_seen}, escalations={scene.escalations}, "
            f"executor_on={scene.adopted_subgoal_id or 'none'})",
            flush=True,
        )
        await asyncio.sleep(interval)


async def setup(params: Parameters, node_runner: NodeRunner) -> list[asyncio.Task]:
    scene = Scene()

    async def announce_shutdown():
        print("[deliberative_planner] Shutdown signal received", flush=True)

    node_runner.on_shutdown(announce_shutdown)

    return [
        asyncio.create_task(watch_scene(node_runner, scene)),
        asyncio.create_task(watch_executor(node_runner, scene)),
        asyncio.create_task(plan(params, node_runner, scene)),
    ]


def main():
    NodeBuilder().run(setup)


if __name__ == "__main__":
    main()
