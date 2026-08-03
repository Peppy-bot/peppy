"""The executor half of a `deliberation` pairing.

Reads a camera and an arm that the launcher places on its own machine, adopts
subgoals from a planner that may be on another one, and reports what it is doing
back up the pair. Nothing here is a controller: the value of this node in the
fixture is that it holds one end of every cross-machine mechanism the federated
tests exercise, and says so on stdout.
"""

import asyncio
import time

from peppygen import NodeBuilder, NodeRunner
from peppygen.consumed_topics.arm import joint_states
from peppygen.consumed_topics.camera import video_stream
from peppygen.paired_topics.deliberation import situation, subgoal
from peppygen.parameters import Parameters

# How often the current situation is pushed up to the planner. Decoupled from
# any control rate: this direction may cross a WAN and only has to keep a ~1 Hz
# planner supplied with recent state.
SITUATION_RATE_HZ = 10

# What `situation.active_subgoal_id` carries when nothing is authoritative. The
# planner numbers its subgoals from 1, so 0 can never collide with a real one.
NO_SUBGOAL = 0


class Situation:
    """Everything the loop knows, shared across this node's tasks.

    Plain state rather than a queue: every reader wants the latest value and
    none of them wants a backlog. A subgoal that has expired is exactly as
    useful as one that never arrived.
    """

    def __init__(self, subgoal_ttl_ms: int) -> None:
        self._subgoal_ttl_s = subgoal_ttl_ms / 1000.0
        self.joint_positions = [0.0, 0.0, 0.0]
        self.frames_seen = 0
        self._subgoal_id = NO_SUBGOAL
        self._adopted_at = None

    def adopt(self, subgoal_id: int) -> None:
        """Takes a subgoal as authoritative, starting its lifetime now.

        Timed on this node's own monotonic clock, never on the planner's
        `stamp`: the two machines do not share a clock, and a lifetime measured
        against a remote timestamp would be wrong by whatever they disagree by.
        """
        self._subgoal_id = subgoal_id
        self._adopted_at = time.monotonic()

    def authoritative_subgoal_id(self) -> int:
        """The subgoal in force right now, or `NO_SUBGOAL` if none is.

        Peppy delivers a dissolution notice when a peer instance dies cleanly,
        but an unreachable daemon cannot send one, so a node whose correctness
        depends on freshness owns the bound itself.
        """
        if self._adopted_at is None:
            return NO_SUBGOAL
        if time.monotonic() - self._adopted_at > self._subgoal_ttl_s:
            return NO_SUBGOAL
        return self._subgoal_id


async def track_arm(node_runner: NodeRunner, state: Situation) -> None:
    subscription = await joint_states.subscribe(node_runner)
    while True:
        received = await subscription.next()
        if received is None:
            break
        _producer, message = received
        state.joint_positions = list(message.positions)


async def track_camera(node_runner: NodeRunner, state: Situation) -> None:
    subscription = await video_stream.subscribe(node_runner)
    while True:
        received = await subscription.next()
        if received is None:
            break
        _producer, message = received
        state.frames_seen += 1
        if state.frames_seen == 1:
            print(
                f"[reactive_policy] first frame from the local camera: "
                f"{message.width}x{message.height} {message.encoding}",
                flush=True,
            )


async def adopt_subgoals(node_runner: NodeRunner, state: Situation) -> None:
    """Takes subgoals from the paired planner.

    Subscribing before the pair exists is legal: the subscription follows the
    slot's live pin and stays silent until a planner is paired.
    """
    subscription = await subgoal.subscribe(node_runner)

    peer = await subgoal.wait_paired(node_runner)
    print(
        f"[reactive_policy] paired with planner "
        f"{peer.producer.core_node}/{peer.producer.instance_id}",
        flush=True,
    )

    while True:
        received = await subscription.next()
        if received is None:
            break
        _producer, message = received
        state.adopt(message.subgoal_id)
        print(
            f"[reactive_policy] adopted subgoal {message.subgoal_id} "
            f"target={[round(p, 3) for p in message.target_position]} "
            f"horizon={message.horizon_s}s",
            flush=True,
        )


async def report_situation(node_runner: NodeRunner, state: Situation) -> None:
    """Publishes what this node is facing, up to the paired planner.

    Runs whether or not anything is escalated, so the planner always has recent
    state rather than only hearing from the robot once it is already stuck.
    """
    publisher = await situation.declare_publisher(node_runner)
    interval = 1.0 / SITUATION_RATE_HZ

    while True:
        # Echoing the adopted subgoal back is what makes adoption observable
        # rather than assumed: the planner learns that the subgoal it sent is
        # the one being acted on, and so does anything observing this role.
        active_subgoal_id = state.authoritative_subgoal_id()
        await publisher.publish(
            situation.build_message(
                # The wall clock, deliberately: `stamp` is for the reader, while
                # the lifetime is timed monotonically inside `Situation`.
                time.time(),
                list(state.joint_positions),
                active_subgoal_id,
                active_subgoal_id == NO_SUBGOAL,
            )
        )
        await asyncio.sleep(interval)


async def setup(params: Parameters, node_runner: NodeRunner) -> list[asyncio.Task]:
    state = Situation(params.subgoal_ttl_ms)
    print(
        f"[reactive_policy] executor up, nominal control rate "
        f"{params.control_rate_hz} Hz, subgoal lifetime {params.subgoal_ttl_ms} ms",
        flush=True,
    )

    async def announce_shutdown():
        print("[reactive_policy] Shutdown signal received", flush=True)

    node_runner.on_shutdown(announce_shutdown)

    return [
        asyncio.create_task(track_arm(node_runner, state)),
        asyncio.create_task(track_camera(node_runner, state)),
        asyncio.create_task(adopt_subgoals(node_runner, state)),
        asyncio.create_task(report_situation(node_runner, state)),
    ]


def main():
    NodeBuilder().run(setup)


if __name__ == "__main__":
    main()
