"""Tests for functions.release_hubs.

The GitHub API is a respx stand-in and the wait's sleep and clock are fakes,
so no case touches the network or sleeps. The tag steps of a whole publish,
in their order, are in test_parallel_release.py. The hub set, its parser and
the hub tag of a release are resolve.py's, whose tests
(.github/actions/hub-ci-peppy/test_resolve.py) also hold the tag peppy reads
and the tokens of the release workflow; they run on every change.
"""

from __future__ import annotations

import json
import re
from collections.abc import Callable, Iterator

import httpx
import pytest
import respx

from functions.cli import ReleaseError
from functions.github import RepoSlug
from functions.hub_ci import resolve
from functions.release_hubs import (
    DispatchedRun,
    RunPolling,
    RunState,
    check_launchers,
    parse_dispatch_response,
    parse_run_state,
    read_hub_tag_commit,
    read_interrupted_jobs,
    require_successful_run,
    wait_for_last_attempt,
)

from .helpers import FakeTime, hub_set_document, unwrapped

OWNER = "test-owner"
HUB_TAG = "peppy-release/v0.3.0"
# The hub set the hub-set job records, launchers-hub at its tag of the release.
HUB_SET_DOCUMENT = hub_set_document({"launchers-hub": HUB_TAG})
HUB_SET = resolve.parse_release_set(json.dumps(HUB_SET_DOCUMENT))
RUN = DispatchedRun(repository=RepoSlug(OWNER, "launchers-hub"), run_id=4242)


# --- the hub tags ---

NODES_HUB = RepoSlug(OWNER, "nodes-hub")
HUB_API = NODES_HUB.api_url


def test_a_hub_without_the_tag_has_no_tag_commit(
    mock_api: respx.MockRouter, github_client: httpx.Client
) -> None:
    mock_api.get(f"{HUB_API}/git/ref/tags/{HUB_TAG}").mock(
        return_value=httpx.Response(404, json={"message": "Not Found"})
    )

    assert read_hub_tag_commit(github_client, NODES_HUB, HUB_TAG) is None


def test_a_tag_is_followed_down_to_its_commit(
    mock_api: respx.MockRouter, github_client: httpx.Client
) -> None:
    # A tag of a tag of a commit.
    mock_api.get(f"{HUB_API}/git/ref/tags/{HUB_TAG}").mock(
        return_value=httpx.Response(
            200, json={"object": {"type": "tag", "sha": "1" * 40}}
        )
    )
    mock_api.get(f"{HUB_API}/git/tags/{'1' * 40}").mock(
        return_value=httpx.Response(
            200, json={"object": {"type": "tag", "sha": "2" * 40}}
        )
    )
    mock_api.get(f"{HUB_API}/git/tags/{'2' * 40}").mock(
        return_value=httpx.Response(
            200, json={"object": {"type": "commit", "sha": "3" * 40}}
        )
    )

    assert read_hub_tag_commit(github_client, NODES_HUB, HUB_TAG) == "3" * 40


def test_a_tag_that_names_no_commit_is_refused(
    mock_api: respx.MockRouter, github_client: httpx.Client
) -> None:
    mock_api.get(f"{HUB_API}/git/ref/tags/{HUB_TAG}").mock(
        return_value=httpx.Response(
            200, json={"object": {"type": "tree", "sha": "4" * 40}}
        )
    )

    with pytest.raises(ReleaseError, match="names a tree"):
        read_hub_tag_commit(github_client, NODES_HUB, HUB_TAG)


def test_a_malformed_ref_response_is_refused(
    mock_api: respx.MockRouter, github_client: httpx.Client
) -> None:
    mock_api.get(f"{HUB_API}/git/ref/tags/{HUB_TAG}").mock(
        return_value=httpx.Response(200, json=[{"ref": f"refs/tags/{HUB_TAG}"}])
    )

    with pytest.raises(ReleaseError, match="unexpected GitHub API response"):
        read_hub_tag_commit(github_client, NODES_HUB, HUB_TAG)


# --- the launchers-hub run ---

