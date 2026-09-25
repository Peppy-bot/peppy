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
    RunState,
    check_launchers,
    parse_dispatch_response,
    read_hub_tag_commit,
    require_successful_run,
    wait_for_run,
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


def _dispatch_response(run_id: int = RUN.run_id) -> dict:
    return {
        "workflow_run_id": run_id,
        "run_url": f"{RUN.repository.api_url}/actions/runs/{run_id}",
        "html_url": f"https://github.com/{OWNER}/launchers-hub/actions/runs/{run_id}",
    }


def _run_response(status: str, conclusion: str | None = None) -> dict:
    return {"id": RUN.run_id, "status": status, "conclusion": conclusion}


def _states(*states: RunState | ReleaseError) -> Callable[[], RunState]:
    """A read of the run that answers *states* in turn."""
    answers: Iterator[RunState | ReleaseError] = iter(states)

    def read() -> RunState:
        answer = next(answers)
        if isinstance(answer, ReleaseError):
            raise answer
        return answer

    return read


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


def test_the_wait_polls_until_the_run_completes() -> None:
    time = FakeTime()

    state = wait_for_run(
        _states(
            RunState("queued", None),
            RunState("in_progress", None),
            RunState("in_progress", None),
            RunState("completed", "success"),
        ),
        RUN,
        sleep=time.sleep,
        clock=time.clock,
        poll_interval=60.0,
    )

    assert state == RunState("completed", "success")
    assert time.sleeps == [60.0, 60.0, 60.0]


def test_the_wait_reads_again_after_a_failed_read() -> None:
    time = FakeTime()

    state = wait_for_run(
        _states(
            RunState("in_progress", None),
            ReleaseError("Status: 502"),
            ReleaseError("Status: 502"),
            RunState("completed", "failure"),
        ),
        RUN,
        sleep=time.sleep,
        clock=time.clock,
        max_failed_reads=3,
    )

    assert state == RunState("completed", "failure")


def test_the_wait_stops_after_too_many_failed_reads_in_a_row() -> None:
    time = FakeTime()

    with pytest.raises(ReleaseError) as excinfo:
        wait_for_run(
            _states(
                ReleaseError("Status: 502"),
                RunState("in_progress", None),
                ReleaseError("Status: 502"),
                ReleaseError("Status: 503"),
                ReleaseError("Status: 504"),
            ),
            RUN,
            sleep=time.sleep,
            clock=time.clock,
            max_failed_reads=3,
        )

    message = unwrapped(str(excinfo.value))
    assert f"reading the run {RUN.html_url} failed 3 times in a row" in message
    assert "Status: 504" in message


def test_the_wait_stops_at_its_timeout() -> None:
    time = FakeTime()

    with pytest.raises(ReleaseError) as excinfo:
        wait_for_run(
            lambda: RunState("in_progress", None),
            RUN,
            sleep=time.sleep,
            clock=time.clock,
            poll_interval=60.0,
            timeout=180.0,
        )

    assert time.sleeps == [60.0, 60.0, 60.0]
    assert f"the run {RUN.html_url} did not complete within 3 minutes" in unwrapped(
        str(excinfo.value)
    )


def test_a_successful_run_passes() -> None:
    require_successful_run(RunState("completed", "success"), RUN)


@pytest.mark.parametrize("conclusion", ["failure", "cancelled", "timed_out", None])
def test_any_other_conclusion_fails_naming_the_run(conclusion: str | None) -> None:
    with pytest.raises(ReleaseError) as excinfo:
        require_successful_run(RunState("completed", conclusion), RUN)

    message = unwrapped(str(excinfo.value))
    assert f"the launchers-hub run {RUN.html_url} concluded `{conclusion}`" in message
    assert "nothing is published" in message


@pytest.fixture
def launchers_hub(mock_api: respx.MockRouter) -> dict:
    """launchers-hub's dispatch and run endpoints, recording what reaches them."""
    seen: dict = {"dispatches": [], "reads": []}
    states = iter(
        [
            _run_response("queued"),
            _run_response("in_progress"),
            _run_response("completed", "success"),
        ]
    )

    def dispatch(request: httpx.Request) -> httpx.Response:
        seen["dispatches"].append(request)
        return httpx.Response(
            200, json=seen.get("dispatch_response", _dispatch_response())
        )

    def read(request: httpx.Request) -> httpx.Response:
        seen["reads"].append(request)
        return httpx.Response(200, json=next(states))

    mock_api.post(DISPATCH_URL).mock(side_effect=dispatch)
    mock_api.get(RUN.api_url).mock(side_effect=read)
    return seen


def _client(token: str) -> httpx.Client:
    return httpx.Client(headers={"Authorization": f"Bearer {token}"})


def test_launchers_hub_tests_run_on_the_set_and_the_release_run(
    launchers_hub: dict,
) -> None:
    time = FakeTime()

    run = check_launchers(
        _client("release-token"),
        _client("job-token"),
        OWNER,
        HUB_SET,
        18000000001,
        sleep=time.sleep,
        clock=time.clock,
    )

    assert run == RUN
    [dispatch] = launchers_hub["dispatches"]
    payload = json.loads(dispatch.content)
    dispatched_set = payload["inputs"]["set"]
    assert payload == {
        "ref": "main",
        "inputs": {"peppy-run-id": "18000000001", "set": dispatched_set},
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
    time = FakeTime()

    with pytest.raises(ReleaseError, match="answered without a workflow_run_id"):
        check_launchers(
            _client("release-token"),
            _client("job-token"),
            OWNER,
            HUB_SET,
            18000000001,
            sleep=time.sleep,
            clock=time.clock,
        )

    assert launchers_hub["reads"] == []


def test_launchers_hub_tests_that_fail_fail_the_stage(
    mock_api: respx.MockRouter,
) -> None:
    mock_api.post(DISPATCH_URL).mock(
        return_value=httpx.Response(200, json=_dispatch_response())
    )
    mock_api.get(RUN.api_url).mock(
        return_value=httpx.Response(200, json=_run_response("completed", "failure"))
    )
    time = FakeTime()

    with pytest.raises(
        ReleaseError, match=re.escape(f"{RUN.html_url} concluded `failure`")
    ):
        check_launchers(
            _client("release-token"),
            _client("job-token"),
            OWNER,
            HUB_SET,
            18000000001,
            sleep=time.sleep,
            clock=time.clock,
        )
