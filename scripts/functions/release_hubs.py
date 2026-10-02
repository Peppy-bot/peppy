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
  of this release run, and waits for the last attempt of that run to succeed:
  an attempt that AWS interrupted is followed by the one RunsOn runs again.
- `publish` puts the tag of the release on every hub, at the commit the set
  records, before it publishes the release.
"""

from __future__ import annotations

import json
import re
from collections.abc import Callable
from dataclasses import dataclass
from typing import Any, TypeGuard, TypeVar

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

# The jobs of the launchers-hub run are on RunsOn spot instances in attempts 1
# and 2 of the run. RunsOn marks a job whose instance AWS interrupted with an
# annotation of this title. When such an attempt concludes `failure`, RunsOn
# runs the failed jobs of the run again in the next attempt of the same run.
# It does this after attempt 1 and after attempt 2, never after a later one
# (https://runs-on.com/docs/costs/spot-pricing/).
SPOT_INTERRUPTION_ANNOTATION_TITLE = "EC2 Spot interruption"
LAST_ATTEMPT_RUNS_ON_RETRIES = 2

# How often the wait reads the launchers-hub run. One attempt takes up to about
# 75 minutes, and the wait gives up after RUN_WAIT_TIMEOUT_SECONDS: three
# attempts and the start of the two retries, inside the 265 minutes the
# hub-launch job has.
RUN_POLL_INTERVAL_SECONDS = 60.0
RUN_WAIT_TIMEOUT_SECONDS = 255 * 60.0
# RunsOn starts the next attempt a minute or two after the interrupted one
# completes. The wait gives up when the next attempt has not started this long
# after the wait saw the interrupted one complete.
RETRY_START_TIMEOUT_SECONDS = 15 * 60.0
# A read of the run that fails (a GitHub API error, a dropped connection) is
# tried again at the next poll, so one failure does not throw away a run that
# is well on its way. This many failures in a row end the wait.
MAX_FAILED_RUN_READS = 5
# The page size of the lists the wait reads (the jobs of an attempt, the
# annotations of a job), the largest GitHub serves.
API_PAGE_SIZE = 100


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
    """One read of the run. GitHub shows a run as its latest attempt: the
    number of that attempt, and the status and conclusion of that attempt."""

    attempt: int
    status: str
    conclusion: str | None


@dataclass(frozen=True)
class AttemptJob:
    """A job of one attempt of the run, and the check run that holds the
    annotations of that job."""

    name: str
    conclusion: str | None
    check_run_id: int


@dataclass(frozen=True)
class SpotRetry:
    """RunsOn runs the failed jobs of the run again after *attempt*, since AWS
    interrupted the spot instances of *interrupted_jobs*."""

    attempt: int
    interrupted_jobs: tuple[str, ...]

    @property
    def next_attempt(self) -> int:
        return self.attempt + 1


@dataclass(frozen=True)
class RunPolling:
    """How the wait reads the run: the sleep and the clock it uses
    (`time.sleep` and `time.monotonic` outside the tests), how often it reads,
    when it gives up, and how many failed reads in a row it accepts."""

    sleep: Callable[[float], None]
    clock: Callable[[], float]
    interval: float = RUN_POLL_INTERVAL_SECONDS
    timeout: float = RUN_WAIT_TIMEOUT_SECONDS
    retry_start_timeout: float = RETRY_START_TIMEOUT_SECONDS
    max_failed_reads: int = MAX_FAILED_RUN_READS


def _is_positive_int(value: object) -> TypeGuard[int]:
    # bool is a subclass of int, and a JSON `true` is no number.
    return isinstance(value, int) and not isinstance(value, bool) and value > 0


def _is_optional_str(value: object) -> TypeGuard[str | None]:
    return value is None or isinstance(value, str)


def parse_dispatch_response(response: Any, repository: RepoSlug) -> DispatchedRun:
    """The run a workflow dispatch started, from the dispatch's response.

    The response names the run it started. Without that, nothing tells which
    run of the workflow is this release's, and the release never guesses it
    from the list of runs.
    """
    run_id = response.get("workflow_run_id") if isinstance(response, dict) else None
    if not _is_positive_int(run_id):
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
            # Without it, GitHub answers 204 with no body, which names no run.
            "return_run_details": True,
        },
    )
    return parse_dispatch_response(response, repository)


def parse_run_state(response: Any, run: DispatchedRun) -> RunState:
    """The state of *run* from GitHub's answer to a read of the run."""
    fields = response if isinstance(response, dict) else {}
    attempt = fields.get("run_attempt")
    status = fields.get("status")
    conclusion = fields.get("conclusion")
    if (
        not _is_positive_int(attempt)
        or not isinstance(status, str)
        or not _is_optional_str(conclusion)
    ):
        raise ReleaseError(
            f"unexpected GitHub API response for the run {run.html_url} "
            f"(expected its attempt, status and conclusion): "
            f"{json.dumps(response)[:500]}"
        )
    return RunState(attempt=attempt, status=status, conclusion=conclusion)