DISPATCH_URL = f"{RUN.repository.api_url}/actions/workflows/tests.yml/dispatches"
CHECK_RUNS_URL = f"{RUN.repository.api_url}/check-runs"
LAUNCH_JOB = "Launch the planned combinations"
# The annotations of a launch job that AWS interrupted, as RunsOn and the
# runner write them.
SPOT_ANNOTATION = {
    "annotation_level": "failure",
    "title": "EC2 Spot interruption",
    "message": (
        "AWS interrupted the EC2 Spot instance running this job. RunsOn can "
        "retry it after the workflow run finishes."
    ),
}
EXIT_ANNOTATION = {
    "annotation_level": "failure",
    "title": "",
    "message": "Process completed with exit code 1.",
}


def _dispatch_response(run_id: int = RUN.run_id) -> dict:
    return {
        "workflow_run_id": run_id,
        "run_url": f"{RUN.repository.api_url}/actions/runs/{run_id}",
        "html_url": f"https://github.com/{OWNER}/launchers-hub/actions/runs/{run_id}",
    }


def _run_response(attempt: int, status: str, conclusion: str | None = None) -> dict:
    return {
        "id": RUN.run_id,
        "run_attempt": attempt,
        "status": status,
        "conclusion": conclusion,
    }


def _job(
    job_id: int,
    name: str,
    conclusion: str | None,
    repository: RepoSlug = RUN.repository,
) -> dict:
    return {
        "id": job_id,
        "name": name,
        "status": "completed",
        "conclusion": conclusion,
        "check_run_url": f"{repository.api_url}/check-runs/{job_id}",
    }


# The jobs of an attempt that AWS interrupted: the launch job failed on the
# interrupted instance, and `test`, which reads the results of the others,
# failed after it.
INTERRUPTED_ATTEMPT_JOBS = {
    "total_count": 4,
    "jobs": [
        _job(11, "Wait for the peppy dev build", "success"),
        _job(12, "Discover and plan the launcher combinations", "success"),
        _job(13, LAUNCH_JOB, "failure"),
        _job(14, "test", "failure"),
    ],
}


def _states(*states: RunState | ReleaseError) -> Callable[[], RunState]:
    """A read of the run that answers *states* in turn. A read past the last
    one raises StopIteration, so a case also proves the wait reads no more."""
    answers: Iterator[RunState | ReleaseError] = iter(states)

    def read() -> RunState:
        answer = next(answers)
        if isinstance(answer, ReleaseError):
            raise answer
        return answer

    return read


class _Interruptions:
    """A read of the interrupted jobs of an attempt: the launch job in each
    attempt of *attempts*, no job in any other. It records each attempt it
    reads."""

    def __init__(self, *attempts: int) -> None:
        self.attempts = attempts
        self.reads: list[int] = []

    def __call__(self, attempt: int) -> tuple[str, ...]:
        self.reads.append(attempt)
        return (LAUNCH_JOB,) if attempt in self.attempts else ()


def _polling(
    time: FakeTime,
    *,
    timeout: float = 3600.0,
    retry_start_timeout: float = 900.0,
    max_failed_reads: int = 3,
) -> RunPolling:
    """The polling of the wait on *time*, one read every 60 seconds."""
    return RunPolling(
        sleep=time.sleep,
        clock=time.clock,
        interval=60.0,
        timeout=timeout,
        retry_start_timeout=retry_start_timeout,
        max_failed_reads=max_failed_reads,
    )


def test_the_dispatch_response_names_the_run() -> None:
    assert parse_dispatch_response(_dispatch_response(), RUN.repository) == RUN


@pytest.mark.parametrize(
    "response",
    [
        # A 204 without a body, which github_api answers as {}.
        {},
        {"run_url": "https://api.github.com/x", "html_url": "https://github.com/x"},
        {"workflow_run_id": "4242"},
        {"workflow_run_id": True},
        {"workflow_run_id": 0},
        None,
    ],
)
def test_a_dispatch_response_without_the_run_id_is_refused(response: object) -> None:
    with pytest.raises(ReleaseError) as excinfo:
        parse_dispatch_response(response, RUN.repository)

    assert "answered without a workflow_run_id, so the run it started is unknown" in (
        unwrapped(str(excinfo.value))
    )


def test_a_dispatched_run_has_its_page_and_its_api_url() -> None:
    run = parse_dispatch_response({"workflow_run_id": 7}, RUN.repository)

    assert run.html_url == f"https://github.com/{OWNER}/launchers-hub/actions/runs/7"
    assert (
        run.api_url
        == f"https://api.github.com/repos/{OWNER}/launchers-hub/actions/runs/7"
    )


