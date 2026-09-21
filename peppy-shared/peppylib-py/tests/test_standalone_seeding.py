"""
Tests for the daemon-less seeding builders on StandaloneConfig.

A standalone node has no daemon to hand it a boot config, so every slot kind
that a launch would resolve has a builder standing in for it: `with_peer_pin`
for pairing slots, `with_bound_producer` / `with_vacant_producer_slot` for
producer-binding slots, and `with_observed_source` for observer slots. Python
exposes the same set as Rust, so a node developed in either language runs
standalone on the same terms.

Python equivalent of the standalone seeding tests in
crates/peppylib-rs/src/runtime/processor.rs.
"""

import asyncio
import queue
import tempfile
import threading
from pathlib import Path

import pytest

from peppylib import ZenohdInstance
from peppylib.config import NODE_CONFIG_FILE, RUNTIME_CONFIG_VAR_NAME
from peppylib.runtime import NodeBuilder, StandaloneConfig

# Every slot kind a launch would resolve, at the cardinalities whose seeding
# rules differ. Observer slots must also be consumed, which the manifest parser
# requires; producer-binding slots carry no such rule.
SEEDED_SLOTS_CONFIG = """{
  peppy_schema: "node/v1",
  manifest: {
    name: "test_node",
    tag: "v1",
    depends_on: {
      pairings: [
        { name: "arm_link", tag: "v1", role: "controller", link_id: "arm" },
        { name: "arm_link", tag: "v1", role: "controller", link_id: "limbs", cardinality: "zero_or_more" }
      ],
      nodes: [
        { name: "camera", tag: "v1", link_id: "main" },
        { name: "camera", tag: "v1", link_id: "wrist_camera", cardinality: "zero_or_one" },
        { name: "camera", tag: "v1", link_id: "spare_cameras", cardinality: "zero_or_more" }
      ],
      pairing_observers: [
        { name: "arm_link", tag: "v1", role: "arm", link_id: "sole_arm" },
        { name: "arm_link", tag: "v1", role: "arm", link_id: "watched_arms", cardinality: "one_or_more" },
        { name: "arm_link", tag: "v1", role: "arm", link_id: "spare_arms", cardinality: "zero_or_more" }
      ]
    }
  },
  interfaces: {
    topics: {
      consumes: [
        { link_id: "sole_arm", name: "joint_states" },
        { link_id: "watched_arms", name: "joint_states" },
        { link_id: "spare_arms", name: "joint_states" }
      ]
    }
  },
  execution: {
    language: "python",
    parameters: {
      frequency_hz: "f64"
    },
    run_cmd: ["uv", "run"]
  },
}"""


def _seeded_config(router) -> StandaloneConfig:
    """Every builder exercised at once, in the order a launch would resolve them."""
    return (
        StandaloneConfig()
        .with_parameters({"frequency_hz": 10.0})
        .with_messaging(router.host, router.port)
        .with_instance_id("standalone_1")
        .with_peer_pin("arm", "core_x", "arm_1", "controller")
        .with_peer_pin_in_copy("limbs", "core_x", "left_1", "engine", "robot_a")
        .with_peer_pin("limbs", "core_x", "left_2", "engine")
        .with_bound_producer("main", "core_x", "camera_1")
        .with_vacant_producer_slot("wrist_camera")
        .with_observed_source("sole_arm", "core_x", "left_arm", "commander")
        .with_observed_source("watched_arms", "core_x", "right_arm", "commander")
        .with_observed_source("watched_arms", "core_x", "left_arm", "commander")
    )


def _run_standalone(peppy_config_path: str, standalone_config, setup_fn):
    """Run a node on its own thread and hand back its result and error queues."""
    result_queue: queue.Queue = queue.Queue()
    error_queue: queue.Queue = queue.Queue()

    def run_node():
        try:

            def wrapped(params, node_runner):
                try:
                    setup_fn(params, node_runner, result_queue)
                finally:
                    result_queue.put(("token", node_runner.cancellation_token()))

            (
                NodeBuilder()
                .with_config_path(peppy_config_path)
                .standalone(standalone_config)
                .run(wrapped)
            )
        except Exception as e:  # noqa: BLE001 - surfaced through the queue
            error_queue.put(e)

    thread = threading.Thread(target=run_node, daemon=True)
    thread.start()
    return thread, result_queue, error_queue


