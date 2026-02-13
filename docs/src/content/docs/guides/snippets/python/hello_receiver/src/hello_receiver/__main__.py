"""Main entry point for node."""

import asyncio

from peppygen import NodeBuilder, NodeRunner
from peppygen.parameters import Parameters
from peppygen.subscribed_topics import hello_world_param_message_stream


def setup(params: Parameters, node_runner: NodeRunner):
    asyncio.run(receive_messages(node_runner))


async def receive_messages(node_runner: NodeRunner):
    while True:
        try:
            (
                instance_id,
                message,
            ) = await hello_world_param_message_stream.on_next_message_received(
                node_runner
            )
            print(f"Received from {instance_id}: {message.message}")
        except Exception as e:
            print(f"Error receiving message: {e}", flush=True)
            break


def main():
    NodeBuilder().run(setup)


if __name__ == "__main__":
    main()
