"""Tests for functions.release_hubs.

The GitHub API is a respx stand-in and the wait's sleep and clock are fakes,
so no case touches the network or sleeps. The tag steps of a whole publish,
in their order, are in test_parallel_release.py. What ties this module to
files outside scripts/ (the tag peppy reads, the tokens of the release
workflow) is held by .github/actions/hub-ci-peppy/test_resolve.py, which runs
on every change.
"""

from __future__ import annotations

import json
import re
from collections.abc import Callable, Iterator
from pathlib import Path

import httpx
import pytest
import respx

from functions.cli import ReleaseError
from functions.release_hubs import (
    LAUNCHERS_HUB,
    DispatchedRun,
    HubSet,
    RunState,
    check_launchers,
    hub_release_tag,
    parse_dispatch_response,
    read_hub_tag_commit,
    require_successful_run,
    wait_for_run,
)

OWNER = "test-owner"
HUB_TAG = "peppy-release/v0.3.0"
LAUNCHERS_COMMIT = "b" * 40
HUB_SET_TEXT = json.dumps(
    {
        "hubs": {
            "nodes-hub": {"ref": "main", "commit": "a" * 40},
            "launchers-hub": {"ref": HUB_TAG, "commit": LAUNCHERS_COMMIT},
            "private-nodes-hub": {"ref": "main", "commit": "c" * 40},
        }
    }
)
HUB_SET = HubSet.parse(HUB_SET_TEXT, source="hub-set.json")
RUN = DispatchedRun(
    repository=f"{OWNER}/launchers-hub",
    run_id=4242,
    html_url=f"https://github.com/{OWNER}/launchers-hub/actions/runs/4242",
)


def _unwrapped(text: str) -> str:
    return " ".join(text.split())


# --- the hub set ---


def test_a_hub_set_keeps_every_hub_in_its_order() -> None:
    assert [(hub.name, hub.ref, hub.commit) for hub in HUB_SET.hubs] == [
        ("nodes-hub", "main", "a" * 40),
        ("launchers-hub", HUB_TAG, LAUNCHERS_COMMIT),
        ("private-nodes-hub", "main", "c" * 40),
    ]


def test_a_hub_set_is_passed_on_as_one_line_of_its_schema() -> None:
    text = HUB_SET.compact_json()

    assert "\n" not in text
    assert json.loads(text) == json.loads(HUB_SET_TEXT)


def test_a_hub_set_is_loaded_from_its_file(tmp_path: Path) -> None:
    path = tmp_path / "hub-set.json"
    path.write_text(HUB_SET_TEXT)

    assert HubSet.load(path) == HUB_SET


def test_a_missing_hub_set_file_is_refused(tmp_path: Path) -> None:
    with pytest.raises(ReleaseError, match="cannot read the hub set"):
        HubSet.load(tmp_path / "hub-set.json")


def test_a_hub_is_found_by_name_and_a_missing_one_is_refused() -> None:
    assert HUB_SET.hub("launchers-hub").commit == LAUNCHERS_COMMIT
    with pytest.raises(ReleaseError, match="names no mcp-hub; it names nodes-hub"):
        HUB_SET.hub("mcp-hub")


@pytest.mark.parametrize(
    ("text", "refusal"),
    [
        ("not json", "is not JSON"),
        ("[]", "must have the shape"),
        ('{"hubs": {}}', "must have the shape"),
        ('{"hubs": {}, "name": "x"}', "must have the shape"),
        ('{"hubs": {"nodes-hub": "main"}}', "entry of nodes-hub"),
        (
            '{"hubs": {"nodes-hub": {"ref": "", "commit": "' + "a" * 40 + '"}}}',
            "entry of nodes-hub",
        ),
        ('{"hubs": {"nodes-hub": {"ref": "main"}}}', "entry of nodes-hub"),
        (
            '{"hubs": {"nodes-hub": {"ref": "main", "commit": "main"}}}',
            "records `main`",
        ),
        (
            '{"hubs": {"nodes-hub": {"ref": "main", "commit": "' + "A" * 40 + '"}}}',
            "records `AAAA",
        ),
        (
            '{"hubs": {"../peppy": {"ref": "main", "commit": "' + "a" * 40 + '"}}}',
            "not a repository name",
        ),
    ],
)
def test_a_malformed_hub_set_is_refused(text: str, refusal: str) -> None:
    with pytest.raises(ReleaseError, match=re.escape(refusal)):
        HubSet.parse(text, source="hub-set.json")


