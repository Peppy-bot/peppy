"""Counts the frames a camera publishes, in place of reconstructing a video.

The second container node in this repository, and the one the federated tests
place on the peer. It writes no file: `video_duration_seconds` bounds the window
it reports on, and the report is the whole output.
"""

import asyncio
import time

from peppygen import NodeBuilder, NodeRunner
from peppygen.consumed_topics.camera import video_stream
from peppygen.parameters import Parameters


async def count_frames(params: Parameters, node_runner: NodeRunner) -> None:
    subscription = await video_stream.subscribe(node_runner)

    captured = 0
    window_started = None
    reported = False
    while True:
        received = await subscription.next()
        if received is None:
            break
        producer, message = received

        captured += 1
        if captured == 1:
            window_started = time.monotonic()
            print(
                f"[video_reconstruction] first frame from "
                f"{producer.core_node}/{producer.instance_id}: "
                f"{message.width}x{message.height} {message.encoding}",
                flush=True,
            )
            continue

        elapsed = time.monotonic() - window_started
        if not reported and elapsed >= params.video_duration_seconds:
            reported = True
            print(
                f"[video_reconstruction] {captured} frames over "
                f"{params.video_duration_seconds}s",
                flush=True,
            )


async def setup(params: Parameters, node_runner: NodeRunner) -> list[asyncio.Task]:
    async def announce_shutdown():
        print("[video_reconstruction] Shutdown signal received", flush=True)

    node_runner.on_shutdown(announce_shutdown)

    return [asyncio.create_task(count_frames(params, node_runner))]


def main():
    NodeBuilder().run(setup)


if __name__ == "__main__":
    main()
