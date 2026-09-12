"""An arm that serves every slot kind and logs who it heard from.

It publishes a heartbeat, answers the `echo` service with the token it was
sent, and drives each `move` goal to completion or through a cancel. Every
line it logs names the instance the request came from, which is what the
copy isolation test reads: a request from another copy's commander would
appear here by name.
"""

import asyncio
import time

from peppygen import NodeBuilder, NodeRunner
from peppygen.emitted_topics import heartbeat
from peppygen.exposed_actions import move
from peppygen.exposed_services import echo
from peppygen.parameters import Parameters

LOG = "[isolation-arm]"


async def publish_heartbeat(node_runner: NodeRunner, params: Parameters) -> None:
    publisher = await heartbeat.declare_publisher(node_runner)
    token = node_runner.cancellation_token()
    interval = 1.0 / params.publish_rate_hz
    seq = 0
    while not token.is_cancelled():
        seq += 1
        await publisher.publish(heartbeat.build_message(seq, time.time()))
        await asyncio.sleep(interval)


async def answer_echo(node_runner: NodeRunner) -> None:
    def handler(request):
        print(
            f"{LOG} echo from {request.instance_id} token={request.data.token}",
            flush=True,
        )
        return echo.Response(token=request.data.token)

    while True:
        await echo.handle_next_request(node_runner, handler)


async def drive(ctx) -> None:
    """Holds one accepted goal until its time is up or a cancel arrives."""
    token = ctx.request().data.token
    hold_s = ctx.request().data.hold_ms / 1000.0
    await ctx.publish_feedback(token)
    cancel = asyncio.ensure_future(ctx.cancel_signal())
    hold = asyncio.ensure_future(asyncio.sleep(hold_s))
    done, pending = await asyncio.wait(
        [cancel, hold], return_when=asyncio.FIRST_COMPLETED
    )
    for task in pending:
        task.cancel()
    if cancel in done:
        await ctx.complete_cancelled(token)
        print(f"{LOG} goal {token} cancelled", flush=True)
    else:
        await ctx.complete(token)
        print(f"{LOG} goal {token} completed", flush=True)


async def serve_moves(node_runner: NodeRunner) -> None:
    action = await move.ActionHandle.expose(node_runner)

    def decide(request):
        print(f"{LOG} goal {request.data.token} from {request.instance_id}", flush=True)
        return move.GoalDecision.accept()

    while True:
        ctx = await action.handle_goal_next_request(decide)
        if ctx is None:
            break
        asyncio.create_task(drive(ctx))


async def setup(params: Parameters, node_runner: NodeRunner) -> list[asyncio.Task]:
    print(f"{LOG} up, heartbeat at {params.publish_rate_hz} Hz", flush=True)
    return [
        asyncio.create_task(publish_heartbeat(node_runner, params)),
        asyncio.create_task(answer_echo(node_runner)),
        asyncio.create_task(serve_moves(node_runner)),
    ]


def main():
    NodeBuilder().run(setup)


if __name__ == "__main__":
    main()