def read_run_state(client: httpx.Client, run: DispatchedRun) -> RunState:
    return parse_run_state(github_api(client, "GET", run.api_url), run)


_Item = TypeVar("_Item")


def _read_all_pages(
    client: httpx.Client, url: str, parse_page: Callable[[Any], list[_Item]]
) -> list[_Item]:
    """Every item of the GitHub API list at *url*, read API_PAGE_SIZE at a
    time. *parse_page* reads the items of one page."""
    items: list[_Item] = []
    page = 1
    while True:
        page_items = parse_page(
            github_api(client, "GET", f"{url}?per_page={API_PAGE_SIZE}&page={page}")
        )
        items.extend(page_items)
        if len(page_items) < API_PAGE_SIZE:
            return items
        page += 1


def _check_run_id(check_run_url: Any, repository: RepoSlug) -> int | None:
    """The id of the check run at *check_run_url*, when that is a check run of
    *repository*."""
    if not isinstance(check_run_url, str):
        return None
    found = re.fullmatch(
        rf"{re.escape(repository.api_url)}/check-runs/([1-9][0-9]*)", check_run_url
    )
    return int(found.group(1)) if found else None


def _parse_attempt_job(job: Any, run: DispatchedRun) -> AttemptJob:
    fields = job if isinstance(job, dict) else {}
    name = fields.get("name")
    conclusion = fields.get("conclusion")
    check_run_id = _check_run_id(fields.get("check_run_url"), run.repository)
    if (
        not isinstance(name, str)
        or not _is_optional_str(conclusion)
        or check_run_id is None
    ):
        raise ReleaseError(
            f"unexpected GitHub API response for a job of the run {run.html_url} "
            f"(expected its name, its conclusion and its check run in "
            f"{run.repository.full}): {json.dumps(job)[:500]}"
        )
    return AttemptJob(name=name, conclusion=conclusion, check_run_id=check_run_id)


def _parse_attempt_jobs(response: Any, run: DispatchedRun) -> list[AttemptJob]:
    jobs = response.get("jobs") if isinstance(response, dict) else None
    if not isinstance(jobs, list):
        raise ReleaseError(
            f"unexpected GitHub API response for the jobs of the run "
            f"{run.html_url} (expected a list of jobs): {json.dumps(response)[:500]}"
        )
    return [_parse_attempt_job(job, run) for job in jobs]


def _parse_annotation_titles(response: Any, check_run_url: str) -> list[str | None]:
    """The titles of a page of annotations; None for an annotation without
    one."""
    if not isinstance(response, list) or not all(
        isinstance(annotation, dict) and _is_optional_str(annotation.get("title"))
        for annotation in response
    ):
        raise ReleaseError(
            f"unexpected GitHub API response for the annotations of "
            f"{check_run_url} (expected a list of annotations): "
            f"{json.dumps(response)[:500]}"
        )
    return [annotation.get("title") for annotation in response]


def _was_spot_interrupted(
    client: httpx.Client, repository: RepoSlug, job: AttemptJob
) -> bool:
    check_run_url = f"{repository.api_url}/check-runs/{job.check_run_id}"
    titles = _read_all_pages(
        client,
        f"{check_run_url}/annotations",
        lambda page: _parse_annotation_titles(page, check_run_url),
    )
    return SPOT_INTERRUPTION_ANNOTATION_TITLE in titles


