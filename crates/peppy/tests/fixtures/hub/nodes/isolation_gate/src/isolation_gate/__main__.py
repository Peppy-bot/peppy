"""The test's hand on the commanders' held goals.

Linked at `node run` to the commanders it should move on, it asks each one
to `move_on` with its verb, `cancel` or `release`, and logs the commander
that answered with the token of the goal it moved on. Unlinked, as the
launcher deploys it, it moves nothing on.
"""

import asyncio

from peppygen import NodeBuilder, NodeRunner
from peppygen.consumed_services.commander import move_on
from peppygen.parameters import Parameters

LOG = "[isolation-gate]"
REQUEST_TIMEOUT_S = 5.0
VERBS = ("cancel", "release")


async def move_commanders_on(node_runner: NodeRunner, params: Parameters) -> None:
    if params.verb not in VERBS:
        raise ValueError(f"verb must be one of {VERBS}, got {params.verb!r}")
    commanders = move_on.bound_producers(node_runner)
    print(f"{LOG} up, verb {params.verb}, {len(commanders)} commander(s)", flush=True)
    for commander in commanders:
        response = await move_on.poll(
            node_runner,
            commander,
            move_on.Request(verb=params.verb),
            REQUEST_TIMEOUT_S,
        )
        print(
            f"{LOG} {params.verb} {response.instance_id} token={response.data.token}",
            flush=True,
        )
    await node_runner.cancellation_token().cancelled()


async def setup(params: Parameters, node_runner: NodeRunner) -> list[asyncio.Task]:
    return [asyncio.create_task(move_commanders_on(node_runner, params))]


def main():
    NodeBuilder().run(setup)


if __name__ == "__main__":
    main()
