import asyncio
import enum
import signal
from datetime import datetime

from peppylib import ActionMessenger, MessengerHandle
from peppylib.names import generate_name
from peppylib.config import DEFAULT_MESSAGING_PORT

NODE_NAME = "hello_node"
ACTION_NAME = "hello_action"

BOLD = "\033[1m"
GREEN = "\033[32m"
YELLOW = "\033[33m"
MAGENTA = "\033[35m"
CYAN = "\033[36m"
WHITE = "\033[37m"
RED = "\033[31m"
RESET = "\033[0m"


class LoopState(enum.Enum):
    WAIT_FOR_GOAL = "wait_for_goal"
    WAIT_FOR_FOLLOWUPS = "wait_for_followups"
    SHUTDOWN = "shutdown"


def current_timestamp() -> str:
    return datetime.now().strftime("%Y-%m-%d %H:%M:%S")


async def handle_goal_request(request, feedback_publisher) -> bytes:
    request_id = request.request_id
    daemon_node = request.message.daemon_node
    instance_id = request.message.instance_id
    payload_text = request.message.payload.decode("utf-8")

    print(
        f"{BOLD}{GREEN}[GOAL] [{current_timestamp()}] Received goal `{request_id}` "
        f"from `{instance_id}` and daemon node `{daemon_node}`{RESET}"
    )

    feedback_text = f"feedback: working on `{payload_text}`"
    await feedback_publisher.publish(feedback_text.encode("utf-8"))

    print(
        f"{BOLD}{YELLOW}[FEEDBACK] [{current_timestamp()}] Published feedback "
        f"`{feedback_text}` for goal `{request_id}`{RESET}"
    )

    response_text = f"goal accepted: {payload_text}"
    print(
        f"{BOLD}{GREEN}[GOAL] [{current_timestamp()}] Responding to goal "
        f"`{request_id}` with `{response_text}`{RESET}"
    )

    return response_text.encode("utf-8")


async def handle_cancel_request(request) -> bytes:
    request_id = request.request_id
    print(
        f"{BOLD}{MAGENTA}[CANCEL] [{current_timestamp()}] Received cancel request "
        f"for goal `{request_id}`{RESET}"
    )

    payload = request.message.payload
    if payload:
        payload_text = payload.decode("utf-8")
        print(
            f"{BOLD}{MAGENTA}[CANCEL] [{current_timestamp()}] Cancel payload "
            f"`{payload_text}` will be ignored.{RESET}"
        )

    response_text = f"cancel acknowledged for goal `{request_id}`"
    print(
        f"{BOLD}{MAGENTA}[CANCEL] [{current_timestamp()}] Responding to cancel request "
        f"with `{response_text}`{RESET}"
    )

    return response_text.encode("utf-8")


async def handle_result_request(request) -> bytes:
    request_id = request.request_id
    instance_id = request.message.instance_id
    payload_text = request.message.payload.decode("utf-8")

    print(
        f"{BOLD}{CYAN}[RESULT] [{current_timestamp()}] Received result request "
        f"`{request_id}` from `{instance_id}` with payload `{payload_text}`{RESET}"
    )

    response_text = "SUCCESS!"
    print(
        f"{BOLD}{CYAN}[RESULT] [{current_timestamp()}] Responding to result request "
        f"`{request_id}` with `{response_text}`{RESET}"
    )

    return response_text.encode("utf-8")


async def wait_for_goal(action, active_caller_instance: dict, stop_event: asyncio.Event) -> LoopState:
    async def goal_handler(request):
        active_caller_instance["value"] = request.message.instance_id
        return await handle_goal_request(request, action.feedback_publisher)

    goal_task = asyncio.ensure_future(
        action.goal_service.handle_next_request(goal_handler)
    )
    stop_task = asyncio.ensure_future(stop_event.wait())

    done, pending = await asyncio.wait(
        [goal_task, stop_task], return_when=asyncio.FIRST_COMPLETED
    )
    for task in pending:
        task.cancel()

    if stop_event.is_set():
        print(f"{BOLD}{WHITE}[ACTION] Received CTRL+C, exiting.{RESET}")
        return LoopState.SHUTDOWN

    result = goal_task.result()
    if result is True:
        return LoopState.WAIT_FOR_FOLLOWUPS
    elif result is False:
        print(f"{BOLD}{WHITE}[ACTION] Goal listener closed by client.{RESET}")
        return LoopState.SHUTDOWN
    else:
        print(f"{BOLD}{RED}[ERROR] Failed to handle goal request: {result}{RESET}")
        return LoopState.SHUTDOWN