def read_interrupted_jobs(
    client: httpx.Client, run: DispatchedRun, attempt: int
) -> tuple[str, ...]:
    """The names of the failed jobs of attempt *attempt* of *run* whose spot
    instance AWS interrupted: the failed jobs that RunsOn marked with an
    annotation titled SPOT_INTERRUPTION_ANNOTATION_TITLE."""
    jobs = _read_all_pages(
        client,
        f"{run.api_url}/attempts/{attempt}/jobs",
        lambda page: _parse_attempt_jobs(page, run),
    )
    return tuple(
        job.name
        for job in jobs
        if job.conclusion == "failure"
        and _was_spot_interrupted(client, run.repository, job)
    )


_Answer = TypeVar("_Answer")


def _read_until_it_answers(
    read: Callable[[], _Answer], subject: str, polling: RunPolling
) -> _Answer:
    """The answer of *read*, which reads *subject*.

    A read that fails (a GitHub API error, a dropped connection) is tried again
    at the next poll, and polling.max_failed_reads failures in a row end the
    wait.
    """
    failed_reads = 0
    while True:
        try:
            return read()
        except ReleaseError as e:
            failed_reads += 1
            if failed_reads >= polling.max_failed_reads:
                raise ReleaseError(
                    f"reading {subject} failed {failed_reads} times in a row; "
                    f"the last failure: {e}"
                ) from e
            console.print(
                f"[yellow]Reading {subject} failed ({failed_reads} of "
                f"{polling.max_failed_reads} in a row); reading it again at the "
                f"next poll.[/yellow]",
                soft_wrap=True,
            )
            polling.sleep(polling.interval)


def _poll_run(
    read_state: Callable[[], RunState],
    run: DispatchedRun,
    polling: RunPolling,
    *,
    until: Callable[[RunState], bool],
    deadline: float,
) -> RunState | None:
    """Read *run* every polling.interval seconds until a read satisfies
    *until*, and return that read; None when the clock passes *deadline*
    first. The log names each attempt and status the run moves to."""
    reported: tuple[int, str] | None = None
    while True:
        state = _read_until_it_answers(read_state, f"the run {run.html_url}", polling)
        if until(state):
            return state
        if (state.attempt, state.status) != reported:
            console.print(f"Attempt {state.attempt} of the run is {state.status}.")
            reported = (state.attempt, state.status)
        if polling.clock() >= deadline:
            return None
        polling.sleep(polling.interval)


def _wait_timeout_error(run: DispatchedRun, polling: RunPolling) -> ReleaseError:
    return ReleaseError(
        f"the run {run.html_url} did not complete within "
        f"{polling.timeout / 60:.0f} minutes. It goes on without this release, "
        f"which publishes nothing."
    )


def _wait_for_attempt_to_complete(
    read_state: Callable[[], RunState],
    run: DispatchedRun,
    polling: RunPolling,
    *,
    attempt: int,
    deadline: float,
) -> RunState:
    """The completed state of attempt *attempt* of *run*, or of a later
    attempt that started before the wait saw *attempt* complete."""
    state = _poll_run(
        read_state,
        run,
        polling,
        until=lambda state: state.attempt >= attempt and state.status == "completed",
        deadline=deadline,
    )
    if state is None:
        raise _wait_timeout_error(run, polling)
    return state


def _wait_for_attempt_to_start(
    read_state: Callable[[], RunState],
    run: DispatchedRun,
    polling: RunPolling,
    *,
    attempt: int,
    deadline: float,
) -> None:
    """Wait for attempt *attempt* of *run*, or a later one, to start, for at
    most polling.retry_start_timeout seconds."""
    start_deadline = polling.clock() + polling.retry_start_timeout
    started = _poll_run(
        read_state,
        run,
        polling,
        until=lambda state: state.attempt >= attempt,
        deadline=min(deadline, start_deadline),
    )
    if started is not None:
        return
    if deadline < start_deadline:
        raise _wait_timeout_error(run, polling)
    raise ReleaseError(
        f"RunsOn did not start attempt {attempt} of the run {run.html_url} "
        f"within {polling.retry_start_timeout / 60:.0f} minutes of the end of "
        f"attempt {attempt - 1}, whose spot instance AWS interrupted, so nothing "
        f"is published. Check that RunsOn retries interrupted jobs (its `retry` "
        f"setting) and that its control plane is up."
    )


