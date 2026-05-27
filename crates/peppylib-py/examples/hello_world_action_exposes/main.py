import asyncio
import signal
from datetime import datetime

from peppylib import ConcurrentAction, MessengerHandle, SenderTarget
from peppylib.names import generate_name
from peppylib.config import DEFAULT_MESSAGING_PORT

NODE_NAME = "hello_node"
NODE_TAG = "v1"
ACTION_NAME = "hello_action"

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


async def drive_goal(ctx):
    """Drive a single accepted goal to completion.

    Spawned once per goal so many goals make progress concurrently — each owns
    its own GoalContext, so its feedback, cancel signal, and result never cross
    another goal's streams.
    """
    request_text = bytes(ctx.request_bytes).decode("utf-8")
    goal_id = ctx.goal_id

    print(
        f"{BOLD}{GREEN}[GOAL] [{current_timestamp()}] Accepted goal `{goal_id}` "
        f"for `{request_text}`{RESET}"
    )

    # Feedback goes through this goal's context, not a shared slot.
    feedback_text = f"working on `{request_text}`"
    await ctx.publish_feedback(feedback_text.encode("utf-8"))
    print(
        f"{BOLD}{YELLOW}[FEEDBACK] [{current_timestamp()}] Published `{feedback_text}` "
        f"for goal `{goal_id}`{RESET}"
    )

    # Simulate long-running work that can be cancelled mid-flight.
    cancel_task = asyncio.ensure_future(ctx.cancel_signal())
    work_task = asyncio.ensure_future(asyncio.sleep(2))
    done, pending = await asyncio.wait(
        [cancel_task, work_task], return_when=asyncio.FIRST_COMPLETED
    )
    for task in pending:
        task.cancel()

    if cancel_task in done:
        print(
            f"{BOLD}{MAGENTA}[CANCEL] [{current_timestamp()}] Goal `{goal_id}` cancelled{RESET}"
        )
        await ctx.complete_cancelled(b"CANCELLED")
    else:
        result_text = f"SUCCESS: {request_text}"
        await ctx.complete(result_text.encode("utf-8"))
        print(
            f"{BOLD}{CYAN}[RESULT] [{current_timestamp()}] Goal `{goal_id}` completed{RESET}"
        )


async def run_action_loop(action, stop_event: asyncio.Event):
    """Accept goals forever, spawning a worker per goal.

    The loop only waits for the next goal — cancel and result requests are
    routed to the right goal by the engine, so a slow goal never blocks
    accepting new ones.
    """
    goal_tasks: set[asyncio.Task] = set()

    while True:
        recv_task = asyncio.ensure_future(action.recv_next_goal())
        stop_task = asyncio.ensure_future(stop_event.wait())
        done, pending = await asyncio.wait(
            [recv_task, stop_task], return_when=asyncio.FIRST_COMPLETED
        )
        for task in pending:
            task.cancel()

        if stop_event.is_set():
            print(f"{BOLD}{WHITE}[ACTION] Received CTRL+C, exiting.{RESET}")
            return

        pending_goal = recv_task.result()
        if pending_goal is None:
            print(f"{BOLD}{WHITE}[ACTION] Goal listener closed by client.{RESET}")
            return

        ctx = await pending_goal.accept(b"goal accepted")
        task = asyncio.create_task(drive_goal(ctx))
        goal_tasks.add(task)
        task.add_done_callback(goal_tasks.discard)


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

    action = await ConcurrentAction.expose(
        receiver_handle,
        core_node,
        instance_id,
        SenderTarget.node(NODE_NAME, NODE_TAG),
        ACTION_NAME,
        True,  # this action publishes feedback
    )

    print(
        f"{BOLD}{WHITE}[ACTION] Waiting for action goals as `{instance_id}` "
        f"and core node `{core_node}`... Press CTRL+C to stop.{RESET}"
    )

    loop = asyncio.get_running_loop()
    stop_event = asyncio.Event()
    loop.add_signal_handler(signal.SIGINT, stop_event.set)

    await run_action_loop(action, stop_event)

    print(f"{BOLD}{WHITE}[ACTION] Action receiver shutting down.{RESET}")


if __name__ == "__main__":
    asyncio.run(main())
