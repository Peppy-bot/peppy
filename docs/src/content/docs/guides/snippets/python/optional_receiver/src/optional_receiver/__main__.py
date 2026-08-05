import asyncio

from peppygen import NodeBuilder, NodeRunner
from peppygen.parameters import Parameters
from peppygen.consumed_topics.greeter import message_stream


async def setup(_params: Parameters, node_runner: NodeRunner) -> list[asyncio.Task]:
    # The `greeter` slot declares `cardinality: "zero_or_one"`, so its accessor
    # is `Optional`: a producer where the deployment linked one, `None` where it
    # wrote the slot vacant. There is no third case, and no empty list to
    # interpret.
    greeter = message_stream.bound_producer(node_runner)
    if greeter is None:
        print("no greeter bound: running without greetings")
        return []

    print(f"greeter bound: {greeter.instance_id}")
    return [asyncio.create_task(receive_messages(node_runner))]


async def receive_messages(node_runner: NodeRunner):
    subscription = await message_stream.subscribe(node_runner)
    async for producer, message in subscription:
        print(f"Received from {producer.instance_id}: {message.message}")


def main():
    NodeBuilder().run(setup)


if __name__ == "__main__":
    main()
