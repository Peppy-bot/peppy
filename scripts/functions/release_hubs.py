"""The hubs a peppy release tests, and tags once it publishes.

The hub-set job of .github/workflows/parallel-release.yml records the hub set
of the release with .github/actions/hub-ci-peppy/resolve.py: every hub, each at
the commit of its tag of this release (`peppy-release/<tag>`) where it carries
one, and at the head of its `main` where it does not. The set is an explicit
set, `{"hubs": {"<hub>": {"ref": "<label>", "commit": "<40 hex>"}}}`, the schema
launchers-hub's tests take as their `set` input. Every later stage reads the
commits the set records and never a branch, since `main` can move while the
release runs.

Two stages use the set here:

- `hub-launch` dispatches launchers-hub's tests on the set and on the archive
  of this release run, and waits for that run to succeed.
- `publish` puts the tag of the release on every hub, at the commit the set
  records, before it publishes the release.
"""

from __future__ import annotations

import json
import re
from collections.abc import Callable
from dataclasses import dataclass
from pathlib import Path
from typing import Any

import httpx

from .cli import ReleaseError, console
from .github import github_api

# The prefix of the tag a release puts on each hub it tested. A release build
# of peppy `v0.31.2` reads the tag `peppy-release/v0.31.2` of each hub it
# bundles (PEPPY_RELEASE_TAG_PREFIX in
# peppy-shared/core-node-api/src/encoding/repo/git_ref.rs, which the tests hold
# this prefix to).
HUB_RELEASE_TAG_PREFIX = "peppy-release/"

# The hub whose tests launch every launcher of the set, and the workflow that
# runs them. The workflow runs as `main` defines it, and checks out the
# launchers-hub commit of the set.
LAUNCHERS_HUB = "launchers-hub"
LAUNCHERS_WORKFLOW = "tests.yml"
LAUNCHERS_WORKFLOW_BRANCH = "main"

# How often the wait reads the launchers-hub run. A full run takes up to about
# 75 minutes, and the wait gives up after RUN_WAIT_TIMEOUT_SECONDS, inside the
# 180 minutes the hub-launch job has.
RUN_POLL_INTERVAL_SECONDS = 60.0
RUN_WAIT_TIMEOUT_SECONDS = 170 * 60.0
# A read of the run that fails (a GitHub API error, a dropped connection) is
# tried again at the next poll, so one failure does not throw away a run that
# is well on its way. This many failures in a row end the wait.
MAX_FAILED_RUN_READS = 5

_GITHUB_API = "https://api.github.com"
_COMMIT_PATTERN = re.compile(r"[0-9a-f]{40}")
# What GitHub accepts as a repository name, the key of each hub of the set.
_REPOSITORY_NAME_PATTERN = re.compile(r"[A-Za-z0-9._-]+")
_HUB_SET_SHAPE = '{"hubs": {"<hub>": {"ref": "<label>", "commit": "<40 hex>"}}}'


def hub_release_tag(tag: str) -> str:
    """The tag the release of *tag* puts on each hub it tested."""
    return f"{HUB_RELEASE_TAG_PREFIX}{tag}"


# --- the hub set ---


@dataclass(frozen=True)
class RecordedHub:
    """A hub of the set: its repository name, the label of where its commit
    came from (`main` or its tag of the release), and the commit."""

    name: str
    ref: str
    commit: str


