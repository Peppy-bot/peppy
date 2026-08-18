"""Boots `hello_receiver` in-process under the generated test harness and
drives one message through its consumed topic from the mocked producer: no
daemon, no real `hello_world_param` node, and no sleeps."""

from peppygen.consumed_topics.hello_world_param import message_stream
from peppygen.fixtures import harness

from hello_receiver.__main__ import setup


async def test_receives_a_message_from_the_mocked_producer():
    async with harness.start(setup) as h:
        # The first publish waits for the node's subscription to match before
        # delivering, so this is deterministic: a return means the node
        # received the message, and no subscriber within the readiness
        # timeout is a loud error instead of a silent drop.
        await h.mocks.deps.hello_world_param.message_stream.publish(
            message_stream.Message(message="hello from the mock")
        )