@pytest.mark.asyncio
async def test_standalone_seeds_every_slot_kind(monkeypatch):
    """Each builder reaches the runtime, and each slot reads back what it seeded."""
    monkeypatch.delenv(RUNTIME_CONFIG_VAR_NAME, raising=False)
    async with await ZenohdInstance.start_ephemeral("127.0.0.1") as router:
        with tempfile.TemporaryDirectory() as temp_dir:
            peppy_config_path = str(Path(temp_dir) / NODE_CONFIG_FILE)
            Path(peppy_config_path).write_text(SEEDED_SLOTS_CONFIG)

            def setup_fn(params, node_runner, results):
                assert params.frequency_hz == 10.0

                # A pinned pairing slot reads as already paired; a multi slot
                # holds every pin with the copy it was seeded in.
                peer = node_runner.peer("arm").paired()
                results.put(("peer", peer))
                results.put(("limbs", node_runner.peer_set("limbs").members()))

                # Producer bindings: the sole one, the vacant one, and the
                # zero_or_more slot left unseeded entirely.
                results.put(("main", node_runner.bound_producer("main")))
                results.put(
                    ("wrist", node_runner.optional_bound_producer("wrist_camera"))
                )
                results.put(("spare_cams", node_runner.bound_producers("spare_cameras")))

                # Observer slots, each through the accessor its cardinality
                # declares. A `one` slot answers without an Option.
                results.put(
                    ("sole_arm", node_runner.observation_slot("sole_arm").sole_source())
                )
                results.put(
                    (
                        "watched_arms",
                        node_runner.observation_slot_set("watched_arms").sources(),
                    )
                )
                results.put(
                    (
                        "spare_arms",
                        node_runner.observation_slot_set("spare_arms").sources(),
                    )
                )

            thread, results, errors = _run_standalone(
                peppy_config_path, _seeded_config(router), setup_fn
            )

            # The token is put in a `finally`, so it always arrives last: drain
            # until it does rather than counting the reads, which would silently
            # hang or truncate the moment a slot is added below.
            seen = {}
            while True:
                key, value = await asyncio.to_thread(results.get, timeout=10.0)
                if key == "token":
                    value.cancel()
                    break
                seen[key] = value
            thread.join(timeout=10.0)

    assert errors.empty(), f"Runner error: {errors.get_nowait()}"

    assert seen["peer"] is not None, "a pinned pairing slot boots paired"
    assert seen["peer"].producer.instance_id == "arm_1"
    assert seen["peer"].peer_link_id == "controller"
    assert [(m.info.producer.instance_id, m.copy) for m in seen["limbs"]] == [
        ("left_1", "robot_a"),
        ("left_2", None),
    ]

    assert seen["main"].instance_id == "camera_1"
    assert seen["wrist"] is None, "a vacant zero_or_one slot binds nothing"
    assert seen["spare_cams"] == [], "an unseeded zero_or_more slot binds nothing"

    assert seen["sole_arm"].producer.instance_id == "left_arm"
    assert seen["sole_arm"].source_link_id == "commander"

    assert [source.producer.instance_id for source in seen["watched_arms"]] == [
        "right_arm",
        "left_arm",
    ], "with_observed_source call order must be preserved"

    assert seen["spare_arms"] == [], "an unseeded zero_or_more observer sees nothing"


@pytest.mark.asyncio
async def test_standalone_unseeded_floored_observer_fails_startup(monkeypatch):
    """A `one` observer slot has no empty state, so leaving it unseeded is fatal.

    The same rule a daemon launch enforces at plan time, applied to the builder
    that stands in for it.
    """
    monkeypatch.delenv(RUNTIME_CONFIG_VAR_NAME, raising=False)
    async with await ZenohdInstance.start_ephemeral("127.0.0.1") as router:
        with tempfile.TemporaryDirectory() as temp_dir:
            peppy_config_path = str(Path(temp_dir) / NODE_CONFIG_FILE)
            Path(peppy_config_path).write_text(SEEDED_SLOTS_CONFIG)

            unseeded = (
                StandaloneConfig()
                .with_parameters({"frequency_hz": 10.0})
                .with_messaging(router.host, router.port)
                .with_instance_id("standalone_1")
                .with_peer_pin("arm", "core_x", "arm_1", "controller")
                .with_bound_producer("main", "core_x", "camera_1")
                .with_vacant_producer_slot("wrist_camera")
                .with_observed_source("watched_arms", "core_x", "right_arm", "commander")
            )

            def setup_fn(params, node_runner, results):  # pragma: no cover
                raise AssertionError("setup must not run when startup fails")

            thread, _results, errors = _run_standalone(
                peppy_config_path, unseeded, setup_fn
            )
            thread.join(timeout=10.0)

    assert not thread.is_alive(), "the runner should have exited"
    assert not errors.empty(), "an unseeded `one` observer slot must fail startup"
    message = str(errors.get_nowait())
    assert "sole_arm" in message, message
