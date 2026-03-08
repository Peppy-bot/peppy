import asyncio
import signal
from datetime import datetime

from peppylib import MessengerHandle, ServiceMessenger
from peppylib.names import generate_name
from peppylib.config import DEFAULT_MESSAGING_PORT

SERVICE_NAME = "hello_service"
NODE_NAME = "hello_node"


def current_timestamp() -> str:
    return datetime.now().strftime("%Y-%m-%d %H:%M:%S")


async def handle_request(request) -> bytes:
    payload_text = request.payload.decode("utf-8")
    instance_id = request.instance_id
    core_node = request.core_node

    print(
        f"[{current_timestamp()}] Received request with payload `{payload_text}` "
        f"from `{instance_id}` and core node `{core_node}`"
    )

    response_text = f"ack: {payload_text}"
    print(f"[{current_timestamp()}] Responding with `{response_text}`")

    return response_text.encode("utf-8")


async def main():
    # Create a messenger for the receiving node.
    host = "127.0.0.1"
    port = DEFAULT_MESSAGING_PORT
    core_node = f"{generate_name()}_core"
    instance_id = f"{generate_name()}_listener"

    try:
        receiver_handle = await MessengerHandle.from_host_port(host, port)
    except Exception as error:
        raise RuntimeError(
            f"failed to create service messenger on {host}:{port}: {error}.\n"
            "Did you start a zenohd server with the `zenohd_simple` example?"
        )

    service = await ServiceMessenger.listen(
        receiver_handle,
        core_node,
        instance_id,
        NODE_NAME,
        SERVICE_NAME,
    )

    print(
        f"Waiting for service requests as instance_id {instance_id}... "
        "Press CTRL+C to stop."
    )

    loop = asyncio.get_running_loop()
    stop_event = asyncio.Event()
    loop.add_signal_handler(signal.SIGINT, stop_event.set)

    while not stop_event.is_set():
        done, _ = await asyncio.wait(
            [
                asyncio.ensure_future(service.handle_next_request(handle_request)),
                asyncio.ensure_future(stop_event.wait()),
            ],
            return_when=asyncio.FIRST_COMPLETED,
        )

        for task in done:
            result = task.result()
            if isinstance(result, bool) and not result:
                print("Service listener closed by client.")
                stop_event.set()

    print("Received CTRL+C, exiting.")


if __name__ == "__main__":
    asyncio.run(main())