def test_a_read_of_the_run_names_its_latest_attempt() -> None:
    assert parse_run_state(_run_response(2, "completed", "success"), RUN) == (
        RunState(attempt=2, status="completed", conclusion="success")
    )


@pytest.mark.parametrize(
    "response",
    [
        {"status": "queued", "conclusion": None},
        {"run_attempt": 0, "status": "queued", "conclusion": None},
        {"run_attempt": True, "status": "queued", "conclusion": None},
        {"run_attempt": "1", "status": "queued", "conclusion": None},
        {"run_attempt": 1, "conclusion": None},
        {"run_attempt": 1, "status": "completed", "conclusion": 1},
        [_run_response(1, "queued")],
    ],
)
def test_a_malformed_read_of_the_run_is_refused(response: object) -> None:
    with pytest.raises(ReleaseError) as excinfo:
        parse_run_state(response, RUN)

    assert (
        f"unexpected GitHub API response for the run {RUN.html_url} (expected "
        f"its attempt, status and conclusion)"
    ) in unwrapped(str(excinfo.value))


# --- the interrupted jobs of an attempt ---


def test_the_interrupted_jobs_are_the_failed_jobs_runs_on_marked(
    mock_api: respx.MockRouter, github_client: httpx.Client
) -> None:
    mock_api.get(f"{RUN.api_url}/attempts/1/jobs").mock(
        return_value=httpx.Response(200, json=INTERRUPTED_ATTEMPT_JOBS)
    )
    mock_api.get(f"{CHECK_RUNS_URL}/13/annotations").mock(
        return_value=httpx.Response(200, json=[EXIT_ANNOTATION, SPOT_ANNOTATION])
    )
    mock_api.get(f"{CHECK_RUNS_URL}/14/annotations").mock(
        return_value=httpx.Response(200, json=[EXIT_ANNOTATION])
    )

    # The annotations of the jobs that succeeded are never read: respx refuses
    # a request no route matches.
    assert read_interrupted_jobs(github_client, RUN, 1) == (LAUNCH_JOB,)


def test_an_attempt_without_the_mark_of_runs_on_has_no_interrupted_job(
    mock_api: respx.MockRouter, github_client: httpx.Client
) -> None:
    mock_api.get(f"{RUN.api_url}/attempts/1/jobs").mock(
        return_value=httpx.Response(200, json=INTERRUPTED_ATTEMPT_JOBS)
    )
    mock_api.get(url__regex=rf"{re.escape(CHECK_RUNS_URL)}/1[34]/annotations").mock(
        return_value=httpx.Response(
            200, json=[EXIT_ANNOTATION, {**SPOT_ANNOTATION, "title": None}]
        )
    )

    assert read_interrupted_jobs(github_client, RUN, 1) == ()


def test_every_page_of_the_jobs_and_of_the_annotations_is_read(
    mock_api: respx.MockRouter, github_client: httpx.Client
) -> None:
    jobs_url = f"{RUN.api_url}/attempts/2/jobs"
    annotations_url = f"{CHECK_RUNS_URL}/13/annotations"
    jobs_pages = [
        mock_api.get(jobs_url, params={"page": "1"}).mock(
            return_value=httpx.Response(
                200,
                json={
                    "total_count": 101,
                    "jobs": [_job(1000 + i, f"job {i}", "success") for i in range(100)],
                },
            )
        ),
        mock_api.get(jobs_url, params={"page": "2"}).mock(
            return_value=httpx.Response(
                200,
                json={"total_count": 101, "jobs": [_job(13, LAUNCH_JOB, "failure")]},
            )
        ),
    ]
    annotations_pages = [
        mock_api.get(annotations_url, params={"page": "1"}).mock(
            return_value=httpx.Response(200, json=[EXIT_ANNOTATION] * 100)
        ),
        mock_api.get(annotations_url, params={"page": "2"}).mock(
            return_value=httpx.Response(200, json=[SPOT_ANNOTATION])
        ),
    ]

    assert read_interrupted_jobs(github_client, RUN, 2) == (LAUNCH_JOB,)
    for page in jobs_pages + annotations_pages:
        [call] = page.calls
        assert call.request.url.params["per_page"] == "100"