@dataclass(frozen=True)
class HubSet:
    """The hubs the release tests and tags, in the order the set lists them."""

    hubs: tuple[RecordedHub, ...]

    @classmethod
    def load(cls, path: Path) -> HubSet:
        try:
            text = path.read_text(encoding="utf-8")
        except OSError as e:
            raise ReleaseError(f"cannot read the hub set {path}: {e}") from e
        return cls.parse(text, source=str(path))

    @classmethod
    def parse(cls, text: str, *, source: str) -> HubSet:
        try:
            document = json.loads(text)
        except json.JSONDecodeError as e:
            raise ReleaseError(f"the hub set {source} is not JSON: {e}") from e
        if (
            not isinstance(document, dict)
            or set(document) != {"hubs"}
            or not isinstance(document["hubs"], dict)
            or not document["hubs"]
        ):
            raise ReleaseError(
                f"the hub set {source} must have the shape {_HUB_SET_SHAPE} and "
                "name at least one hub"
            )
        return cls(
            hubs=tuple(
                _parse_recorded_hub(name, entry, source)
                for name, entry in document["hubs"].items()
            )
        )

    def hub(self, name: str) -> RecordedHub:
        for hub in self.hubs:
            if hub.name == name:
                return hub
        names = ", ".join(hub.name for hub in self.hubs)
        raise ReleaseError(f"the hub set names no {name}; it names {names}")

    def compact_json(self) -> str:
        """The set as one line of JSON, in the schema it was read from."""
        document = {
            "hubs": {
                hub.name: {"ref": hub.ref, "commit": hub.commit} for hub in self.hubs
            }
        }
        return json.dumps(document, separators=(",", ":"))


def _parse_recorded_hub(name: str, entry: object, source: str) -> RecordedHub:
    if not _REPOSITORY_NAME_PATTERN.fullmatch(name):
        raise ReleaseError(
            f"the hub set {source} names `{name}`, which is not a repository name"
        )
    if (
        not isinstance(entry, dict)
        or set(entry) != {"ref", "commit"}
        or not all(isinstance(value, str) for value in entry.values())
        or not entry["ref"]
    ):
        raise ReleaseError(
            f"the entry of {name} in the hub set {source} must have the shape "
            f'{{"ref": "<label>", "commit": "<40 hex>"}}, got {json.dumps(entry)}'
        )
    if not _COMMIT_PATTERN.fullmatch(entry["commit"]):
        raise ReleaseError(
            f"the hub set {source} records `{entry['commit']}` for {name}, which "
            "is not a 40-character lowercase hex commit"
        )
    return RecordedHub(name=name, ref=entry["ref"], commit=entry["commit"])


# --- the hub tags ---


def _repository_api(owner: str, repository: str) -> str:
    return f"{_GITHUB_API}/repos/{owner}/{repository}"


@dataclass(frozen=True)
class _GitObject:
    type: str
    sha: str


def _parse_git_object(response: Any, what: str) -> _GitObject:
    """The object a ref or an annotated tag names, from the GitHub API."""
    git_object = response.get("object") if isinstance(response, dict) else None
    if (
        not isinstance(git_object, dict)
        or not isinstance(git_object.get("type"), str)
        or not isinstance(git_object.get("sha"), str)
    ):
        raise ReleaseError(
            f"unexpected GitHub API response for {what} (expected an object with "
            f"a type and a sha): {json.dumps(response)[:500]}"
        )
    return _GitObject(type=git_object["type"], sha=git_object["sha"])


def read_hub_tag_commit(
    client: httpx.Client, owner: str, hub: str, hub_tag: str
) -> str | None:
    """The commit *hub_tag* names in *hub*, or None when the hub has no such tag.

    An annotated tag names a tag object, which names the commit, so the tag is
    followed down to its commit.
    """
    api = _repository_api(owner, hub)
    what = f"the tag {hub_tag} of {hub}"
    ref = github_api(client, "GET", f"{api}/git/ref/tags/{hub_tag}", none_on_404=True)
    if ref is None:
        return None
    git_object = _parse_git_object(ref, what)
    while git_object.type == "tag":
        tag_object = github_api(client, "GET", f"{api}/git/tags/{git_object.sha}")
        git_object = _parse_git_object(tag_object, what)
    if git_object.type != "commit":
        raise ReleaseError(
            f"{what} names a {git_object.type} ({git_object.sha}), not a commit"
        )
    return git_object.sha


def _create_hub_tag(
    client: httpx.Client, owner: str, hub: RecordedHub, hub_tag: str, tag: str
) -> None:
    """Create the annotated tag *hub_tag* of *hub* at its recorded commit."""
    api = _repository_api(owner, hub.name)
    tag_object = github_api(
        client,
        "POST",
        f"{api}/git/tags",
        json_data={
            "tag": hub_tag,
            "message": f"Released with peppy {tag}",
            "object": hub.commit,
            "type": "commit",
        },
    )
    sha = tag_object.get("sha") if isinstance(tag_object, dict) else None
    if not isinstance(sha, str):
        raise ReleaseError(
            f"unexpected GitHub API response for the new tag {hub_tag} of "
            f"{hub.name} (expected its sha): {json.dumps(tag_object)[:500]}"
        )
    github_api(
        client,
        "POST",
        f"{api}/git/refs",
        json_data={"ref": f"refs/tags/{hub_tag}", "sha": sha},
    )


