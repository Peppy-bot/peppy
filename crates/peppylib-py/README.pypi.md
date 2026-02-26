# peppylib

Python bindings for the peppyOS control library — topics, services, and actions over [Zenoh](https://zenoh.io/).

Requires Python >= 3.11.

## Installation

```bash
pip install peppylib
```

## Quick Start

### Topics (Publish / Subscribe)

```python
import asyncio
from peppylib import MessengerHandle, TopicMessenger
from peppylib.config import DEFAULT_MESSAGING_PORT, QoSProfile

async def main():
    publisher = await MessengerHandle.from_host_port("localhost", DEFAULT_MESSAGING_PORT)
    subscriber = await MessengerHandle.from_host_port("localhost", DEFAULT_MESSAGING_PORT)

    # Subscribe first so we don't miss the message
    subscription = await TopicMessenger.subscribe(
        subscriber, "daemon", "instance-1", "my-node", "greetings",
        None, None, QoSProfile.Reliable,
    )

    # Publish
    await TopicMessenger.emit(
        publisher, "daemon", "instance-1", "my-node", "greetings",
        QoSProfile.Reliable, b"Hello!",
    )

    # Receive
    msg = await asyncio.wait_for(subscription.on_next_message(), timeout=3.0)
    if msg is not None:
        print(msg.payload.decode())

asyncio.run(main())
```

### Services (Request / Response)

```python
import asyncio
from peppylib import MessengerHandle, ServiceMessenger
from peppylib.config import DEFAULT_MESSAGING_PORT

async def main():
    server_handle = await MessengerHandle.from_host_port("localhost", DEFAULT_MESSAGING_PORT)
    client_handle = await MessengerHandle.from_host_port("localhost", DEFAULT_MESSAGING_PORT)

    # Server
    service = await ServiceMessenger.listen(
        server_handle, "daemon", "instance-1", "my-node", "echo",
    )

    async def on_request(req) -> bytes:
        return req.payload

    async def serve():
        await service.handle_next_request(on_request)

    # Client
    async def call():
        response = await ServiceMessenger.poll(
            client_handle, "daemon", "instance-1", "my-node", "echo",
            None, None, b"ping", 3.0,
        )
        print(response.payload.decode())

    # Run server and client concurrently
    await asyncio.gather(asyncio.create_task(serve()), asyncio.create_task(call()))

asyncio.run(main())
```

### Actions (Goal / Feedback / Result)

```python
import asyncio
from peppylib import ActionMessenger, MessengerHandle
from peppylib.config import DEFAULT_MESSAGING_PORT, QoSProfile

async def main():
    server_handle = await MessengerHandle.from_host_port("localhost", DEFAULT_MESSAGING_PORT)
    client_handle = await MessengerHandle.from_host_port("localhost", DEFAULT_MESSAGING_PORT)

    # Server: expose an action and handle goal → feedback → result
    action = await ActionMessenger.expose(
        server_handle, "daemon", "instance-1", "my-node", "compute",
    )

    async def server():
        await action.goal_service.handle_next_request(
            lambda req: f"Goal accepted: {req.payload.decode()}".encode()
        )
        await action.feedback_publisher.publish(b"Working on it...")
        await action.result_service.handle_next_request(
            lambda _req: b"Computation complete!"
        )

    # Client: send goal, receive feedback, get result
    async def client():
        goal = await ActionMessenger.send_goal(
            client_handle, "daemon", "instance-1", "my-node", "compute",
            None, None, b"task-data", QoSProfile.Reliable, 5.0,
        )
        print(goal.goal_response.payload.decode())

        feedback = await asyncio.wait_for(goal.on_next_feedback(), timeout=5.0)
        print(feedback.payload.decode())

        result = await ActionMessenger.request_result(client_handle, goal, 5.0)
        print(result.payload.decode())

    await asyncio.gather(asyncio.create_task(server()), asyncio.create_task(client()))

asyncio.run(main())
```

## Documentation

Full documentation is available at [docs.peppy.bot](https://docs.peppy.bot).

## License

Apache-2.0