@pytest.mark.parametrize(
    "response",
    [
        [],
        {"total_count": 0},
        {"jobs": [{"name": LAUNCH_JOB, "conclusion": "failure"}]},
        {"jobs": [{**_job(13, LAUNCH_JOB, "failure"), "name": None}]},
        {"jobs": [{**_job(13, LAUNCH_JOB, "failure"), "conclusion": 1}]},
        # A check run of another repository.
        {"jobs": [_job(13, LAUNCH_JOB, "failure", RepoSlug(OWNER, "nodes-hub"))]},
        {"jobs": ["13"]},
    ],
)
def test_a_malformed_list_of_jobs_is_refused(
    response: object, mock_api: respx.MockRouter, github_client: httpx.Client
) -> None:
    mock_api.get(f"{RUN.api_url}/attempts/1/jobs").mock(
        return_value=httpx.Response(200, json=response)
    )

    with pytest.raises(ReleaseError) as excinfo:
        read_interrupted_jobs(github_client, RUN, 1)

    assert re.search(
        rf"unexpected GitHub API response for (the jobs|a job) of the run "
        rf"{re.escape(RUN.html_url)}",
        unwrapped(str(excinfo.value)),
    )


@pytest.mark.parametrize(
    "response",
    [SPOT_ANNOTATION, [{"title": 7}], ["EC2 Spot interruption"]],
)
def test_a_malformed_list_of_annotations_is_refused(
    response: object, mock_api: respx.MockRouter, github_client: httpx.Client
) -> None:
    mock_api.get(f"{RUN.api_url}/attempts/1/jobs").mock(
        return_value=httpx.Response(
            200, json={"jobs": [_job(13, LAUNCH_JOB, "failure")]}
        )
    )
    mock_api.get(f"{CHECK_RUNS_URL}/13/annotations").mock(
        return_value=httpx.Response(200, json=response)
    )

    with pytest.raises(ReleaseError) as excinfo:
        read_interrupted_jobs(github_client, RUN, 1)

    assert (
        f"unexpected GitHub API response for the annotations of {CHECK_RUNS_URL}/13"
    ) in unwrapped(str(excinfo.value))


# --- the wait for the last attempt ---


def test_the_wait_polls_until_the_run_completes() -> None:
    time = FakeTime()
    interruptions = _Interruptions()

    state = wait_for_last_attempt(
        _states(
            RunState(1, "queued", None),
            RunState(1, "in_progress", None),
            RunState(1, "in_progress", None),
            RunState(1, "completed", "success"),
        ),
        interruptions,
        RUN,
        _polling(time),
    )

    assert state == RunState(1, "completed", "success")
    assert time.sleeps == [60.0, 60.0, 60.0]
    # A successful attempt is the last one: the wait reads none of its jobs.
    assert interruptions.reads == []


def test_the_wait_reads_again_after_a_failed_read() -> None:
    time = FakeTime()

    state = wait_for_last_attempt(
        _states(
            RunState(1, "in_progress", None),
            ReleaseError("Status: 502"),
            ReleaseError("Status: 502"),
            RunState(1, "completed", "success"),
        ),
        _Interruptions(),
        RUN,
        _polling(time, max_failed_reads=3),
    )

    assert state == RunState(1, "completed", "success")
    assert time.sleeps == [60.0, 60.0, 60.0]


def test_the_wait_stops_after_too_many_failed_reads_in_a_row() -> None:
    time = FakeTime()

    with pytest.raises(ReleaseError) as excinfo:
        wait_for_last_attempt(
            _states(
                ReleaseError("Status: 502"),
                RunState(1, "in_progress", None),
                ReleaseError("Status: 502"),
                ReleaseError("Status: 503"),
                ReleaseError("Status: 504"),
            ),
            _Interruptions(),
            RUN,
            _polling(time, max_failed_reads=3),
        )

    message = unwrapped(str(excinfo.value))
    assert f"reading the run {RUN.html_url} failed 3 times in a row" in message
    assert "Status: 504" in message


