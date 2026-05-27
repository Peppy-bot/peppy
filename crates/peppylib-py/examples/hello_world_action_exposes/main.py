"""Concurrent action server example.

Exposes a single action and serves many goals at once: the accept loop
registers each goal, replies "accepted", and hands the goal's GoalContext to an
independent worker coroutine before going back to accept the next goal. Each
worker streams progress feedback on its own goal's stream while watching that
goal's cancel signal, then delivers a result. Because every goal owns a separate
context, two clients fire goals that progress in parallel, and cancelling one
goal never disturbs another.

Pair this with the `hello_world_action_subscribes` example and a `zenohd`
router.
"""

import asyncio
import signal
from datetime import datetime

from peppylib import ActionMessenger, MessengerHandle, SenderTarget
from peppylib.names import generate_name
from peppylib.config import DEFAULT_MESSAGING_PORT

NODE_NAME = "hello_node"
NODE_TAG = "v1"
ACTION_NAME = "hello_action"

# How long each simulated work step takes between feedback messages.
STEP_DELAY_SECS = 0.4

BOLD = "\033[1m"
GREEN = "\033[32m"
YELLOW = "\033[33m"
MAGENTA = "\033[35m"
CYAN = "\033[36m"
WHITE = "\033[37m"
RED = "\033[31m"
RESET = "\033[0m"


def current_timestamp() -> str:
    return datetime.now().strftime("%Y-%m-%d %H:%M:%S")


async def run_goal(ctx):
    """Per-goal worker.

    Streams progress feedback for one goal while racing the goal's cancel
    signal, then delivers the result. The goal's payload is the resource (e.g.
    a device) this goal targets; the framework has already stripped the wire
    envelope, so `request_bytes` is the user payload.
    """
    target = bytes(ctx.request_bytes).decode("utf-8")
    print(
        f"{BOLD}{GREEN}[GOAL] [{current_timestamp()}] accepted goal "
        f"`{ctx.goal_id}` for `{target}`{RESET}"
    )

    cancelled = asyncio.ensure_future(ctx.cancel_signal())
    try:
        for percent in range(20, 101, 20):
            step = asyncio.ensure_future(asyncio.sleep(STEP_DELAY_SECS))
            done, _ = await asyncio.wait(
                [cancelled, step], return_when=asyncio.FIRST_COMPLETED
            )

            if cancelled in done:
                step.cancel()
                print(
                    f"{BOLD}{MAGENTA}[CANCEL] [{current_timestamp()}] `{target}` "
                    f"cancelled at {percent}%; finishing early{RESET}"
                )
                # The worker decides how to react; here it completes with a
                # cancelled result. `complete` also closes the feedback stream.
                await ctx.complete(f"`{target}` cancelled at {percent}%".encode("utf-8"))
                return

            line = f"`{target}` progress {percent}%"
            await ctx.publish_feedback(line.encode("utf-8"))
            print(f"{BOLD}{YELLOW}[FEEDBACK] [{current_timestamp()}] {line}{RESET}")

        await ctx.complete(f"`{target}` complete".encode("utf-8"))
        print(f"{BOLD}{CYAN}[RESULT] [{current_timestamp()}] `{target}` complete{RESET}")
    finally:
        cancelled.cancel()


async def run_action_server(server, stop_event: asyncio.Event):
    """Accept-and-spawn loop.

    Accepts the next goal, registers its context (so a fast follow-up
    cancel/result cannot miss it) and replies "accepted", then spawns an
    independent worker. The loop returns to accepting immediately, so a second
    goal never waits behind the first.
    """
    workers: list[asyncio.Task] = []
    while not stop_event.is_set():
        recv_task = asyncio.ensure_future(server.recv_next_goal())
        stop_task = asyncio.ensure_future(stop_event.wait())
        done, pending = await asyncio.wait(
            [recv_task, stop_task], return_when=asyncio.FIRST_COMPLETED
        )
        for task in pending:
            task.cancel()

        if stop_event.is_set():
            print(f"{BOLD}{WHITE}[ACTION] Received CTRL+C, exiting.{RESET}")
            break

        goal_request = recv_task.result()
        if goal_request is None:
            print(f"{BOLD}{WHITE}[ACTION] Goal service closed.{RESET}")
            break

        # accept registers the goal and replies; the returned context is owned
        # by an independent worker coroutine.
        ctx = await goal_request.accept(b"accepted")
        workers.append(asyncio.ensure_future(run_goal(ctx)))

    for worker in workers:
        worker.cancel()


async def main():
    host = "127.0.0.1"
    port = DEFAULT_MESSAGING_PORT

    try:
        receiver_handle = await MessengerHandle.from_host_port(host, port)
    except Exception as error:
        raise RuntimeError(
            f"failed to create action messenger on {host}:{port}: {error}.\n"
            "Did you start a zenohd server with the `zenohd_simple` example?"
        )

    core_node = f"{generate_name()}_core"
    instance_id = f"{generate_name()}_listener"

    server = await ActionMessenger.expose(
        receiver_handle,
        core_node,
        instance_id,
        SenderTarget.node(NODE_NAME, NODE_TAG),
        ACTION_NAME,
    )

    print(
        f"{BOLD}{WHITE}[ACTION] Serving concurrent goals as `{instance_id}` "
        f"and core node `{core_node}`... Press CTRL+C to stop.{RESET}"
    )

    loop = asyncio.get_running_loop()
    stop_event = asyncio.Event()
    loop.add_signal_handler(signal.SIGINT, stop_event.set)

    await run_action_server(server, stop_event)

    print(f"{BOLD}{WHITE}[ACTION] Action receiver shutting down.{RESET}")


if __name__ == "__main__":
    asyncio.run(main())
