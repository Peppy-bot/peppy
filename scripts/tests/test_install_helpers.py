"""Tests for the helpers of the install.sh tests that change no machine."""

from __future__ import annotations

import json
from pathlib import Path

import pytest

from functions.hub_ci import resolve

from .helpers import HUB_COMMITS, hub_set_document
from .install_helpers import HUB_SET_VARIABLE, peppy_environment, pin_default_hubs


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
