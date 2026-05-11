import asyncio

from peppygen import NodeBuilder, NodeRunner
from peppygen.parameters import Parameters
from peppygen.consumed_topics import hello_world_param_message_stream


async def setup(_params: Parameters, node_runner: NodeRunner) -> list[asyncio.Task]:
    return [asyncio.create_task(receive_messages(node_runner))]


async def receive_messages(node_runner: NodeRunner):
    while True:
        (
            instance_id,
            _variant,
            message,
        ) = await hello_world_param_message_stream.on_next_message_received(node_runner)
        print(f"Received from {instance_id}: {message.message}")


def main():
    NodeBuilder().run(setup)


if __name__ == "__main__":
    main()
