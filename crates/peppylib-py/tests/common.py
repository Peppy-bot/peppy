"""
Shared test helpers and constants for peppylib integration tests.
"""

import asyncio
import hashlib
import json
import queue
import threading
from pathlib import Path

import pytest

from peppylib import SenderTarget, ServiceMessenger

TEST_NODE_NAME = "test_node"
TEST_NODE_TAG = "v1"
TEST_INSTANCE_ID = "test_instance"
TEST_FREQUENCY_HZ = 10.0

PEPPY_CONFIG = """{
  peppy_schema: "node_v1",
  manifest: {
    name: "test_node",
    tag: "v1",
  },
  execution: {
    language: "python",
    parameters: {
      frequency_hz: "f64"
    },
    run_cmd: ["uv", "run"]
  },
}"""


def create_codegen_fingerprint(config_path: str, output_path: str) -> None:
    """Create a SHA256 fingerprint of the config file."""
    config = Path(config_path)
    fingerprint_dir = config.parent / output_path
    fingerprint_dir.mkdir(parents=True, exist_ok=True)
    config_bytes = config.read_bytes()
    fingerprint = hashlib.sha256(config_bytes).hexdigest()
    (fingerprint_dir / "peppy.json5.sha256").write_text(f"{fingerprint}\n")


def create_runtime_config(
    path: str,
    host: str,
    port: int,
    node_name: str,
    core_node: str,
    instance_id: str,
    arguments: dict,
    node_tag: str = TEST_NODE_TAG,
) -> None:
    """Write a runtime config JSON file."""
    config = {
        "messaging_host": host,
        "messaging_port": port,
        "node_name": node_name,
        "node_tag": node_tag,
        "bound_core_node": core_node,
        "node_instance": {
            "instance_id": instance_id,
            "arguments": arguments,
        },
    }
    Path(path).write_text(json.dumps(config))


async def wait_for_service(
    messenger,
    service_name: str,
    bound_core_node: str,
    as_instance_id: str,
    target_node_name: str,
    target_core_node: str,
    target_instance_id: str,
    runner_thread: threading.Thread,
    error_queue: queue.Queue,
    timeout_secs: float = 10.0,
):
    """Poll until a service becomes reachable, or fail."""
    deadline = asyncio.get_event_loop().time() + timeout_secs
    while True:
        if not runner_thread.is_alive():
            error = error_queue.get_nowait() if not error_queue.empty() else None
            pytest.fail(f"Runner exited early: {error}")

        if await ServiceMessenger.is_reachable(
            messenger,
            bound_core_node,
            as_instance_id,
            SenderTarget.node(target_node_name, TEST_NODE_TAG),
            service_name,
            target_core_node,
            target_instance_id,):
            return

        if asyncio.get_event_loop().time() >= deadline:
            pytest.fail(f"{service_name} service did not become reachable")

        await asyncio.sleep(0.05)