async def handle_followups(action, active_caller_instance: dict, stop_event: asyncio.Event) -> LoopState:
    while True:
        async def cancel_handler(request):
            caller_instance = request.message.instance_id
            if active_caller_instance.get("value") != caller_instance:
                print(f"{BOLD}{MAGENTA}[CANCEL] Ignoring cancel request for inactive goal.{RESET}")
                return b"cancel ignored: no active goal for caller"
            response = await handle_cancel_request(request)
            active_caller_instance["value"] = None
            return response

        async def result_handler(request):
            caller_instance = request.message.instance_id
            if active_caller_instance.get("value") != caller_instance:
                print(f"{BOLD}{CYAN}[RESULT] Ignoring result request for inactive goal.{RESET}")
                return b"result ignored: no active goal for caller"
            response = await handle_result_request(request)
            active_caller_instance["value"] = None
            return response

        cancel_task = asyncio.ensure_future(
            action.cancel_service.handle_next_request(cancel_handler)
        )
        result_task = asyncio.ensure_future(
            action.result_service.handle_next_request(result_handler)
        )
        stop_task = asyncio.ensure_future(stop_event.wait())

        done, pending = await asyncio.wait(
            [cancel_task, result_task, stop_task],
            return_when=asyncio.FIRST_COMPLETED,
        )
        for task in pending:
            task.cancel()

        if stop_event.is_set():
            print(f"{BOLD}{WHITE}[ACTION] Received CTRL+C, exiting.{RESET}")
            return LoopState.SHUTDOWN

        for task in done:
            try:
                outcome = task.result()
            except Exception as error:
                print(f"{BOLD}{RED}[ERROR] Failed to handle request: {error}{RESET}")
                return LoopState.SHUTDOWN

            if outcome is False:
                label = "Cancel" if task is cancel_task else "Result"
                print(f"{BOLD}{WHITE}[ACTION] {label} listener closed by client.{RESET}")
                return LoopState.SHUTDOWN

        if active_caller_instance.get("value") is None:
            return LoopState.WAIT_FOR_GOAL


async def run_action_loop(action, stop_event: asyncio.Event):
    active_caller_instance = {"value": None}
    state = LoopState.WAIT_FOR_GOAL

    while state != LoopState.SHUTDOWN:
        if state == LoopState.WAIT_FOR_GOAL:
            state = await wait_for_goal(action, active_caller_instance, stop_event)
        elif state == LoopState.WAIT_FOR_FOLLOWUPS:
            state = await handle_followups(action, active_caller_instance, stop_event)


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

    daemon_node = f"{generate_name()}_daemon"
    instance_id = f"{generate_name()}_listener"

    action = await ActionMessenger.expose(
        receiver_handle,
        daemon_node,
        instance_id,
        NODE_NAME,
        ACTION_NAME,
    )

    print(
        f"{BOLD}{WHITE}[ACTION] Waiting for action goals as `{instance_id}` "
        f"and daemon node `{daemon_node}`... Press CTRL+C to stop.{RESET}"
    )

    loop = asyncio.get_running_loop()
    stop_event = asyncio.Event()
    loop.add_signal_handler(signal.SIGINT, stop_event.set)

    await run_action_loop(action, stop_event)

    print(f"{BOLD}{WHITE}[ACTION] Action receiver shutting down.{RESET}")


if __name__ == "__main__":
    asyncio.run(main())
