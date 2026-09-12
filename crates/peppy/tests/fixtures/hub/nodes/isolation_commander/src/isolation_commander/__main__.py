"""The commander of one copy, exercising its arm over every slot kind.

Round after round it polls the arm's `echo` service, drives one `move` goal
to completion and cancels another, all with tokens carrying this copy's
label, while a subscription follows the arm's heartbeat. Every line it logs
names the instance that answered, which is what the copy isolation test
reads: an answer from another copy's arm would appear here by name.
"""

import asyncio

from peppygen import NodeBuilder, NodeRunner, QoSProfile
from peppygen.consumed_actions.arm import move
from peppygen.consumed_services.arm import echo
from peppygen.consumed_topics.arm import heartbeat
from peppygen.parameters import Parameters

LOG = "[isolation-commander]"
# The goal that runs to completion holds briefly; the one the commander
# cancels would hold long enough that the cancel always lands on a live goal.
COMPLETED_GOAL_HOLD_MS = 100
CANCELLED_GOAL_HOLD_MS = 10_000
REQUEST_TIMEOUT_S = 5.0
RESULT_TIMEOUT_S = 15.0


async def track_heartbeat(node_runner: NodeRunner) -> None:
    subscription = await heartbeat.subscribe(node_runner)
    while True:
        received = await subscription.next()
        if received is None:
            break
        producer, message = received
        print(
            f"{LOG} heartbeat from {producer.instance_id} seq={message.seq}", flush=True
        )


async def run_goal(
    node_runner: NodeRunner, token: str, hold_ms: int, cancel: bool
) -> None:
    arm = move.bound_producer(node_runner)
    handle = await move.ActionHandle.fire_goal(
        node_runner,
        arm,
        move.GoalRequest(token=token, hold_ms=hold_ms),
        REQUEST_TIMEOUT_S,
        QoSProfile.SensorData,
    )
    if not handle.accepted:
        print(f"{LOG} goal {token} rejected: {handle.reason}", flush=True)
        return
    try:
        feedback = await handle.on_next_feedback_message()
        print(f"{LOG} feedback token={feedback.token}", flush=True)
    except Exception:
        # The goal finished before its feedback was read; the result says so.
        pass
    if cancel:
        cancel_response = await handle.cancel_goal(REQUEST_TIMEOUT_S)
        print(f"{LOG} cancel {token} {cancel_response.state.name}", flush=True)
    result = await handle.get_result(RESULT_TIMEOUT_S)
    answered = result.data.token if result.data is not None else "-"
    print(
        f"{LOG} goal {token} {result.status.name} by {result.instance_id} token={answered}",
        flush=True,
    )


async def exercise_arm(node_runner: NodeRunner, params: Parameters) -> None:
    arm = echo.bound_producer(node_runner)
    interval = params.round_interval_ms / 1000.0
    round_number = 0
    while True:
        round_number += 1
        token = f"{params.label}-{round_number}"
        response = await echo.poll(
            node_runner, arm, echo.Request(token=token), REQUEST_TIMEOUT_S
        )
        print(
            f"{LOG} echo answered by {response.instance_id} token={response.data.token}",
            flush=True,
        )
        await run_goal(node_runner, token, COMPLETED_GOAL_HOLD_MS, cancel=False)
        await run_goal(node_runner, f"{token}c", CANCELLED_GOAL_HOLD_MS, cancel=True)
        await asyncio.sleep(interval)


async def setup(params: Parameters, node_runner: NodeRunner) -> list[asyncio.Task]:
    print(f"{LOG} up, label {params.label}", flush=True)
    return [
        asyncio.create_task(track_heartbeat(node_runner)),
        asyncio.create_task(exercise_arm(node_runner, params)),
    ]


def main():
    NodeBuilder().run(setup)


if __name__ == "__main__":
    main()