def _tagged_elsewhere_message(
    hub_tag: str, tag: str, misplaced: list[tuple[RecordedHub, str]]
) -> str:
    lines = "\n".join(
        f"  {hub.name}: tagged at {tagged[:12]}, tested at {hub.commit[:12]}"
        for hub, tagged in misplaced
    )
    return (
        f"{hub_tag} already names another commit than the one this run tested "
        f"in these hubs:\n{lines}\n"
        f"This publish tagged no hub and publishes nothing. Nobody moves or "
        f"deletes a hub tag, so a new run of {tag} tests each of these hubs at "
        f"the commit its tag names; if that commit fails the checks, release "
        f"the next patch number instead."
    )


def tag_release_hubs(
    client: httpx.Client, owner: str, hub_set: HubSet, tag: str
) -> None:
    """Put the tag of the release *tag* on every hub of *hub_set*, at the
    commit the set records.

    A hub that already carries the tag at that commit keeps it: an earlier
    attempt of this publish, or an earlier run of the same version, tagged it.
    A hub that carries it at another commit stops the release, and every hub
    is read before any is tagged, so that stop leaves every hub as it was. No
    tag is ever moved or deleted.
    """
    hub_tag = hub_release_tag(tag)
    console.print(f"Reading the tag {hub_tag} of every hub...")
    tagged = {
        hub.name: read_hub_tag_commit(client, owner, hub.name, hub_tag)
        for hub in hub_set.hubs
    }
    misplaced = [
        (hub, tagged[hub.name])
        for hub in hub_set.hubs
        if tagged[hub.name] not in (None, hub.commit)
    ]
    if misplaced:
        raise ReleaseError(_tagged_elsewhere_message(hub_tag, tag, misplaced))

    for hub in hub_set.hubs:
        if tagged[hub.name] == hub.commit:
            console.print(
                f"{hub.name} already carries {hub_tag} at {hub.commit[:12]}; it stays."
            )
            continue
        _create_hub_tag(client, owner, hub, hub_tag, tag)
        console.print(f"Tagged {hub.name} {hub_tag} at {hub.commit[:12]}.")


# --- the launchers-hub run ---


@dataclass(frozen=True)
class DispatchedRun:
    """The launchers-hub run a dispatch started."""

    repository: str
    run_id: int
    html_url: str

    @property
    def api_url(self) -> str:
        return f"{_GITHUB_API}/repos/{self.repository}/actions/runs/{self.run_id}"


@dataclass(frozen=True)
class RunState:
    status: str
    conclusion: str | None


def parse_dispatch_response(response: Any, repository: str) -> DispatchedRun:
    """The run a workflow dispatch started, from the dispatch's response.

    The response names the run it started. Without that, nothing tells which
    run of the workflow is this release's, and the release never guesses it
    from the list of runs.
    """
    run_id = response.get("workflow_run_id") if isinstance(response, dict) else None
    html_url = response.get("html_url") if isinstance(response, dict) else None
    if not isinstance(run_id, int) or isinstance(run_id, bool):
        raise ReleaseError(
            f"the dispatch of {LAUNCHERS_WORKFLOW} in {repository} answered "
            f"without a workflow_run_id, so the run it started is unknown: "
            f"{json.dumps(response)[:500]}"
        )
    if not isinstance(html_url, str) or not html_url:
        html_url = f"https://github.com/{repository}/actions/runs/{run_id}"
    return DispatchedRun(repository=repository, run_id=run_id, html_url=html_url)