# --- the hub tags ---


def test_the_hub_tag_of_a_release() -> None:
    assert hub_release_tag("v0.3.0") == HUB_TAG


HUB_API = f"https://api.github.com/repos/{OWNER}/nodes-hub"


def test_a_hub_without_the_tag_has_no_tag_commit(
    mock_api: respx.MockRouter, github_client: httpx.Client
) -> None:
    mock_api.get(f"{HUB_API}/git/ref/tags/{HUB_TAG}").mock(
        return_value=httpx.Response(404, json={"message": "Not Found"})
    )

    assert read_hub_tag_commit(github_client, OWNER, "nodes-hub", HUB_TAG) is None


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

    assert read_hub_tag_commit(github_client, OWNER, "nodes-hub", HUB_TAG) == "3" * 40


def test_a_tag_that_names_no_commit_is_refused(
    mock_api: respx.MockRouter, github_client: httpx.Client
) -> None:
    mock_api.get(f"{HUB_API}/git/ref/tags/{HUB_TAG}").mock(
        return_value=httpx.Response(
            200, json={"object": {"type": "tree", "sha": "4" * 40}}
        )
    )

    with pytest.raises(ReleaseError, match="names a tree"):
        read_hub_tag_commit(github_client, OWNER, "nodes-hub", HUB_TAG)


def test_a_malformed_ref_response_is_refused(
    mock_api: respx.MockRouter, github_client: httpx.Client
) -> None:
    mock_api.get(f"{HUB_API}/git/ref/tags/{HUB_TAG}").mock(
        return_value=httpx.Response(200, json=[{"ref": f"refs/tags/{HUB_TAG}"}])
    )

    with pytest.raises(ReleaseError, match="unexpected GitHub API response"):
        read_hub_tag_commit(github_client, OWNER, "nodes-hub", HUB_TAG)


# --- the launchers-hub run ---

DISPATCH_URL = (
    f"https://api.github.com/repos/{OWNER}/launchers-hub/actions/workflows/"
    "tests.yml/dispatches"
)


def _dispatch_response(run_id: int = RUN.run_id) -> dict:
    return {
        "workflow_run_id": run_id,
        "run_url": f"https://api.github.com/repos/{OWNER}/launchers-hub/actions/runs/{run_id}",
        "html_url": f"https://github.com/{OWNER}/launchers-hub/actions/runs/{run_id}",
    }


def _run_response(status: str, conclusion: str | None = None) -> dict:
    return {"id": RUN.run_id, "status": status, "conclusion": conclusion}


class FakeTime:
    """A clock that moves only when the wait sleeps."""

    def __init__(self) -> None:
        self.now = 1000.0
        self.sleeps: list[float] = []

    def clock(self) -> float:
        return self.now

    def sleep(self, seconds: float) -> None:
        self.sleeps.append(seconds)
        self.now += seconds


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
        _unwrapped(str(excinfo.value))
    )


def test_a_dispatch_response_without_its_page_gets_the_run_page() -> None:
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

    message = _unwrapped(str(excinfo.value))
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
    assert f"the run {RUN.html_url} did not complete within 3 minutes" in _unwrapped(
        str(excinfo.value)
    )


def test_a_successful_run_passes() -> None:
    require_successful_run(RunState("completed", "success"), RUN)


@pytest.mark.parametrize("conclusion", ["failure", "cancelled", "timed_out", None])
def test_any_other_conclusion_fails_naming_the_run(conclusion: str | None) -> None:
    with pytest.raises(ReleaseError) as excinfo:
        require_successful_run(RunState("completed", conclusion), RUN)

    message = _unwrapped(str(excinfo.value))
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
    assert json.loads(dispatch.content) == {
        "ref": "main",
        "inputs": {"peppy-run-id": "18000000001", "set": HUB_SET.compact_json()},
    }
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


def test_a_set_without_launchers_hub_dispatches_nothing(
    mock_api: respx.MockRouter,
) -> None:
    hub_set = HubSet.parse(
        json.dumps({"hubs": {"nodes-hub": {"ref": "main", "commit": "a" * 40}}}),
        source="hub-set.json",
    )
    time = FakeTime()

    with pytest.raises(ReleaseError, match=f"names no {LAUNCHERS_HUB}"):
        check_launchers(
            _client("release-token"),
            _client("job-token"),
            OWNER,
            hub_set,
            1,
            sleep=time.sleep,
            clock=time.clock,
        )
