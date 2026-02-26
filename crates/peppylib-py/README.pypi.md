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
    handle = await MessengerHandle.from_host_port("localhost", DEFAULT_MESSAGING_PORT)

    # Publish
    await TopicMessenger.emit(
        handle, "daemon", "instance-1", "my-node", "greetings",
        QoSProfile.Reliable, b"Hello!",
    )

    # Subscribe
    subscription = await TopicMessenger.subscribe(
        handle, "daemon", "instance-1", "my-node", "greetings",
        None, None, QoSProfile.Reliable,
    )
    msg = await subscription.on_next_message()
    print(msg.payload.decode())

asyncio.run(main())
```

### Services (Request / Response)

```python
import asyncio
from peppylib import MessengerHandle, ServiceMessenger
from peppylib.config import DEFAULT_MESSAGING_PORT

async def main():
    handle = await MessengerHandle.from_host_port("localhost", DEFAULT_MESSAGING_PORT)

    # Server
    service = await ServiceMessenger.listen(
        handle, "daemon", "instance-1", "my-node", "echo",
    )

    async def on_request(req) -> bytes:
        return req.payload

    await service.handle_next_request(on_request)

    # Client
    response = await ServiceMessenger.poll(
        handle, "daemon", "instance-1", "my-node", "echo",
        None, None, b"ping", 3.0,
    )
    print(response.payload.decode())

asyncio.run(main())
```

### Actions (Goal / Feedback / Result)

```python
import asyncio
from peppylib import ActionMessenger, MessengerHandle
from peppylib.config import DEFAULT_MESSAGING_PORT, QoSProfile

async def main():
    handle = await MessengerHandle.from_host_port("localhost", DEFAULT_MESSAGING_PORT)

    # Send a goal
    goal_handle = await ActionMessenger.send_goal(
        handle, "daemon", "instance-1", "my-node", "compute",
        None, None, b"task-data", QoSProfile.Reliable, 5.0,
    )
    print(goal_handle.goal_response.payload.decode())

    # Receive feedback
    feedback = await goal_handle.on_next_feedback()
    print(feedback.payload.decode())

    # Get result
    result = await ActionMessenger.request_result(handle, goal_handle, 5.0)
    print(result.payload.decode())

asyncio.run(main())
```

## Documentation

Full documentation is available at [docs.peppy.bot](https://docs.peppy.bot).

## License

Apache-2.0
