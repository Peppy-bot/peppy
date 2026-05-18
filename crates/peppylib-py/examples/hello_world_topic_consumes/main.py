import asyncio
import signal
from datetime import datetime

from peppylib import MessengerHandle, SenderTarget, TopicMessenger
from peppylib.names import generate_name
from peppylib.config import DEFAULT_MESSAGING_PORT, QoSProfile


async def main():
    topic_name = "hello_msg"
    qos = QoSProfile.Reliable

    # Those properties are found in the peppy_launcher.json5 `deployments` array
    node_name = "hello_node"
    node_tag = "v1"
    core_node = f"{generate_name()}_core"
    instance_id = f"{generate_name()}_receiver"

    # Create a messenger for the receiving node.
    host = "127.0.0.1"
    port = DEFAULT_MESSAGING_PORT

    try:
        receiver_handle = await MessengerHandle.from_host_port(host, port)
    except Exception as error:
        raise RuntimeError(
            f"failed to create messenger on {host}:{port}: {error}.\n"
            "Did you start a zenohd server with the `zenohd_simple` example?"
        )

    subscription = await TopicMessenger.subscribe(
        receiver_handle,
        core_node,
        instance_id,
        SenderTarget.node(node_name, node_tag),
        topic_name,
        None,  # target_core_node (None = any)
        None,  # target_instance_id (None = any)
        qos,
    )

    print("Waiting for payload... Press CTRL+C to stop.")

    stop_event = asyncio.Event()

    def handle_sigint():
        print("Received CTRL+C, exiting.")
        stop_event.set()

    loop = asyncio.get_running_loop()
    loop.add_signal_handler(signal.SIGINT, handle_sigint)

    while not stop_event.is_set():
        try:
            maybe_msg = await asyncio.wait_for(
                subscription.on_next_message(),
                timeout=0.1,
            )
            if maybe_msg is not None:
                payload = maybe_msg.payload.decode("utf-8", errors="replace")
                timestamp = datetime.now().strftime("%Y-%m-%d %H:%M:%S")
                print(
                    f"[{timestamp}] Received `{payload}` from instance_id `{maybe_msg.instance_id}` "
                    f"and core_node `{maybe_msg.core_node}`"
                )
            else:
                print("Subscription closed by sender.")
                break
        except asyncio.TimeoutError:
            continue


if __name__ == "__main__":
    asyncio.run(main())
