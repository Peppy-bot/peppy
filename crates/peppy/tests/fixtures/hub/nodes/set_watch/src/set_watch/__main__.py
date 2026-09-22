"""Watches the arms and leaders the copies of a fleet bring.

One slot binds every arm's joint_states and one observes every leader's
setpoints on its pair with the hub, both `zero_or_more` and both empty until a
copy joins. The watch logs both member sets whenever either changes, in the
order the copies joined, and the first message from each member, tagged with
the member that sent it. A member that leaves is forgotten, so a copy that
rejoins is heard again. The cross-daemon set test reads these lines.
"""

import asyncio
import traceback

from peppygen import NodeBuilder, NodeRunner
from peppygen.consumed_topics.arms import joint_states
from peppygen.paired_topics.leaders import joint_setpoints
from peppygen.parameters import Parameters

LOG = "[set-watch]"


def members(node_runner: NodeRunner) -> tuple[list[str], list[str]]:
    """Both slots' members, by instance, in the order the slots hold them."""
    arms = [producer.instance_id for producer in joint_states.bound_producers(node_runner)]
    leaders = [source.producer.instance_id for source in joint_setpoints.sources(node_runner)]
    return arms, leaders


class Heard:
    """The members a message has been logged from, while they stay members."""

    def __init__(self) -> None:
        self.names: set[str] = set()

    def first(self, name: str) -> bool:
        if name in self.names:
            return False
        self.names.add(name)
        return True

    def keep(self, current: list[str]) -> None:
        self.names &= set(current)


async def report_members(node_runner: NodeRunner, params: Parameters, heard: Heard) -> None:
    token = node_runner.cancellation_token()
    interval = 1.0 / params.report_rate_hz
    last = None
    while not token.is_cancelled():
        arms, leaders = members(node_runner)
        heard.keep(arms + leaders)
        line = f"members arms=[{','.join(arms)}] leaders=[{','.join(leaders)}]"
        if line != last:
            print(f"{LOG} {line}", flush=True)
            last = line
        await asyncio.sleep(interval)


async def log_first_messages(subscription, name_of, heard: Heard) -> None:
    while True:
        received = await subscription.next()
        if received is None:
            return
        sender, _message = received
        name = name_of(sender)
        if heard.first(name):
            print(f"{LOG} received from {name}", flush=True)


async def setup(params: Parameters, node_runner: NodeRunner) -> list:
    heard = Heard()
    arms = await joint_states.subscribe(node_runner)
    leaders = await joint_setpoints.subscribe(node_runner)
    return [
        asyncio.create_task(report_members(node_runner, params, heard)),
        asyncio.create_task(
            log_first_messages(arms, lambda producer: producer.instance_id, heard)
        ),
        asyncio.create_task(
            log_first_messages(leaders, lambda source: source.producer.instance_id, heard)
        ),
    ]


def main() -> None:
    try:
        NodeBuilder().run(setup)
    except Exception:
        traceback.print_exc()
        raise


if __name__ == "__main__":
    main()
