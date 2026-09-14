"""An arm that serves every slot kind and logs who it heard from.

It publishes a heartbeat, answers the `echo` service with the token it was
sent, and drives each `move` goal to completion or through a cancel: a
timed goal completes when its hold is up, a gated one when `release` names
its token. Every line it logs names the instance the request came from and
the instance that handled it, which is what the copy isolation test reads:
a request from another copy's commander would appear here by name.
"""

import asyncio
import time
import traceback

from peppygen import NodeBuilder, NodeRunner
from peppygen.emitted_topics import heartbeat
from peppygen.exposed_actions import move
from peppygen.exposed_services import echo, release
from peppygen.parameters import Parameters

LOG = "[isolation-arm]"


class Releases:
    """The gated goals waiting on `release`, by token.

    A goal registers before its feedback goes out, so the release that its
    feedback provokes always finds it; whichever of the release and the
    goal's end comes first takes the token out.
    """

    def __init__(self) -> None:
        self._held: dict[str, asyncio.Event] = {}

    def register(self, token: str) -> asyncio.Event:
        if token in self._held:
            raise RuntimeError(f"goal {token} is already held")
        held = asyncio.Event()
        self._held[token] = held
        return held

    def take(self, token: str) -> asyncio.Event | None:
        return self._held.pop(token, None)


async def publish_heartbeat(node_runner: NodeRunner, params: Parameters) -> None:
    publisher = await heartbeat.declare_publisher(node_runner)
    token = node_runner.cancellation_token()
    interval = 1.0 / params.publish_rate_hz
    seq = 0
    while not token.is_cancelled():
        seq += 1
        await publisher.publish(heartbeat.build_message(seq, time.time()))
        await asyncio.sleep(interval)


async def answer_echo(node_runner: NodeRunner, me: str) -> None:
    def handler(request):
        print(
            f"{LOG} echo from {request.instance_id} token={request.data.token} "
            f"answered by {me}",
            flush=True,
        )
        return echo.Response(token=request.data.token)

    while True:
        await echo.handle_next_request(node_runner, handler)


async def answer_release(node_runner: NodeRunner, releases: Releases, me: str) -> None:
    def handler(request):
        token = request.data.token
        held = releases.take(token)
        if held is None:
            raise RuntimeError(
                f"release from {request.instance_id} names {token}, which {me} does not hold"
            )
        print(f"{LOG} release {token} from {request.instance_id} to {me}", flush=True)
        held.set()
        return release.Response(token=token)

    while True:
        await release.handle_next_request(node_runner, handler)


async def drive(ctx, releases: Releases, me: str) -> None:
    """Holds one accepted goal until its release, its time is up, or a cancel arrives."""
    request = ctx.request().data
    token = request.token
    if request.gated:
        hold = asyncio.ensure_future(releases.register(token).wait())
    else:
        hold = asyncio.ensure_future(asyncio.sleep(request.hold_ms / 1000.0))
    await ctx.publish_feedback(token)
    cancel = asyncio.ensure_future(ctx.cancel_signal())
    done, pending = await asyncio.wait(
        [cancel, hold], return_when=asyncio.FIRST_COMPLETED
    )
    for task in pending:
        task.cancel()
    await asyncio.gather(*pending, return_exceptions=True)
    # A hold that is up has already earned its completion, whatever else
    # landed in the same wait.
    if hold in done:
        await ctx.complete(token)
        print(f"{LOG} goal {token} completed by {me}", flush=True)
    else:
        releases.take(token)
        await ctx.complete_cancelled(token)
        print(f"{LOG} goal {token} cancelled by {me}", flush=True)


async def serve_moves(node_runner: NodeRunner, releases: Releases, me: str) -> None:
    action = await move.ActionHandle.expose(node_runner)
    # Each goal runs in its own task, held here so the loop can accept the
    # next one and so a task that raises takes the node down with it.
    driving: set[asyncio.Task] = set()

    def decide(request):
        print(
            f"{LOG} goal {request.data.token} from {request.instance_id} accepted by {me}",
            flush=True,
        )
        return move.GoalDecision.accept()

    def finished(task: asyncio.Task) -> None:
        driving.discard(task)
        error = None if task.cancelled() else task.exception()
        if error is not None:
            print(
                f"{LOG} a goal failed:\n{''.join(traceback.format_exception(error))}",
                flush=True,
            )
            node_runner.cancellation_token().cancel()

    while True:
        ctx = await action.handle_goal_next_request(decide)
        if ctx is None:
            break
        task = asyncio.create_task(drive(ctx, releases, me))
        driving.add(task)
        task.add_done_callback(finished)


async def setup(params: Parameters, node_runner: NodeRunner) -> list[asyncio.Task]:
    me = node_runner.bound_instance_id()
    print(f"{LOG} up as {me}, heartbeat at {params.publish_rate_hz} Hz", flush=True)
    releases = Releases()
    return [
        asyncio.create_task(publish_heartbeat(node_runner, params)),
        asyncio.create_task(answer_echo(node_runner, me)),
        asyncio.create_task(answer_release(node_runner, releases, me)),
        asyncio.create_task(serve_moves(node_runner, releases, me)),
    ]


def main():
    NodeBuilder().run(setup)


if __name__ == "__main__":
    main()