def test_the_wait_stops_at_its_timeout() -> None:
    time = FakeTime()

    with pytest.raises(ReleaseError) as excinfo:
        wait_for_last_attempt(
            lambda: RunState(1, "in_progress", None),
            _Interruptions(),
            RUN,
            _polling(time, timeout=180.0),
        )

    assert time.sleeps == [60.0, 60.0, 60.0]
    assert f"the run {RUN.html_url} did not complete within 3 minutes" in unwrapped(
        str(excinfo.value)
    )


def test_a_failed_attempt_without_a_spot_interruption_is_the_last_one() -> None:
    time = FakeTime()
    interruptions = _Interruptions()

    state = wait_for_last_attempt(
        _states(RunState(1, "completed", "failure")),
        interruptions,
        RUN,
        _polling(time),
    )

    assert state == RunState(1, "completed", "failure")
    assert interruptions.reads == [1]
    assert time.sleeps == []


@pytest.mark.parametrize("conclusion", ["success", "cancelled", "timed_out", None])
def test_runs_on_retries_a_failed_attempt_alone(conclusion: str | None) -> None:
    interruptions = _Interruptions(1)

    state = wait_for_last_attempt(
        _states(RunState(1, "completed", conclusion)),
        interruptions,
        RUN,
        _polling(FakeTime()),
    )

    assert state == RunState(1, "completed", conclusion)
    assert interruptions.reads == []


def test_the_wait_follows_the_attempt_runs_on_starts_after_a_spot_interruption() -> (
    None
):
    time = FakeTime()
    interruptions = _Interruptions(1)

    state = wait_for_last_attempt(
        _states(
            RunState(1, "in_progress", None),
            RunState(1, "completed", "failure"),
            # RunsOn has not started the next attempt yet.
            RunState(1, "completed", "failure"),
            RunState(2, "queued", None),
            RunState(2, "in_progress", None),
            RunState(2, "completed", "success"),
        ),
        interruptions,
        RUN,
        _polling(time),
    )

    assert state == RunState(2, "completed", "success")
    assert interruptions.reads == [1]
    assert time.sleeps == [60.0, 60.0, 60.0]


def test_the_wait_follows_runs_on_after_attempt_2_at_the_latest() -> None:
    interruptions = _Interruptions(1, 2, 3)

    state = wait_for_last_attempt(
        _states(
            RunState(1, "completed", "failure"),
            RunState(2, "in_progress", None),
            RunState(2, "completed", "failure"),
            RunState(3, "in_progress", None),
            RunState(3, "completed", "failure"),
        ),
        interruptions,
        RUN,
        _polling(FakeTime()),
    )

    # Attempt 3 is the last one RunsOn runs, so its jobs are never read.
    assert state == RunState(3, "completed", "failure")
    assert interruptions.reads == [1, 2]


def test_an_attempt_that_starts_between_two_reads_is_followed() -> None:
    time = FakeTime()
    interruptions = _Interruptions(1)

    # Attempt 1 completes and attempt 2 starts between two reads, so the wait
    # never sees attempt 1 complete.
    state = wait_for_last_attempt(
        _states(
            RunState(1, "in_progress", None),
            RunState(2, "in_progress", None),
            RunState(2, "completed", "success"),
        ),
        interruptions,
        RUN,
        _polling(time),
    )

    assert state == RunState(2, "completed", "success")
    assert interruptions.reads == []
    assert time.sleeps == [60.0, 60.0]


def test_the_wait_stops_when_runs_on_does_not_start_the_next_attempt() -> None:
    time = FakeTime()

    with pytest.raises(ReleaseError) as excinfo:
        wait_for_last_attempt(
            lambda: RunState(1, "completed", "failure"),
            _Interruptions(1),
            RUN,
            _polling(time, retry_start_timeout=180.0),
        )

    assert time.sleeps == [60.0, 60.0, 60.0]
    message = unwrapped(str(excinfo.value))
    assert (
        f"RunsOn did not start attempt 2 of the run {RUN.html_url} within 3 "
        f"minutes of the end of attempt 1"
    ) in message
    assert "nothing is published" in message


def test_the_timeout_of_the_wait_covers_the_start_of_the_next_attempt() -> None:
    time = FakeTime()

    with pytest.raises(ReleaseError) as excinfo:
        wait_for_last_attempt(
            lambda: RunState(1, "completed", "failure"),
            _Interruptions(1),
            RUN,
            _polling(time, timeout=120.0, retry_start_timeout=900.0),
        )

    assert time.sleeps == [60.0, 60.0]
    assert f"the run {RUN.html_url} did not complete within 2 minutes" in unwrapped(
        str(excinfo.value)
    )