def _spot_retry_after(
    state: RunState,
    read_interrupted_jobs: Callable[[int], tuple[str, ...]],
    run: DispatchedRun,
    polling: RunPolling,
) -> SpotRetry | None:
    """The retry RunsOn runs after the completed attempt *state*, or None when
    that attempt is the last one of the run.

    RunsOn runs the failed jobs of the run again after an attempt that
    concluded `failure` with a job whose spot instance AWS interrupted, after
    attempt LAST_ATTEMPT_RUNS_ON_RETRIES at the latest.
    """
    if state.conclusion != "failure" or state.attempt > LAST_ATTEMPT_RUNS_ON_RETRIES:
        return None
    interrupted_jobs = _read_until_it_answers(
        lambda: read_interrupted_jobs(state.attempt),
        f"the jobs of attempt {state.attempt} of the run {run.html_url}",
        polling,
    )
    if not interrupted_jobs:
        return None
    return SpotRetry(attempt=state.attempt, interrupted_jobs=interrupted_jobs)


def wait_for_last_attempt(
    read_state: Callable[[], RunState],
    read_interrupted_jobs: Callable[[int], tuple[str, ...]],
    run: DispatchedRun,
    polling: RunPolling,
) -> RunState:
    """Wait until the last attempt of *run* completes, and return its
    completed state.

    An attempt is not the last one when RunsOn runs the failed jobs of the run
    again after it (see _spot_retry_after). The wait then waits for the next
    attempt to start, for at most polling.retry_start_timeout seconds, and to
    complete. When the next attempt starts between two reads, the wait never
    sees the attempt before it complete, and follows the next one. The whole
    wait, every attempt included, gives up after polling.timeout seconds.
    """
    deadline = polling.clock() + polling.timeout
    attempt = 1
    while True:
        state = _wait_for_attempt_to_complete(
            read_state, run, polling, attempt=attempt, deadline=deadline
        )
        retry = _spot_retry_after(state, read_interrupted_jobs, run, polling)
        if retry is None:
            return state
        interrupted_jobs = ", ".join(f"`{name}`" for name in retry.interrupted_jobs)
        console.print(
            f"[yellow]AWS interrupted the spot instance of {interrupted_jobs} in "
            f"attempt {retry.attempt} of the run. RunsOn runs the failed jobs of "
            f"the run again in attempt {retry.next_attempt}, and the wait follows "
            f"that attempt.[/yellow]",
            soft_wrap=True,
        )
        _wait_for_attempt_to_start(
            read_state, run, polling, attempt=retry.next_attempt, deadline=deadline
        )
        attempt = retry.next_attempt


def require_successful_run(state: RunState, run: DispatchedRun) -> None:
    if state.conclusion == "success":
        return
    raise ReleaseError(
        f"attempt {state.attempt} of the launchers-hub run {run.html_url} "
        f"concluded `{state.conclusion}`: the launchers do not all launch with "
        f"the peppy and the hubs of this release, so nothing is published. Its "
        f"summary names every combination that failed."
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
    run *peppy_run_id*, and return the run once its last attempt succeeded.

    *dispatch_client* starts the run. *read_client* reads it until its last
    attempt completes, which takes longer than the hour a release token lasts.
    *sleep* and *clock* are `time.sleep` and `time.monotonic` outside the
    tests.
    """
    run = dispatch_launchers_tests(dispatch_client, owner, hub_set, peppy_run_id)
    console.print(
        f"Dispatched {LAUNCHERS_WORKFLOW} of {run.repository.full}: {run.html_url}",
        soft_wrap=True,
    )
    state = wait_for_last_attempt(
        lambda: read_run_state(read_client, run),
        lambda attempt: read_interrupted_jobs(read_client, run, attempt),
        run,
        RunPolling(sleep=sleep, clock=clock),
    )
    require_successful_run(state, run)
    return run
