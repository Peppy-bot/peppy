import asyncio

from peppylib import ActionMessenger, MessengerHandle, SenderTarget
from peppylib.names import generate_name
from peppylib.config import DEFAULT_MESSAGING_PORT, QoSProfile

NODE_NAME = "hello_node"
NODE_TAG = "v1"
ACTION_NAME = "hello_action"

BOLD = "\033[1m"
GREEN = "\033[32m"
YELLOW = "\033[33m"
MAGENTA = "\033[35m"
CYAN = "\033[36m"
WHITE = "\033[37m"
RESET = "\033[0m"

FEEDBACK_TIMEOUT = 5.0
GOAL_TIMEOUT = 3.0
CANCEL_TIMEOUT = 3.0


async def receive_feedback(goal_handle, goal_label: str):
    try:
        message = await asyncio.wait_for(
            goal_handle.on_next_feedback(), timeout=FEEDBACK_TIMEOUT
        )
    except asyncio.TimeoutError:
        print(f"{BOLD}{YELLOW}[FEEDBACK] Timed out waiting for feedback for `{goal_label}`{RESET}")
        return
    except Exception:
        print(f"{BOLD}{YELLOW}[FEEDBACK] Feedback channel closed early for `{goal_label}`{RESET}")
        return

    feedback_text = message.payload.decode("utf-8")
    core_node = message.core_node
    instance_id = message.instance_id
    print(
        f"{BOLD}{YELLOW}[FEEDBACK] Received feedback for `{goal_label}` from `{instance_id}` "
        f"and core node `{core_node}`: `{feedback_text}`{RESET}"
    )


async def main():
    host = "127.0.0.1"
    port = DEFAULT_MESSAGING_PORT

    try:
        sender_handle = await MessengerHandle.from_host_port(host, port)
    except Exception as error:
        raise RuntimeError(
            f"failed to create action messenger on {host}:{port}: {error}.\n"
            "Did you start a zenohd server with the `zenohd_simple` example?"
        )

    core_node = f"{generate_name()}_core"
    instance_id = f"{generate_name()}_listener"

    # --- Send initial goal ---
    print(
        f"{BOLD}{GREEN}[GOAL] Sending goal to `{ACTION_NAME}` action as `{instance_id}` "
        f"and core node `{core_node}`...{RESET}"
    )
    goal_handle = await ActionMessenger.send_goal(
        sender_handle,
        core_node,
        instance_id,
        SenderTarget.node(NODE_NAME, NODE_TAG),
        ACTION_NAME,
        None,  # target_core_node - binds with the first found
        None,  # target_instance_id - binds with the first found
        b"Hello from the action client",
        QoSProfile.Reliable,
        GOAL_TIMEOUT,)

    goal_response = goal_handle.goal_response
    goal_response_text = goal_response.payload.decode("utf-8")
    print(
        f"{BOLD}{GREEN}[GOAL] Received goal response from `{goal_response.instance_id}` "
        f"and core node `{goal_response.core_node}`: `{goal_response_text}`{RESET}"
    )

    await receive_feedback(goal_handle, "initial goal")

    # --- Request result ---
    print(f"{BOLD}{CYAN}[RESULT] Requesting result payload...{RESET}")
    result = await ActionMessenger.request_result(
        sender_handle, goal_handle, GOAL_TIMEOUT
    )
    # `status` is the ResultStatus tag: 0=Completed, 1=Cancelled, 2=Abandoned, 3=Expired.
    result_text = result.body.decode("utf-8")
    print(f"{BOLD}{CYAN}[RESULT] Received status {result.status} result: `{result_text}`{RESET}")

    # --- Send cancellable goal ---
    print("Waiting before sending cancellable goal...")
    await asyncio.sleep(2)

    print(f"{BOLD}{GREEN}[GOAL] Sending cancellable goal...{RESET}")
    goal_handle = await ActionMessenger.send_goal(
        sender_handle,
        core_node,
        instance_id,
        SenderTarget.node(NODE_NAME, NODE_TAG),
        ACTION_NAME,
        None,  # target_core_node
        None,  # target_instance_id
        b"This goal will be cancelled",
        QoSProfile.Reliable,
        GOAL_TIMEOUT,)

    cancel_goal_response_text = goal_handle.goal_response.payload.decode("utf-8")
    print(f"{BOLD}{GREEN}[GOAL] Received goal response: `{cancel_goal_response_text}`{RESET}")

    await receive_feedback(goal_handle, "cancellable goal")

    # --- Cancel the goal ---
    print("Waiting before issuing cancel request...")
    await asyncio.sleep(2)

    cancel_reply = await ActionMessenger.cancel_goal(
        sender_handle, goal_handle, CANCEL_TIMEOUT
    )
    # `state` is the CancelState tag: 0=Signalled, 1=AlreadyTerminal, 2=Unknown.
    print(f"{BOLD}{MAGENTA}[CANCEL] Received cancel state: `{cancel_reply.state}`{RESET}")

    # --- Result after cancellation ---
    # `get_result` still resolves to a definitive typed outcome (the worker here
    # observes the cancel and reports status 1=Cancelled) rather than hanging.
    print(f"{BOLD}{CYAN}[RESULT] Requesting the result after cancellation...{RESET}")
    result = await ActionMessenger.request_result(
        sender_handle, goal_handle, GOAL_TIMEOUT
    )
    result_text = result.body.decode("utf-8")
    print(
        f"{BOLD}{CYAN}[RESULT] Goal resolved with status {result.status}: `{result_text}`{RESET}"
    )

    print(
        f"{BOLD}{WHITE}Action sender finished exercising goal, feedback, result, and cancel flows.{RESET}"
    )


if __name__ == "__main__":
    asyncio.run(main())
