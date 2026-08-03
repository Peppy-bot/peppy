"""A camera that publishes a synthetic RGB frame at the configured rate.

Stands in for hardware, and deliberately decodes nothing: the frame buffer is
built once at startup and republished with a fresh header, so what this node
costs is the publish itself rather than a video decoder. That leaves the node
with no dependency beyond the peppy runtime, so building it fetches nothing this
repository does not already require.
"""

import asyncio
import time

from peppygen import NodeBuilder, NodeRunner
from peppygen.emitted_topics.camera import video_stream
from peppygen.emitted_topics.camera.video_stream import MessageHeader
from peppygen.exposed_services.camera import video_stream_info
from peppygen.parameters import Parameters

# How often the emitted-frame count is reported, counted in frames rather than
# seconds so the output is the same on a fast host and a loaded one. The offset
# of one puts the first report on the first frame, which is what a reader (or a
# test) waits for to know the stream is live.
REPORT_EVERY_FRAMES = 30

# `frame_id` is a u32 on the wire, so the counter wraps where the field does.
FRAME_ID_MODULUS = 2**32


def synthetic_frame(width: int, height: int, encoding: str) -> bytes:
    """One frame's pixel bytes: a horizontal gradient in `encoding`.

    Built once and republished unchanged. A consumer here reads the header and
    the geometry, never the pixels, so the content only has to be the right size
    for the geometry the message declares.
    """
    if encoding != "rgb8":
        raise ValueError(f"unsupported topic_encoding '{encoding}', expected 'rgb8'")
    row = bytes((x * 255 // max(width - 1, 1)) % 256 for x in range(width) for _ in range(3))
    return row * height


async def emit_frames(node_runner: NodeRunner, params: Parameters) -> None:
    video = params.video
    width = video.resolution.width
    height = video.resolution.height
    frame = synthetic_frame(width, height, video.topic_encoding)

    publisher = await video_stream.declare_publisher(node_runner)
    token = node_runner.cancellation_token()
    interval = 1.0 / video.frame_rate

    emitted = 0
    frame_id = 0
    while not token.is_cancelled():
        await publisher.publish(
            video_stream.build_message(
                MessageHeader(stamp=time.time(), frame_id=frame_id),
                video.topic_encoding,
                width,
                height,
                frame,
            )
        )
        frame_id = (frame_id + 1) % FRAME_ID_MODULUS
        emitted += 1
        if emitted % REPORT_EVERY_FRAMES == 1:
            print(f"[uvc_camera] Emitted frame {emitted}", flush=True)
        await asyncio.sleep(interval)


async def serve_stream_info(node_runner: NodeRunner, params: Parameters) -> None:
    """Answers `video_stream_info` for as long as the node runs.

    Each request is raced against the cancellation token: `handle_next_request`
    parks until a caller arrives, and at shutdown there is no caller, so without
    the race this task would still be parked when the runtime tears it down.
    """
    video = params.video
    token = node_runner.cancellation_token()
    cancelled = asyncio.ensure_future(token.cancelled())
    try:
        while not token.is_cancelled():
            request = asyncio.ensure_future(
                video_stream_info.handle_next_request(
                    node_runner,
                    lambda _request: video_stream_info.Response(
                        width=video.resolution.width,
                        height=video.resolution.height,
                        frames_per_second=video.frame_rate,
                        encoding=video.topic_encoding,
                    ),
                )
            )
            await asyncio.wait(
                [cancelled, request], return_when=asyncio.FIRST_COMPLETED
            )
            if not request.done():
                request.cancel()
                break
    finally:
        cancelled.cancel()


async def setup(params: Parameters, node_runner: NodeRunner) -> list[asyncio.Task]:
    print(
        f"[uvc_camera] {params.device_path} at "
        f"{params.video.resolution.width}x{params.video.resolution.height} "
        f"@ {params.video.frame_rate} fps, publishing {params.video.topic_encoding}",
        flush=True,
    )

    async def announce_shutdown():
        print("[uvc_camera] Shutdown signal received", flush=True)

    node_runner.on_shutdown(announce_shutdown)

    return [
        asyncio.create_task(emit_frames(node_runner, params)),
        asyncio.create_task(serve_stream_info(node_runner, params)),
    ]


def main():
    NodeBuilder().run(setup)


if __name__ == "__main__":
    main()
