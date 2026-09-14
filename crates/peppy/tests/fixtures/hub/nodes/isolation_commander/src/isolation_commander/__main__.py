"""The commander of one copy, exercising its arm over every slot kind.

Round after round it polls the arm's `echo` service, drives one `move` goal
to completion, cancels another, and then holds a gated one until the gate
moves it on through `move_on`: `cancel` cancels it, `release` asks the arm
to complete it. Every token carries this copy's label, and a subscription
follows the arm's heartbeat. Every line it logs names the instance that
answered, which is what the copy isolation test reads: an answer from
another copy's arm would appear here by name.
"""

import asyncio
from dataclasses import dataclass

from peppygen import NodeBuilder, NodeRunner, QoSProfile
from peppygen.consumed_actions.arm import move
from peppygen.consumed_services.arm import echo, release
from peppygen.consumed_topics.arm import heartbeat
from peppygen.exposed_services import move_on
from peppygen.parameters import Parameters

LOG = "[isolation-commander]"
# The goal that runs to completion holds briefly. Every other goal is gated,
# which leaves its cancel or its release the only thing that can end it; the
# arm reads `gated` alone, so a gated hold carries no time.
COMPLETED_GOAL_HOLD_MS = 100
GATED_GOAL_HOLD_MS = 0
REQUEST_TIMEOUT_S = 5.0
RESULT_TIMEOUT_S = 15.0
# Ceiling on a held goal's wait for the gate; reaching it fails the round.
HELD_RESULT_TIMEOUT_S = 600.0


@dataclass
class Held:
    """The gated goal of the current round, until the gate moves it on."""

    token: str
    handle: move.ActionHandle


@dataclass
class CurrentlyHeld:
    """The goal the `move_on` handler moves on, while a round holds one."""

    goal: Held | None = None


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


async def fire(
    node_runner: NodeRunner, token: str, hold_ms: int, gated: bool
) -> move.ActionHandle:
    """Fires one goal and waits for its first feedback, so the arm holds it."""
    arm = move.bound_producer(node_runner)
    handle = await move.ActionHandle.fire_goal(
        node_runner,
        arm,
        move.GoalRequest(token=token, hold_ms=hold_ms, gated=gated),
        REQUEST_TIMEOUT_S,
        QoSProfile.SensorData,
    )
    if not handle.accepted:
        raise RuntimeError(f"goal {token} rejected by {arm.instance_id}: {handle.reason}")
    feedback = await handle.on_next_feedback_message()
    print(
        f"{LOG} feedback token={feedback.token} from {arm.instance_id}",
        flush=True,
    )
    return handle


async def report(handle: move.ActionHandle, token: str, timeout_s: float) -> None:
    result = await handle.get_result(timeout_s)
    answered = result.data.token if result.data is not None else "-"
    print(
        f"{LOG} goal {token} {result.status.name} by {result.instance_id} token={answered}",
        flush=True,
    )


async def cancel(handle: move.ActionHandle, token: str) -> None:
    response = await handle.cancel_goal(REQUEST_TIMEOUT_S)
    print(
        f"{LOG} cancel {token} {response.state.name} by {response.instance_id}",
        flush=True,
    )


async def run_timed_goal(node_runner: NodeRunner, token: str) -> None:
    handle = await fire(node_runner, token, COMPLETED_GOAL_HOLD_MS, gated=False)
    await report(handle, token, RESULT_TIMEOUT_S)


async def run_cancelled_goal(node_runner: NodeRunner, token: str) -> None:
    handle = await fire(node_runner, token, GATED_GOAL_HOLD_MS, gated=True)
    await cancel(handle, token)
    await report(handle, token, RESULT_TIMEOUT_S)


async def run_held_goal(node_runner: NodeRunner, token: str, held: CurrentlyHeld) -> None:
    handle = await fire(node_runner, token, GATED_GOAL_HOLD_MS, gated=True)
    held.goal = Held(token, handle)
    print(f"{LOG} holding {token}", flush=True)
    await report(handle, token, HELD_RESULT_TIMEOUT_S)
    held.goal = None


async def answer_move_on(node_runner: NodeRunner, held: CurrentlyHeld) -> None:
    async def handler(request):
        verb = request.data.verb
        current = held.goal
        if current is None:
            refusal = f"move_on {verb} from {request.instance_id}: nothing held"
            print(f"{LOG} {refusal}", flush=True)
            raise RuntimeError(refusal)
        print(
            f"{LOG} move_on {verb} from {request.instance_id} for {current.token}",
            flush=True,
        )
        if verb == "cancel":
            await cancel(current.handle, current.token)
        elif verb == "release":
            arm = release.bound_producer(node_runner)
            response = await release.poll(
                node_runner,
                arm,
                release.Request(token=current.token),
                REQUEST_TIMEOUT_S,
            )
            print(
                f"{LOG} release {current.token} relayed to {response.instance_id}",
                flush=True,
            )
        else:
            refusal = f"move_on verb {verb!r} from {request.instance_id}"
            print(f"{LOG} {refusal}", flush=True)
            raise RuntimeError(refusal)
        return move_on.Response(token=current.token)

    while True:
        await move_on.handle_next_request(node_runner, handler)


async def exercise_arm(
    node_runner: NodeRunner, params: Parameters, held: CurrentlyHeld
) -> None:
    arm = echo.bound_producer(node_runner)
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
        await run_timed_goal(node_runner, token)
        await run_cancelled_goal(node_runner, f"{token}c")
        await run_held_goal(node_runner, f"{token}h", held)


async def setup(params: Parameters, node_runner: NodeRunner) -> list[asyncio.Task]:
    print(
        f"{LOG} up as {node_runner.bound_instance_id()}, label {params.label}",
        flush=True,
    )
    held = CurrentlyHeld()
    return [
        asyncio.create_task(track_heartbeat(node_runner)),
        asyncio.create_task(answer_move_on(node_runner, held)),
        asyncio.create_task(exercise_arm(node_runner, params, held)),
    ]


def main():
    NodeBuilder().run(setup)


if __name__ == "__main__":
    main()