def dispatch_launchers_tests(
    client: httpx.Client, owner: str, hub_set: HubSet, peppy_run_id: int
) -> DispatchedRun:
    """Start launchers-hub's tests on *hub_set*, with the peppy archive of the
    release run *peppy_run_id*."""
    repository = f"{owner}/{hub_set.hub(LAUNCHERS_HUB).name}"
    response = github_api(
        client,
        "POST",
        f"{_GITHUB_API}/repos/{repository}/actions/workflows/"
        f"{LAUNCHERS_WORKFLOW}/dispatches",
        json_data={
            "ref": LAUNCHERS_WORKFLOW_BRANCH,
            "inputs": {
                "peppy-run-id": str(peppy_run_id),
                "set": hub_set.compact_json(),
            },
        },
    )
    return parse_dispatch_response(response, repository)


def read_run_state(client: httpx.Client, run: DispatchedRun) -> RunState:
    response = github_api(client, "GET", run.api_url)
    status = response.get("status") if isinstance(response, dict) else None
    conclusion = response.get("conclusion") if isinstance(response, dict) else None
    if not isinstance(status, str) or not (
        conclusion is None or isinstance(conclusion, str)
    ):
        raise ReleaseError(
            f"unexpected GitHub API response for the run {run.html_url} "
            f"(expected its status and conclusion): {json.dumps(response)[:500]}"
        )
    return RunState(status=status, conclusion=conclusion)


def wait_for_run(
    read_state: Callable[[], RunState],
    run: DispatchedRun,
    *,
    sleep: Callable[[float], None],
    clock: Callable[[], float],
    poll_interval: float = RUN_POLL_INTERVAL_SECONDS,
    timeout: float = RUN_WAIT_TIMEOUT_SECONDS,
    max_failed_reads: int = MAX_FAILED_RUN_READS,
) -> RunState:
    """Read *run* every *poll_interval* seconds until it completes, and return
    its completed state.

    The log names each status the run moves to. The wait stops with an error
    once *timeout* seconds have passed, or once *max_failed_reads* reads in a
    row failed. *sleep* and *clock* are `time.sleep` and `time.monotonic`
    outside the tests.
    """
    deadline = clock() + timeout
    failed_reads = 0
    reported_status: str | None = None
    while True:
        try:
            state = read_state()
        except ReleaseError as e:
            failed_reads += 1
            if failed_reads >= max_failed_reads:
                raise ReleaseError(
                    f"reading the run {run.html_url} failed {failed_reads} times "
                    f"in a row; the last failure: {e}"
                ) from e
            console.print(
                f"[yellow]Reading the run failed ({failed_reads} of "
                f"{max_failed_reads} in a row); reading it again at the next "
                f"poll.[/yellow]"
            )
        else:
            failed_reads = 0
            if state.status == "completed":
                return state
            if state.status != reported_status:
                console.print(f"The run is {state.status}.")
                reported_status = state.status
        if clock() >= deadline:
            raise ReleaseError(
                f"the run {run.html_url} did not complete within "
                f"{timeout / 60:.0f} minutes. It goes on without this release, "
                f"which publishes nothing."
            )
        sleep(poll_interval)


def require_successful_run(state: RunState, run: DispatchedRun) -> None:
    if state.conclusion == "success":
        return
    raise ReleaseError(
        f"the launchers-hub run {run.html_url} concluded "
        f"`{state.conclusion}`: the launchers do not all launch with the peppy "
        f"and the hubs of this release, so nothing is published. Its summary "
        f"names every combination that failed."
    )


def check_launchers(
    dispatch_client: httpx.Client,
    read_client: httpx.Client,
    owner: str,
    hub_set: HubSet,
    peppy_run_id: int,
    *,
    sleep: Callable[[float], None],
    clock: Callable[[], float],
) -> DispatchedRun:
    """Run launchers-hub's tests on *hub_set* and the archive of the release
    run *peppy_run_id*, and return the run once it succeeded.

    *dispatch_client* starts the run. *read_client* reads it until it
    completes, which takes longer than the hour a release token lasts.
    """
    run = dispatch_launchers_tests(dispatch_client, owner, hub_set, peppy_run_id)
    console.print(
        f"Dispatched {LAUNCHERS_WORKFLOW} of {run.repository}: {run.html_url}",
        soft_wrap=True,
    )
    state = wait_for_run(
        lambda: read_run_state(read_client, run), run, sleep=sleep, clock=clock
    )
    require_successful_run(state, run)
    return run
