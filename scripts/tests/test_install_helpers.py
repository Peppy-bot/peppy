"""Tests for the helpers of the install.sh tests that change no machine."""

from __future__ import annotations

import http.server
import json
import socket
import threading
from collections.abc import Iterator
from pathlib import Path

import pytest

from functions.hub_ci import resolve

from .helpers import HUB_COMMITS, hub_set_document
from .install_helpers import (
    HUB_SET_VARIABLE,
    StackInstance,
    page_status,
    parse_stack_list,
    peppy_environment,
    pin_default_hubs,
)


def test_without_a_hub_set_the_install_keeps_its_default_hubs(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.delenv(HUB_SET_VARIABLE, raising=False)

    pin_default_hubs(tmp_path)

    assert list(tmp_path.iterdir()) == []


def test_a_hub_set_pins_each_default_hub_at_its_commit(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    hub_set_file = tmp_path / "hub-set.json"
    hub_set_file.write_text(json.dumps(hub_set_document()))
    monkeypatch.setenv(HUB_SET_VARIABLE, str(hub_set_file))
    home = tmp_path / "home"

    pin_default_hubs(home)

    entries = json.loads((home / "conf" / "repositories.json5").read_text())
    assert entries == [
        {
            "id": hub.repository_id,
            "type": "git",
            "url": hub.clone_url,
            "ref": HUB_COMMITS[hub.name],
        }
        for hub in resolve.HUBS
        if hub.visibility is resolve.Visibility.PUBLIC
    ]
    # The machine of a user holds no key that reads the private hub.
    assert "private-nodes-hub" not in json.dumps(entries)


def test_the_peppy_environment_names_the_install_of_the_home() -> None:
    assert peppy_environment(Path("/homes/quickstart")) == (
        'export PATH="/homes/quickstart/bin:$PATH" '
        "&& export PEPPY_HOME=/homes/quickstart"
    )


def _stack_listing(*nodes: dict) -> str:
    """What `peppy stack list --json` prints for one daemon whose stack holds
    *nodes*."""
    return json.dumps({"core_nodes": [{"core_node": "cn", "stack": {"nodes": nodes}}]})


def _node(name: str, *instances: dict) -> dict:
    return {"name": name, "tag": "v1", "stage": "Ready", "instances": list(instances)}


def _instance(instance_id: str, state: str = "running", healthy: bool = True) -> dict:
    return {"instance_id": instance_id, "state": state, "healthy": healthy}


def test_a_stack_listing_gives_each_instance_with_its_node() -> None:
    listing = _stack_listing(
        _node("sim_mujoco", _instance("simulation_inst")),
        _node("camera", _instance("front"), _instance("rear", "failed")),
        _node("built_and_not_started"),
    )

    assert parse_stack_list(listing) == [
        StackInstance("sim_mujoco:v1", "simulation_inst", "running", True),
        StackInstance("camera:v1", "front", "running", True),
        StackInstance("camera:v1", "rear", "failed", True),
    ]


@pytest.mark.parametrize(
    ("state", "healthy", "is_up"),
    [
        ("running", True, True),
        ("running", False, False),
        ("starting", True, False),
        ("restarting", True, False),
        ("finished", True, False),
        ("failed", True, False),
    ],
)
def test_an_instance_is_up_when_it_is_running_and_healthy(
    state: str, healthy: bool, is_up: bool
) -> None:
    assert StackInstance("camera:v1", "front", state, healthy).is_up is is_up


def test_an_instance_reads_as_its_line_of_the_report() -> None:
    restarting = StackInstance(
        "openarm_backbone:v1", "alpha_backbone_inst", "restarting", False
    )

    assert str(restarting) == (
        "alpha_backbone_inst of openarm_backbone:v1: restarting, unhealthy"
    )


@pytest.mark.parametrize(
    "listing",
    [
        "",
        "Launch complete",
        "[]",
        json.dumps({"core_nodes": [{"core_node": "cn"}]}),
        _stack_listing(_node("camera", {"instance_id": "front", "state": "running"})),
        _stack_listing(_node("camera", _instance("front", healthy="yes"))),  # type: ignore[arg-type]
    ],
)
def test_a_text_that_is_no_stack_listing_fails_the_test(listing: str) -> None:
    with pytest.raises(pytest.fail.Exception, match="printed no stack listing"):
        parse_stack_list(listing)


class _Pages(http.server.BaseHTTPRequestHandler):
    """A server with one page, `/`."""

    def do_GET(self) -> None:
        self.send_response(200 if self.path == "/" else 404)
        self.end_headers()

    def log_message(self, format: str, *args: object) -> None:
        """Keep the requests out of the output of the tests."""


@pytest.fixture
def page_server() -> Iterator[str]:
    """The address of a server of this machine that serves `_Pages`."""
    server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), _Pages)
    serving = threading.Thread(target=server.serve_forever)
    serving.start()
    try:
        yield f"http://127.0.0.1:{server.server_port}"
    finally:
        server.shutdown()
        serving.join()
        server.server_close()


def test_a_page_that_a_server_serves_has_the_status_200(page_server: str) -> None:
    assert page_status(f"{page_server}/") == 200


def test_a_page_that_a_server_refuses_has_the_status_of_the_refusal(
    page_server: str,
) -> None:
    assert page_status(f"{page_server}/absent") == 404


def test_a_page_of_no_server_raises_an_os_error() -> None:
    # A port that this socket holds and does not listen on: the kernel
    # refuses each connection to it.
    with socket.socket() as held:
        held.bind(("127.0.0.1", 0))
        port = held.getsockname()[1]

        with pytest.raises(OSError):
            page_status(f"http://127.0.0.1:{port}/")
