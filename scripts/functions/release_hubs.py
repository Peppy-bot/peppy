"""The hubs a peppy release tests, and tags once it publishes.

The hub-set job of .github/workflows/parallel-release.yml records the hub set
of the release with .github/actions/hub-ci-peppy/resolve.py: every hub, each at
the commit of its tag of this release (`peppy-release/<tag>`) where it carries
one, and at the head of its `main` where it does not. The set is an explicit
set, the schema launchers-hub's tests take as their `set` input, and the
stages read it with the parser of resolve.py (see hub_ci.py). Every later
stage reads the commits the set records and never a branch, since `main` can
move while the release runs.

Two stages use the set here:

- `hub-launch` dispatches launchers-hub's tests on the set and on the archive
  of this release run, and waits for that run to succeed.
- `publish` puts the tag of the release on every hub, at the commit the set
  records, before it publishes the release.
"""

from __future__ import annotations

import json
from collections.abc import Callable
from dataclasses import dataclass
from typing import Any

import httpx

from .cli import ReleaseError, console
from .github import RepoSlug, github_api
from .hub_ci import resolve

# The hub whose tests launch every launcher of the set, and the workflow that
# runs them. The workflow runs as `main` defines it, and checks out the
# launchers-hub commit of the set.
LAUNCHERS_HUB = resolve.HUBS_BY_NAME["launchers-hub"]
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


def hub_repository(owner: str, hub: resolve.Hub) -> RepoSlug:
    """The repository of *hub* under *owner*, the owner of the release."""
    return RepoSlug(owner=owner, repo=hub.name)


# --- the hub tags ---


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
    client: httpx.Client, repository: RepoSlug, hub_tag: str
) -> str | None:
    """The commit *hub_tag* names in *repository*, or None when the hub has no
    such tag.

    An annotated tag names a tag object, which names the commit, so the tag is
    followed down to its commit.
    """
    what = f"the tag {hub_tag} of {repository.repo}"
    ref = github_api(
        client,
        "GET",
        f"{repository.api_url}/git/ref/tags/{hub_tag}",
        none_on_404=True,
    )
    if ref is None:
        return None
    git_object = _parse_git_object(ref, what)
    while git_object.type == "tag":
        tag_object = github_api(
            client, "GET", f"{repository.api_url}/git/tags/{git_object.sha}"
        )
        git_object = _parse_git_object(tag_object, what)
    if git_object.type != "commit":
        raise ReleaseError(
            f"{what} names a {git_object.type} ({git_object.sha}), not a commit"
        )
    return git_object.sha


def _create_hub_tag(
    client: httpx.Client, repository: RepoSlug, commit: str, hub_tag: str, tag: str
) -> None:
    """Create the annotated tag *hub_tag* of *repository* at *commit*."""
    tag_object = github_api(
        client,
        "POST",
        f"{repository.api_url}/git/tags",
        json_data={
            "tag": hub_tag,
            "message": f"Released with peppy {tag}",
            "object": commit,
            "type": "commit",
        },
    )
    sha = tag_object.get("sha") if isinstance(tag_object, dict) else None
    if not isinstance(sha, str):
        raise ReleaseError(
            f"unexpected GitHub API response for the new tag {hub_tag} of "
            f"{repository.repo} (expected its sha): {json.dumps(tag_object)[:500]}"
        )
    github_api(
        client,
        "POST",
        f"{repository.api_url}/git/refs",
        json_data={"ref": f"refs/tags/{hub_tag}", "sha": sha},
    )


