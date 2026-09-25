"""Helpers the release script tests share."""

from __future__ import annotations

from collections.abc import Mapping

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
