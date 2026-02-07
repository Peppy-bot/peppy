import asyncio

from peppylib import MessengerHandle, ServiceMessenger
from peppylib.names import generate_name
from peppylib.config import DEFAULT_MESSAGING_PORT

SERVICE_NAME = "hello_service"
NODE_NAME = "hello_node"


async def main():
    # Create a messenger for the sending node.
    host = "127.0.0.1"
    port = DEFAULT_MESSAGING_PORT
    master_node = f"{generate_name()}_master"
    instance_id = f"{generate_name()}_caller"

    try:
        sender_handle = await MessengerHandle.from_host_port(host, port)
    except Exception as error:
        raise RuntimeError(
            f"failed to create service messenger on {host}:{port}: {error}.\n"
            "Did you start a zenohd server with the `zenohd_simple` example?"
        )

    request_payload = b"Hello service"

    print(
        f"Sending service request as instance_id {instance_id} "
        f"and master node {master_node}..."
    )
    response = await ServiceMessenger.poll(
        sender_handle,
        master_node,
        instance_id,
        NODE_NAME,
        SERVICE_NAME,
        None,  # target_master_node - not needed
        None,  # target_instance_id - any instance would work
        request_payload,
        3.0,  # response_timeout_secs
    )

    response_text = response.payload.decode("utf-8")
    print(
        f"Received response from {response.instance_id} instance_id: "
        f"`{response_text}`"
    )


if __name__ == "__main__":
    asyncio.run(main())