def test_the_timeout_of_the_wait_covers_every_attempt() -> None:
    time = FakeTime()
    states = iter([RunState(1, "completed", "failure")])

    with pytest.raises(ReleaseError) as excinfo:
        wait_for_last_attempt(
            lambda: next(states, RunState(2, "in_progress", None)),
            _Interruptions(1),
            RUN,
            _polling(time, timeout=300.0),
        )

    # Attempt 2 starts at once and runs until the timeout, which counts from
    # the start of the wait, not from the start of attempt 2.
    assert time.sleeps == [60.0] * 5
    assert f"the run {RUN.html_url} did not complete within 5 minutes" in unwrapped(
        str(excinfo.value)
    )


def test_a_failed_read_of_the_jobs_is_tried_again() -> None:
    time = FakeTime()
    answers: Iterator[tuple[str, ...] | ReleaseError] = iter(
        [ReleaseError("Status: 502"), (LAUNCH_JOB,)]
    )

    def read_interrupted_jobs(attempt: int) -> tuple[str, ...]:
        answer = next(answers)
        if isinstance(answer, ReleaseError):
            raise answer
        return answer

    state = wait_for_last_attempt(
        _states(
            RunState(1, "completed", "failure"),
            RunState(2, "in_progress", None),
            RunState(2, "completed", "success"),
        ),
        read_interrupted_jobs,
        RUN,
        _polling(time, max_failed_reads=3),
    )

    assert state == RunState(2, "completed", "success")
    assert time.sleeps == [60.0]


def test_the_wait_stops_after_too_many_failed_reads_of_the_jobs() -> None:
    time = FakeTime()

    def read_interrupted_jobs(attempt: int) -> tuple[str, ...]:
        raise ReleaseError("Status: 503")

    with pytest.raises(ReleaseError) as excinfo:
        wait_for_last_attempt(
            _states(RunState(1, "completed", "failure")),
            read_interrupted_jobs,
            RUN,
            _polling(time, max_failed_reads=3),
        )

    assert time.sleeps == [60.0, 60.0]
    message = unwrapped(str(excinfo.value))
    assert (
        f"reading the jobs of attempt 1 of the run {RUN.html_url} failed 3 times "
        f"in a row"
    ) in message
    assert "Status: 503" in message


def test_a_successful_run_passes() -> None:
    require_successful_run(RunState(1, "completed", "success"), RUN)


@pytest.mark.parametrize("conclusion", ["failure", "cancelled", "timed_out", None])
def test_any_other_conclusion_fails_naming_the_run(conclusion: str | None) -> None:
    with pytest.raises(ReleaseError) as excinfo:
        require_successful_run(RunState(2, "completed", conclusion), RUN)

    message = unwrapped(str(excinfo.value))
    assert (
        f"attempt 2 of the launchers-hub run {RUN.html_url} concluded `{conclusion}`"
    ) in message
    assert "nothing is published" in message


# --- the hub-launch stage ---


@pytest.fixture
def launchers_hub(mock_api: respx.MockRouter) -> dict:
    """launchers-hub's dispatch, run, jobs and annotations endpoints,
    recording what reaches them. The run answers the reads of `run_states` in
    turn, a successful attempt 1 unless a case sets them."""
    seen: dict = {
        "dispatches": [],
        "reads": [],
        "run_states": [
            _run_response(1, "queued"),
            _run_response(1, "in_progress"),
            _run_response(1, "completed", "success"),
        ],
    }

    def dispatch(request: httpx.Request) -> httpx.Response:
        seen["dispatches"].append(request)
        # As GitHub does: the response names the run only when the request
        # asks for it, and is 204 with no body otherwise.
        if json.loads(request.content).get("return_run_details") is not True:
            return httpx.Response(204)
        return httpx.Response(
            200, json=seen.get("dispatch_response", _dispatch_response())
        )

    def read_run(request: httpx.Request) -> httpx.Response:
        seen["reads"].append(request)
        return httpx.Response(200, json=seen["run_states"].pop(0))

    def read(answer: object) -> Callable[[httpx.Request], httpx.Response]:
        def respond(request: httpx.Request) -> httpx.Response:
            seen["reads"].append(request)
            return httpx.Response(200, json=answer)

        return respond

    mock_api.post(DISPATCH_URL).mock(side_effect=dispatch)
    mock_api.get(RUN.api_url).mock(side_effect=read_run)
    mock_api.get(f"{RUN.api_url}/attempts/1/jobs").mock(
        side_effect=read(INTERRUPTED_ATTEMPT_JOBS)
    )
    mock_api.get(f"{CHECK_RUNS_URL}/13/annotations").mock(
        side_effect=read([EXIT_ANNOTATION, SPOT_ANNOTATION])
    )
    mock_api.get(f"{CHECK_RUNS_URL}/14/annotations").mock(
        side_effect=read([EXIT_ANNOTATION])
    )
    return seen


