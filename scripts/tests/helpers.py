"""Helpers the release script tests share."""

from __future__ import annotations

import json
from collections.abc import Callable, Mapping, Sequence
from unittest.mock import MagicMock

from functions.hub_ci import resolve

# A commit of its own for each hub, by hub name, in the order of the hubs.
HUB_COMMITS = {
    hub.name: f"{index:x}" * 40 for index, hub in enumerate(resolve.HUBS, start=10)
}


def hub_set_document(refs: Mapping[str, str] | None = None) -> dict:
    """A hub set as the hub-set job records it: every hub at its commit of
    HUB_COMMITS, and at the ref *refs* names for it, else at `main`."""
    refs = refs or {}
    return {
        "hubs": {
            name: {"ref": refs.get(name, "main"), "commit": commit}
            for name, commit in HUB_COMMITS.items()
        }
    }


def ancestry_resolver(
    ancestry: Mapping[tuple[str, str], bool],
) -> Callable[[str, str], bool]:
    """An is_ancestor stub answering the given (ancestor, descendant) pairs;
    any other pair raises KeyError."""
    return lambda ancestor, descendant: ancestry[(ancestor, descendant)]


def claude_answer(structured: object, returncode: int = 0) -> MagicMock:
    """A claude run that exits with *returncode* and whose JSON envelope
    carries *structured* as its structured output."""
    mock = MagicMock()
    mock.returncode = returncode
    mock.stdout = json.dumps(
        {
            "type": "result",
            "result": json.dumps(structured),
            "structured_output": structured,
        }
    )
    mock.stderr = ""
    return mock


def flag_value(cmd: Sequence[str], flag: str) -> str:
    """Return the argv element following *flag* in *cmd*."""
    assert flag in cmd, f"{flag} not in {cmd}"
    return cmd[cmd.index(flag) + 1]


def unwrapped(text: str) -> str:
    """*text* on one line, every run of whitespace a single space: what the
    console printed, without the line breaks it wrapped the text at."""
    return " ".join(text.split())


class FakeTime:
    """A clock that moves only when the code under test sleeps, and the sleep
    that moves it. Each sleep is recorded."""

    def __init__(self) -> None:
        self.now = 0.0
        self.sleeps: list[float] = []

    def clock(self) -> float:
        return self.now

    def sleep(self, seconds: float) -> None:
        self.sleeps.append(seconds)
        self.now += seconds
