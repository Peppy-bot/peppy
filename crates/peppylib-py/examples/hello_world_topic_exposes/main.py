import asyncio

from peppylib import MessengerHandle, TopicMessenger
from peppylib.names import generate_name
from peppylib.config import DEFAULT_MESSAGING_PORT, QoSProfile


async def main():
    topic_name = "hello_msg"
    qos = QoSProfile.Reliable

    # Those properties are found in the peppy_launcher.json5 `deployments` array
    node_name = "hello_node"
    core_node = f"{generate_name()}_core"
    instance_id = f"{generate_name()}_emitter"

    # Create a messenger for the sending node.
    host = "127.0.0.1"
    port = DEFAULT_MESSAGING_PORT

    try:
        sender_handle = await MessengerHandle.from_host_port(host, port)
    except Exception as error:
        raise RuntimeError(
            f"failed to create messenger on {host}:{port}: {error}.\n"
            "Did you start a zenohd server with the `zenohd_simple` example?"
        )

    payload = b"Hello world"

    print(f"Sending payload as {instance_id} with core node {core_node}...")
    await TopicMessenger.emit(
        sender_handle,
        core_node,
        instance_id,
        node_name,
        topic_name,
        qos,
        payload,
    )
    print("Payload sent")


if __name__ == "__main__":
    asyncio.run(main())