def _check_launchers() -> DispatchedRun:
    time = FakeTime()
    return check_launchers(
        _client("release-token"),
        _client("job-token"),
        OWNER,
        HUB_SET,
        18000000001,
        sleep=time.sleep,
        clock=time.clock,
    )


def _client(token: str) -> httpx.Client:
    return httpx.Client(headers={"Authorization": f"Bearer {token}"})


def test_launchers_hub_tests_run_on_the_set_and_the_release_run(
    launchers_hub: dict,
) -> None:
    run = _check_launchers()

    assert run == RUN
    [dispatch] = launchers_hub["dispatches"]
    payload = json.loads(dispatch.content)
    dispatched_set = payload["inputs"]["set"]
    assert payload == {
        "ref": "main",
        "inputs": {"peppy-run-id": "18000000001", "set": dispatched_set},
        "return_run_details": True,
    }
    # The set goes on as one line of the schema it was recorded in.
    assert "\n" not in dispatched_set
    assert json.loads(dispatched_set) == HUB_SET_DOCUMENT
    # The release token dispatches; the job token, which outlives it, reads.
    assert dispatch.headers["Authorization"] == "Bearer release-token"
    assert {read.headers["Authorization"] for read in launchers_hub["reads"]} == {
        "Bearer job-token"
    }
    assert len(launchers_hub["reads"]) == 3


def test_a_dispatch_that_names_no_run_is_never_waited_for(
    launchers_hub: dict,
) -> None:
    launchers_hub["dispatch_response"] = {}

    with pytest.raises(ReleaseError, match="answered without a workflow_run_id"):
        _check_launchers()

    assert launchers_hub["reads"] == []


def test_launchers_hub_tests_that_fail_fail_the_stage(
    launchers_hub: dict, mock_api: respx.MockRouter
) -> None:
    launchers_hub["run_states"] = [_run_response(1, "completed", "failure")]
    # No job of the attempt carries the mark of a spot interruption.
    mock_api.get(f"{CHECK_RUNS_URL}/13/annotations").mock(
        return_value=httpx.Response(200, json=[EXIT_ANNOTATION])
    )

    with pytest.raises(
        ReleaseError,
        match=re.escape(f"attempt 1 of the launchers-hub run {RUN.html_url}"),
    ):
        _check_launchers()


def test_launchers_hub_tests_that_aws_interrupted_pass_on_the_next_attempt(
    launchers_hub: dict,
) -> None:
    launchers_hub["run_states"] = [
        _run_response(1, "completed", "failure"),
        _run_response(2, "queued"),
        _run_response(2, "in_progress"),
        _run_response(2, "completed", "success"),
    ]

    assert _check_launchers() == RUN

    # The job token reads the run, the jobs of attempt 1 and the annotations
    # of its two failed jobs.
    read_urls = [str(read.url.copy_with(query=None)) for read in launchers_hub["reads"]]
    assert set(read_urls) == {
        RUN.api_url,
        f"{RUN.api_url}/attempts/1/jobs",
        f"{CHECK_RUNS_URL}/13/annotations",
        f"{CHECK_RUNS_URL}/14/annotations",
    }
    # One read of the run for attempt 1, which completed at once, and three
    # for attempt 2.
    assert read_urls.count(RUN.api_url) == 4
    assert {read.headers["Authorization"] for read in launchers_hub["reads"]} == {
        "Bearer job-token"
    }