def _tagged_elsewhere_message(
    hub_tag: str, tag: str, misplaced: list[tuple[resolve.ResolvedHub, str]]
) -> str:
    lines = "\n".join(
        f"  {resolved.hub.name}: tagged at {tagged[:12]}, tested at "
        f"{resolved.commit[:12]}"
        for resolved, tagged in misplaced
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
    client: httpx.Client, owner: str, hub_set: resolve.HubSet, tag: str
) -> None:
    """Put the tag of the release *tag* on every hub of *hub_set*, at the
    commit the set records.

    A hub that already carries the tag at that commit keeps it: an earlier
    attempt of this publish, or an earlier run of the same version, tagged it.
    A hub that carries it at another commit stops the release, and every hub
    is read before any is tagged, so that stop leaves every hub as it was. No
    tag is ever moved or deleted.
    """
    hub_tag = resolve.hub_release_tag(tag)
    console.print(f"Reading the tag {hub_tag} of every hub...")
    tagged = {
        resolved.hub: read_hub_tag_commit(
            client, hub_repository(owner, resolved.hub), hub_tag
        )
        for resolved in hub_set.hubs
    }
    misplaced = [
        (resolved, tagged[resolved.hub])
        for resolved in hub_set.hubs
        if tagged[resolved.hub] not in (None, resolved.commit)
    ]
    if misplaced:
        raise ReleaseError(_tagged_elsewhere_message(hub_tag, tag, misplaced))

    for resolved in hub_set.hubs:
        at_commit = f"{hub_tag} at {resolved.commit[:12]}"
        if tagged[resolved.hub] == resolved.commit:
            console.print(f"{resolved.hub.name} already carries {at_commit}; it stays.")
            continue
        repository = hub_repository(owner, resolved.hub)
        _create_hub_tag(client, repository, resolved.commit, hub_tag, tag)
        console.print(f"Tagged {resolved.hub.name} {at_commit}.")


# --- the launchers-hub run ---


@dataclass(frozen=True)
class DispatchedRun:
    """The launchers-hub run a dispatch started."""

    repository: RepoSlug
    run_id: int

    @property
    def api_url(self) -> str:
        return f"{self.repository.api_url}/actions/runs/{self.run_id}"

    @property
    def html_url(self) -> str:
        return f"https://github.com/{self.repository.full}/actions/runs/{self.run_id}"


@dataclass(frozen=True)
class RunState:
    status: str
    conclusion: str | None


def parse_dispatch_response(response: Any, repository: RepoSlug) -> DispatchedRun:
    """The run a workflow dispatch started, from the dispatch's response.

    The response names the run it started. Without that, nothing tells which
    run of the workflow is this release's, and the release never guesses it
    from the list of runs.
    """
    run_id = response.get("workflow_run_id") if isinstance(response, dict) else None
    if not isinstance(run_id, int) or isinstance(run_id, bool):
        raise ReleaseError(
            f"the dispatch of {LAUNCHERS_WORKFLOW} in {repository.full} answered "
            f"without a workflow_run_id, so the run it started is unknown: "
            f"{json.dumps(response)[:500]}"
        )
    return DispatchedRun(repository=repository, run_id=run_id)


def dispatch_launchers_tests(
    client: httpx.Client, owner: str, hub_set: resolve.HubSet, peppy_run_id: int
) -> DispatchedRun:
    """Start launchers-hub's tests on *hub_set*, with the peppy archive of the
    release run *peppy_run_id*. The set names every hub, launchers-hub among
    them."""
    repository = hub_repository(owner, LAUNCHERS_HUB)
    response = github_api(
        client,
        "POST",
        f"{repository.api_url}/actions/workflows/{LAUNCHERS_WORKFLOW}/dispatches",
        json_data={
            "ref": LAUNCHERS_WORKFLOW_BRANCH,
            "inputs": {
                "peppy-run-id": str(peppy_run_id),
                "set": resolve.compact_json(resolve.release_set_document(hub_set)),
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
    hub_set: resolve.HubSet,
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
        f"Dispatched {LAUNCHERS_WORKFLOW} of {run.repository.full}: {run.html_url}",
        soft_wrap=True,
    )
    state = wait_for_run(
        lambda: read_run_state(read_client, run), run, sleep=sleep, clock=clock
    )
    require_successful_run(state, run)
    return run
