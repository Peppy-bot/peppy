import asyncio

from peppylib import ActionMessenger, MessengerHandle
from peppylib.names import generate_name
from peppylib.config import DEFAULT_MESSAGING_PORT, QoSProfile

NODE_NAME = "hello_node"
ACTION_NAME = "hello_action"

FEEDBACK_TIMEOUT = 5.0
GOAL_TIMEOUT = 3.0
CANCEL_TIMEOUT = 3.0


async def receive_feedback(goal_handle, goal_label: str):
    try:
        message = await asyncio.wait_for(
            goal_handle.on_next_feedback(), timeout=FEEDBACK_TIMEOUT
        )
    except asyncio.TimeoutError:
        print(f"[FEEDBACK] Timed out waiting for feedback for `{goal_label}`")
        return
    except Exception:
        print(f"[FEEDBACK] Feedback channel closed early for `{goal_label}`")
        return

    feedback_text = message.payload.decode("utf-8")
    master_node = message.master_node
    instance_id = message.instance_id
    print(
        f"[FEEDBACK] Received feedback for `{goal_label}` from `{instance_id}` "
        f"and master node `{master_node}`: `{feedback_text}`"
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

    master_node = f"{generate_name()}_master"
    instance_id = f"{generate_name()}_listener"

    # --- Send initial goal ---
    print(
        f"[GOAL] Sending goal to `{ACTION_NAME}` action as `{instance_id}` "
        f"and master node `{master_node}`..."
    )
    # NOTE: ActionMessenger is not yet exposed in the Python bindings.
    goal_handle = await ActionMessenger.send_goal(
        sender_handle,
        master_node,
        instance_id,
        NODE_NAME,
        ACTION_NAME,
        None,  # target_master_node - binds with the first found
        None,  # target_instance_id - binds with the first found
        b"Hello from the action client",
        QoSProfile.Reliable,
        GOAL_TIMEOUT,
    )

    goal_response = goal_handle.goal_response
    goal_response_text = goal_response.payload.decode("utf-8")
    print(
        f"[GOAL] Received goal response from `{goal_response.instance_id}` "
        f"and master node `{goal_response.master_node}`: `{goal_response_text}`"
    )

    await receive_feedback(goal_handle, "initial goal")

    # --- Request result ---
    print("[RESULT] Requesting result payload...")
    result_payload = await ActionMessenger.request_result(
        sender_handle, goal_handle, GOAL_TIMEOUT
    )
    result_text = result_payload.payload.decode("utf-8")
    print(f"[RESULT] Received result: `{result_text}`")

    # --- Send cancellable goal ---
    print("Waiting before sending cancellable goal...")
    await asyncio.sleep(2)

    print("[GOAL] Sending cancellable goal...")
    goal_handle = await ActionMessenger.send_goal(
        sender_handle,
        master_node,
        instance_id,
        NODE_NAME,
        ACTION_NAME,
        None,  # target_master_node
        None,  # target_instance_id
        b"This goal will be cancelled",
        QoSProfile.Reliable,
        GOAL_TIMEOUT,
    )

    cancel_goal_response_text = goal_handle.goal_response.payload.decode("utf-8")
    print(f"[GOAL] Received goal response: `{cancel_goal_response_text}`")

    await receive_feedback(goal_handle, "cancellable goal")

    # --- Cancel the goal ---
    print("Waiting before issuing cancel request...")
    await asyncio.sleep(2)

    cancel_response = await ActionMessenger.cancel_goal(
        sender_handle, goal_handle, CANCEL_TIMEOUT
    )
    cancel_text = cancel_response.payload.decode("utf-8")
    print(f"[CANCEL] Received cancel response: `{cancel_text}`")

    # --- Attempt result after cancellation (should fail) ---
    print("[RESULT] Attempting to request result after cancellation...")
    try:
        result_payload = await ActionMessenger.request_result(
            sender_handle, goal_handle, GOAL_TIMEOUT
        )
        result_text = result_payload.payload.decode("utf-8")
        raise AssertionError(
            f"Received result `{result_text}` even though the goal was cancelled. "
            "The action should stop responding to this goal."
        )
    except Exception as error:
        if "Timeout" in type(error).__name__ or "Unreachable" in type(error).__name__:
            print("[RESULT] No result returned after cancellation, as expected.")
        elif isinstance(error, AssertionError):
            raise
        else:
            raise RuntimeError(
                f"Unexpected error after cancelling goal: {error}"
            ) from error

    print(
        "Action sender finished exercising goal, feedback, result, and cancel flows."
    )


if __name__ == "__main__":
    asyncio.run(main())
