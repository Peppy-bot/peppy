import asyncio

from peppygen import NodeBuilder, NodeRunner
from peppygen.parameters import Parameters
from peppygen.consumed_topics import hello_world_param_message_stream


async def setup(_params: Parameters, node_runner: NodeRunner) -> list[asyncio.Task]:
    return [asyncio.create_task(receive_messages(node_runner))]


async def receive_messages(node_runner: NodeRunner):
    # Subscribe once; the held subscription buffers every message in order, so
    # iterating never drops a message published between iterations.
    subscription = await hello_world_param_message_stream.subscribe(node_runner)
    async for producer, message in subscription:
        print(f"Received from {producer.instance_id}: {message.message}")


def main():
    NodeBuilder().run(setup)


if __name__ == "__main__":
    main()
