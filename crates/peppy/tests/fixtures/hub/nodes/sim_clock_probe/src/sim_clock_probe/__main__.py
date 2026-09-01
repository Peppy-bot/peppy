"""Reads this machine's resolved clock and reports every sample.

Samples are numbered so a log reader can name "the Nth sample after this
instant" and know the probe kept sampling; the values are whatever the
machine's clock serves, reported verbatim.
"""

import asyncio

from peppygen import NodeBuilder, NodeRunner, clock
from peppygen.parameters import Parameters


async def report_clock(node_runner: NodeRunner, params: Parameters) -> None:
    await clock.init(node_runner)
    interval = params.poll_interval_ms / 1000.0
    token = node_runner.cancellation_token()
    sample = 0
    while not token.is_cancelled():
        sample += 1
        try:
            print(
                f"[clock-probe] sample={sample} now_ns={clock.now_ns()}",
                flush=True,
            )
        except RuntimeError:
            print(f"[clock-probe] sample={sample} clock not ready", flush=True)
        await asyncio.sleep(interval)


async def setup(params: Parameters, node_runner: NodeRunner) -> list[asyncio.Task]:
    return [asyncio.create_task(report_clock(node_runner, params))]


def main():
    NodeBuilder().run(setup)


if __name__ == "__main__":
    main()
