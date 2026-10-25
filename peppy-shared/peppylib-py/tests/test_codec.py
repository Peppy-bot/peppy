"""
Tests for `peppylib.codec`, the codec API of peppy.

The parser of contracts gives the formats, the enums, the marks and the
comments of a document; the codec converts canonical values to the wire and
back; the clients call a service, follow an action and read a topic of a
contract on one producer, whose side the low-level messengers of peppylib
play here.
"""

import asyncio
import json
import platform
import sys
import uuid
from pathlib import Path

import pytest
from peppylib._peppylib import codec as native_codec

from peppylib import (
    ConcurrentAction,
    FailureError,
    MessengerHandle,
    ProducerRef,
    QoSProfile,
    SenderTarget,
    ServiceMessenger,
    TopicMessenger,
    TopicRetention,
    ZenohdInstance,
)
from peppylib.codec import (
    ActionClient,
    Contract,
    ContractError,
    ConversionError,
    MessageCodec,
    ServiceClient,
    TopicConsumer,
    UndeclaredFailureKindError,
)

TOOLS_DIR = Path(__file__).resolve().parents[2] / "peppy-config-model" / "tools"

CORE_NODE = "codec_core"
PROVIDER_ID = "arm_provider"
CLIENT_ID = "codec_client"
PROVIDER = ProducerRef(CORE_NODE, PROVIDER_ID)

ARM = """// An arm with a gripper.
{
    peppy_schema: "contract/v1",
    manifest: { name: "arm", tag: "v1", labels: ["robot"] },
    failure_kinds: { unreachable: "No plan to the target" },
    interfaces: {
        topics: [
            {
                name: "state",
                retention: { latest: 1 },
                message_format: {
                    // The mode of the arm.
                    mode: { $type: "string", $enum: ["idle", "moving"] },
                    joints: {
                        $type: "array",
                        $items: { $type: "object", angle: { $type: "f64", $marks: ["joint_angle"] } },
                    },
                },
            },
        ],
        services: [
            // Sends the arm home.
            {
                name: "home",
                marks: ["moves_robot"],
                request_message_format: { speed: "f32", since: "time" },
                response_message_format: { moved: "bool", count: "u64" },
                failure_kinds: ["unreachable"],
            },
        ],
        actions: [
            {
                name: "move",
                marks: ["moves_robot", "long_motion"],
                goal_service: { request_message_format: { x: "f64" } },
                feedback_topic: { message_format: { progress: "f32" } },
                result_service: { response_message_format: { reached: "bool" } },
                failure_kinds: ["unreachable"],
            },
        ],
    },
}
"""


def _bundled_compiler() -> Path:
    """The Cap'n Proto compiler of this host in the tree of peppy-shared."""
    system = {"linux": "linux", "darwin": "macos"}[sys.platform]
    machine = {"x86_64": "x86_64", "aarch64": "aarch64", "arm64": "aarch64"}[
        platform.machine()
    ]
    return TOOLS_DIR / f"capnp_{system}_{machine}"


@pytest.fixture(scope="module", autouse=True)
def _compiler():
    """Hand the codec the compiler of the tree, as `peppylib.codec` hands it
    the one that peppy deploys beside it in a node."""
    native_codec.carry_compiler_from_file(str(_bundled_compiler()))


@pytest.fixture
def arm() -> Contract:
    return Contract.parse(ARM)


def test_the_contract_gives_its_members_formats_enums_marks_and_comments(arm):
    assert (arm.name, arm.tag, arm.labels) == ("arm", "v1", ["robot"])
    assert arm.comment == "An arm with a gripper."
    assert (arm.topics, arm.services, arm.actions) == (["state"], ["home"], ["move"])
    assert repr(arm) == 'Contract("arm", "v1")'

    assert arm.message_format("service", "home", "request") == {
        "speed": "f32",
        "since": "time",
    }
    assert arm.message_format("action", "move", "result") == {"reached": "bool"}
    assert arm.message_format("action", "move", "request") is None

    assert arm.field_enum("topic", "state", "message", ["mode"]) == ["idle", "moving"]
    assert arm.field_enum("topic", "state", "message", ["joints", "angle"]) is None
    assert arm.field_marks("topic", "state", "message", ["joints", "angle"]) == [
        "joint_angle"
    ]
    assert arm.field_marks("topic", "state", "message", ["mode"]) == []
    with pytest.raises(KeyError):
        arm.field_marks("topic", "state", "message", ["pose"])

    assert arm.member_marks("action", "move") == ["moves_robot", "long_motion"]
    assert arm.member_marks("topic", "state") == []
    assert arm.member_comment("service", "home") == "Sends the arm home."
    assert arm.member_comment("action", "move") is None
    assert (
        arm.field_comment("topic", "state", "message", ["mode"])
        == "The mode of the arm."
    )
    assert arm.failure_kinds("action", "move") == [
        ("unreachable", "No plan to the target")
    ]

    with pytest.raises(ValueError, match="unknown member kind"):
        arm.member_marks("goal", "move")
    with pytest.raises(ValueError, match="unknown message side"):
        arm.message_format("service", "home", "reply")


