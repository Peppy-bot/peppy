"""Tests for functions.hub_ci.

What resolve.py decides is tested by its own cases
(.github/actions/hub-ci-peppy/test_resolve.py). These cases hold what the
release scripts add on top: resolve.py is the script of the hub CI action,
and each of its refusals reaches a release stage as a ReleaseError.
"""

from __future__ import annotations

import json
from pathlib import Path

import pytest

from functions.cli import ReleaseError
from functions.hub_ci import load_release_set, parse_release_version, resolve

from .helpers import HUB_COMMITS, hub_set_document

REPOSITORY_ROOT = Path(__file__).resolve().parents[2]


def test_resolve_is_the_script_of_the_hub_ci_action() -> None:
    assert Path(resolve.__file__) == (
        REPOSITORY_ROOT / ".github/actions/hub-ci-peppy/resolve.py"
    )


def test_a_release_set_is_loaded_from_its_file(tmp_path: Path) -> None:
    path = tmp_path / "hub-set.json"
    path.write_text(json.dumps(hub_set_document({"mcp-hub": "peppy-release/v0.3.0"})))

    hub_set = load_release_set(path)

    assert [(r.hub.name, r.ref, r.commit) for r in hub_set.hubs] == [
        (name, "peppy-release/v0.3.0" if name == "mcp-hub" else "main", commit)
        for name, commit in HUB_COMMITS.items()
    ]


def test_a_hub_set_resolve_refuses_is_a_release_error(tmp_path: Path) -> None:
    path = tmp_path / "hub-set.json"
    path.write_text("not json")

    with pytest.raises(ReleaseError, match="is not JSON"):
        load_release_set(path)


def test_a_missing_hub_set_file_is_a_release_error(tmp_path: Path) -> None:
    with pytest.raises(ReleaseError, match="is unreadable"):
        load_release_set(tmp_path / "hub-set.json")


def test_a_release_version_resolve_refuses_is_a_release_error() -> None:
    assert parse_release_version(" v0.3.0\n") == "v0.3.0"
    with pytest.raises(ReleaseError, match="is not a peppy release version"):
        parse_release_version("v0.3")
