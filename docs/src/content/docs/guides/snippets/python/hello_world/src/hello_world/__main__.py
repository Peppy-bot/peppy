import asyncio
import logging

from peppygen import NodeBuilder, NodeRunner
from peppygen.emitted_topics import message_stream
from peppygen.parameters import Parameters

logger = logging.getLogger(__name__)


async def emit_hello_world_loop(node_runner: NodeRunner):
    # Declare the publisher once, then publish each message on it.
    try:
        publisher = await message_stream.declare_publisher(node_runner)
    except Exception:
        logger.exception("Failed to declare message_stream publisher")
        return

    counter = 0
    while True:
        counter += 1
        message = f"hello world count {counter}"
        print(message, flush=True)
        try:
            await publisher.publish(message_stream.build_message(message))
        except Exception:
            logger.exception("Failed to publish hello world")
        await asyncio.sleep(3)


async def setup(params: Parameters, node_runner: NodeRunner) -> list[asyncio.Task]:
    return [asyncio.create_task(emit_hello_world_loop(node_runner))]


def main():
    NodeBuilder().run(setup)


if __name__ == "__main__":
    main()