def test_the_document_as_data_parses_to_the_same_contract(arm):
    again = Contract.parse(json.dumps(arm.to_dict()))
    assert again.to_dict() == arm.to_dict()
    assert again.member_marks("action", "move") == ["moves_robot", "long_motion"]
    assert again.comment is None, "the data holds no comment"


def test_a_text_that_is_no_contract_is_refused_with_the_key():
    with pytest.raises(ContractError, match="interfaces.services"):
        Contract.parse(
            '{ peppy_schema: "contract/v1", manifest: { name: "x", tag: "v1" },'
            ' interfaces: { services: [{ name: "s", request_message_format: { a: "f65" } }] } }'
        )
    with pytest.raises(ValueError):
        Contract.parse("{")


def test_the_codec_round_trips_the_canonical_values(arm):
    codec = MessageCodec(
        "home_request", arm.message_format("service", "home", "request")
    )
    value = {"speed": 0.5, "since": "2023-11-14T22:13:20.123456789Z"}
    encoded = codec.encode(value)
    assert isinstance(encoded, bytes)
    assert codec.decode(encoded) == value

    response = MessageCodec(
        "home_response", arm.message_format("service", "home", "response")
    )
    big = {"moved": True, "count": "18446744073709551615"}
    assert response.decode(response.encode(big)) == big

    with pytest.raises(ConversionError, match="`count` is not a decimal string"):
        response.encode({"moved": True, "count": 5})
    with pytest.raises(ConversionError, match="`moved` is missing"):
        response.encode({"count": "1"})
    with pytest.raises(ValueError, match="not a message format"):
        MessageCodec("broken", {"a": "f65"})


def test_a_client_of_a_member_that_the_contract_does_not_declare_is_refused(arm):
    with pytest.raises(ValueError, match="declares no service `move`"):
        ServiceClient(arm, "move")
    with pytest.raises(ValueError, match="declares no action `home`"):
        ActionClient(arm, "home")


@pytest.mark.asyncio
async def test_the_service_client_calls_a_service_of_the_contract(arm):
    request_codec = MessageCodec(
        "req", arm.message_format("service", "home", "request")
    )
    response_codec = MessageCodec(
        "res", arm.message_format("service", "home", "response")
    )
    async with await ZenohdInstance.start_ephemeral("127.0.0.1") as router:
        provider = await MessengerHandle.from_host_port(router.host, router.port)
        client = await MessengerHandle.from_host_port(router.host, router.port)
        service = await ServiceMessenger.listen(
            provider, CORE_NODE, PROVIDER_ID, SenderTarget.contract("arm", "v1"), "home"
        )

        received = []

        def answer(request):
            received.append(request_codec.decode(request.payload))
            return response_codec.encode({"moved": True, "count": "3"})

        handler = asyncio.ensure_future(service.handle_next_request(answer))
        home = ServiceClient(arm, "home")
        response = await home.call(
            client,
            CORE_NODE,
            CLIENT_ID,
            PROVIDER,
            {"speed": 0.25, "since": "1970-01-01T00:00:01.000000000Z"},
            response_timeout_secs=30.0,
        )
        await handler
        assert response == {"moved": True, "count": "3"}
        assert received == [{"speed": 0.25, "since": "1970-01-01T00:00:01.000000000Z"}]

        async def refuse(kind):
            def fail(_request):
                raise FailureError(kind, "the arm is blocked")

            await service.handle_next_request(fail)

        refusing = asyncio.create_task(refuse("unreachable"))
        with pytest.raises(FailureError) as declared:
            await home.call(
                client,
                CORE_NODE,
                CLIENT_ID,
                PROVIDER,
                {"speed": 1.0, "since": "1970-01-01T00:00:00Z"},
            )
        await refusing
        assert declared.value.args == ("unreachable", "the arm is blocked")

        undeclared = asyncio.create_task(refuse("jammed"))
        with pytest.raises(UndeclaredFailureKindError, match="jammed"):
            await home.call(
                client,
                CORE_NODE,
                CLIENT_ID,
                PROVIDER,
                {"speed": 1.0, "since": "1970-01-01T00:00:00Z"},
            )
        await undeclared


