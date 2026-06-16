import asyncio
from peppygen import NodeBuilder, NodeRunner
from peppygen.parameters import Parameters
from peppygen.emitted_topics import message_stream


async def emit_hello_world_loop(node_runner: NodeRunner, name: str):
    # Declare the publisher once, then publish each message on it.
    publisher = await message_stream.declare_publisher(node_runner)
    counter = 0
    while True:
        counter += 1
        message = f"hello {name} count {counter}"
        print(message, flush=True)
        await publisher.publish(message_stream.build_message(message))
        await asyncio.sleep(3)


async def setup(params: Parameters, node_runner: NodeRunner) -> list[asyncio.Task]:
    return [asyncio.create_task(emit_hello_world_loop(node_runner, params.name))]


def main():
    NodeBuilder().run(setup)


if __name__ == "__main__":
    main()
