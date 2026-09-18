"""
Tests for `NodeRunner.announce_endpoint` and `NodeRunner.seal_endpoints`.

Python equivalent of the endpoint cases in crates/peppylib/tests/runner.rs:
a node built from a manifest fixture that declares two endpoints, driven
through a standalone runner on the test's own event loop.
"""

import tempfile
from pathlib import Path

import pytest

from peppylib import ZenohdInstance
from peppylib.config import NODE_CONFIG_FILE, RUNTIME_CONFIG_VAR_NAME
from peppylib.runtime import NodeRunner, StandaloneConfig

from common import TEST_FREQUENCY_HZ, TEST_INSTANCE_ID

ENDPOINTS_PEPPY_CONFIG = """{
  peppy_schema: "node/v1",
  manifest: {
    name: "test_node",
    tag: "v1",
  },
  execution: {
    language: "python",
    parameters: {
      frequency_hz: "f64"
    },
    run_cmd: ["uv", "run"],
    endpoints: {
      panel: { kind: "page", description: "The operator panel." },
      viewer: { kind: "page", description: "The viewer page." },
    },
  },
}"""


async def _standalone_runner(router, temp_dir: str) -> NodeRunner:
    peppy_config_path = str(Path(temp_dir) / NODE_CONFIG_FILE)
    Path(peppy_config_path).write_text(ENDPOINTS_PEPPY_CONFIG)
    standalone_config = (
        StandaloneConfig()
        .with_parameters({"frequency_hz": TEST_FREQUENCY_HZ})
        .with_messaging(router.host, router.port)
        .with_instance_id(TEST_INSTANCE_ID)
    )
    return await NodeRunner.new_standalone(peppy_config_path, standalone_config)


@pytest.mark.asyncio
async def test_declared_labels_are_announced_once_and_sealed_in_label_order(monkeypatch):
    monkeypatch.delenv(RUNTIME_CONFIG_VAR_NAME, raising=False)
    async with await ZenohdInstance.start_ephemeral("127.0.0.1") as router:
        with tempfile.TemporaryDirectory() as temp_dir:
            node_runner = await _standalone_runner(router, temp_dir)

            node_runner.announce_endpoint("viewer", "https", "0.0.0.0", 8080, "/")
            node_runner.announce_endpoint("panel", "http", "127.0.0.1", 8765)

            with pytest.raises(ValueError, match="`admin` is not declared"):
                node_runner.announce_endpoint("admin", "http", "127.0.0.1", 9000)
            with pytest.raises(ValueError, match="`panel` is already announced"):
                node_runner.announce_endpoint("panel", "http", "127.0.0.1", 8766)

            node_runner.seal_endpoints()
            assert node_runner.announced_endpoints() == [
                ("panel", "http", "127.0.0.1", 8765, ""),
                ("viewer", "https", "0.0.0.0", 8080, "/"),
            ]
            with pytest.raises(ValueError, match="`panel` cannot be announced after setup"):
                node_runner.announce_endpoint("panel", "http", "127.0.0.1", 8765)


@pytest.mark.asyncio
async def test_a_malformed_binding_is_refused_naming_the_label(monkeypatch):
    monkeypatch.delenv(RUNTIME_CONFIG_VAR_NAME, raising=False)
    async with await ZenohdInstance.start_ephemeral("127.0.0.1") as router:
        with tempfile.TemporaryDirectory() as temp_dir:
            node_runner = await _standalone_runner(router, temp_dir)

            for scheme in ("Http", "1http", ""):
                with pytest.raises(ValueError, match="`panel` has an invalid binding.*scheme"):
                    node_runner.announce_endpoint("panel", scheme, "127.0.0.1", 8765)
            with pytest.raises(ValueError, match="`panel` has an invalid binding.*path"):
                node_runner.announce_endpoint("panel", "http", "127.0.0.1", 8765, "mcp")
            with pytest.raises(ValueError, match="`panel` has an invalid binding.*IP literal"):
                node_runner.announce_endpoint("panel", "http", "localhost", 8765)
            assert node_runner.announced_endpoints() == []


@pytest.mark.asyncio
async def test_a_declared_label_left_unannounced_fails_the_seal(monkeypatch):
    monkeypatch.delenv(RUNTIME_CONFIG_VAR_NAME, raising=False)
    async with await ZenohdInstance.start_ephemeral("127.0.0.1") as router:
        with tempfile.TemporaryDirectory() as temp_dir:
            node_runner = await _standalone_runner(router, temp_dir)

            node_runner.announce_endpoint("panel", "http", "0.0.0.0", 8765)
            with pytest.raises(ValueError, match="`viewer` is declared .* without announcing it"):
                node_runner.seal_endpoints()