@pytest.mark.asyncio
async def test_the_action_client_follows_a_goal_to_its_result_and_cancels_one(arm):
    goal_codec = MessageCodec("goal", arm.message_format("action", "move", "goal"))
    feedback_codec = MessageCodec(
        "feedback", arm.message_format("action", "move", "feedback")
    )
    result_codec = MessageCodec(
        "result", arm.message_format("action", "move", "result")
    )
    async with await ZenohdInstance.start_ephemeral("127.0.0.1") as router:
        provider = await MessengerHandle.from_host_port(router.host, router.port)
        client = await MessengerHandle.from_host_port(router.host, router.port)
        action = await ConcurrentAction.expose(
            provider,
            CORE_NODE,
            PROVIDER_ID,
            SenderTarget.contract("arm", "v1"),
            "move",
            True,
        )
        move = ActionClient(arm, "move")
        assert move.reports_feedback

        async def complete():
            pending = await action.recv_next_goal()
            assert goal_codec.decode(pending.request_bytes) == {"x": 1.5}
            context = await pending.accept(b"")
            await context.publish_feedback(feedback_codec.encode({"progress": 0.5}))
            await context.complete(result_codec.encode({"reached": True}))

        server = asyncio.create_task(complete())
        goal = await move.send_goal(client, CORE_NODE, CLIENT_ID, PROVIDER, {"x": 1.5})
        assert goal.accepted and goal.refusal is None and goal.goal_id
        assert await asyncio.wait_for(goal.next_feedback(), timeout=30.0) == {
            "progress": 0.5
        }
        assert await asyncio.wait_for(goal.next_feedback(), timeout=30.0) is None
        outcome = await goal.result(client, 30.0)
        assert (outcome.status, outcome.result) == ("completed", {"reached": True})
        assert outcome.failure_kind is None
        await server

        async def cancelled():
            pending = await action.recv_next_goal()
            context = await pending.accept(b"")
            await context.cancel_signal()
            await context.complete_cancelled(
                "stopped by the client", result_codec.encode({"reached": False})
            )

        server = asyncio.create_task(cancelled())
        goal = await move.send_goal(client, CORE_NODE, CLIENT_ID, PROVIDER, {"x": 2.0})
        assert await goal.cancel(client, 30.0) == "signalled"
        outcome = await goal.result(client, 30.0)
        assert outcome.status == "cancelled"
        assert outcome.reason == "stopped by the client"
        assert outcome.result == {"reached": False}
        await server


@pytest.mark.asyncio
async def test_the_topic_consumer_reads_the_retained_message_of_a_topic(arm):
    codec = MessageCodec("state", arm.message_format("topic", "state", "message"))
    test_id = uuid.uuid4().hex[:8]
    async with await ZenohdInstance.start_ephemeral("127.0.0.1") as router:
        provider = await MessengerHandle.from_host_port(router.host, router.port)
        client = await MessengerHandle.from_host_port(router.host, router.port)
        publisher = await TopicMessenger.declare_publisher(
            provider,
            CORE_NODE,
            f"{PROVIDER_ID}_{test_id}",
            SenderTarget.contract("arm", "v1"),
            "state",
            QoSProfile.Standard,
            TopicRetention.latest(1),
        )
        state = {"mode": "moving", "joints": [{"angle": 0.25}, {"angle": -1.0}]}
        await publisher.publish(codec.encode(state))

        producer = ProducerRef(CORE_NODE, f"{PROVIDER_ID}_{test_id}")
        consumer = await TopicConsumer.subscribe(
            client, CORE_NODE, CLIENT_ID, arm, "state", producer
        )
        message = await asyncio.wait_for(consumer.next_message(), timeout=30.0)
        assert message == (producer, state)
