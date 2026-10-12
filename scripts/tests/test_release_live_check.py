"""Tests for functions.release_live_check.

peppy.bot, GitHub and docs.peppy.bot are respx stand-ins, and the wait's sleep
and clock are fakes, so no case touches the network or sleeps.
"""

from __future__ import annotations

from collections.abc import Iterator

import httpx
import pytest
import respx

from functions.cli import ReleaseError
from functions.github import RepoSlug
from functions.release_live_check import (
    LiveCheck,
    archive_download_problem,
    changelog_problem,
    install_script_problem,
    release_live_checks,
    wait_until_live,
)

from .helpers import FakeTime, unwrapped

TAG = "v0.3.0"
REPOSITORY = RepoSlug(owner="Test-Owner", repo="peppy")
ARCHIVE = "peppy-x86_64-unknown-linux-gnu.tgz"
LATEST_URL = f"https://peppy.bot/latest/{ARCHIVE}"
GITHUB_LATEST_URL = (
    f"https://github.com/Test-Owner/peppy/releases/latest/download/{ARCHIVE}"
)
ASSET_URL = f"https://github.com/Test-Owner/peppy/releases/download/{TAG}/{ARCHIVE}"
STORAGE_URL = "https://release-assets.githubusercontent.com/asset/42?sig=x"
INSTALL_URL = "https://peppy.bot/install.sh"
RAW_INSTALL_URL = (
    "https://raw.githubusercontent.com/Test-Owner/peppy/main/scripts/install.sh"
)
CHANGELOG_URL = "https://docs.peppy.bot/reference/changelog/"


@pytest.fixture()
def client(mock_api: respx.MockRouter) -> Iterator[httpx.Client]:
    with httpx.Client(follow_redirects=True) as client:
        yield client


def _redirect(mock_api: respx.MockRouter, url: str, location: str) -> None:
    mock_api.get(url).mock(
        return_value=httpx.Response(302, headers={"Location": location})
    )


# --- the URLs ---


def test_the_checks_cover_install_sh_every_archive_and_the_changelog() -> None:
    checks = release_live_checks(TAG, REPOSITORY, b"#!/bin/sh\n")

    assert [(check.url, check.content) for check in checks] == [
        ("https://peppy.bot/install.sh", "the install.sh of v0.3.0"),
        (
            "https://peppy.bot/latest/peppy-aarch64-apple-darwin.tgz",
            "the v0.3.0 archive",
        ),
        (
            "https://peppy.bot/v0.3.0/peppy-aarch64-apple-darwin.tgz",
            "the v0.3.0 archive",
        ),
        (
            "https://peppy.bot/latest/peppy-x86_64-unknown-linux-gnu.tgz",
            "the v0.3.0 archive",
        ),
        (
            "https://peppy.bot/v0.3.0/peppy-x86_64-unknown-linux-gnu.tgz",
            "the v0.3.0 archive",
        ),
        (
            "https://peppy.bot/latest/peppy-aarch64-unknown-linux-gnu.tgz",
            "the v0.3.0 archive",
        ),
        (
            "https://peppy.bot/v0.3.0/peppy-aarch64-unknown-linux-gnu.tgz",
            "the v0.3.0 archive",
        ),
        ("https://docs.peppy.bot/reference/changelog/", "the notes of v0.3.0"),
    ]


def test_each_archive_check_requires_the_github_download_of_its_own_archive(
    mock_api: respx.MockRouter, client: httpx.Client
) -> None:
    checks = release_live_checks(TAG, REPOSITORY, b"#!/bin/sh\n")
    latest_x86_64 = next(check for check in checks if check.url == LATEST_URL)
    _redirect(mock_api, LATEST_URL, GITHUB_LATEST_URL)
    _redirect(mock_api, GITHUB_LATEST_URL, ASSET_URL)
    _redirect(mock_api, ASSET_URL, STORAGE_URL)
    mock_api.get(STORAGE_URL).mock(return_value=httpx.Response(200, content=b"tgz"))

    assert latest_x86_64.probe(client, latest_x86_64.url) is None


def test_the_install_check_requires_the_install_sh_it_was_given(
    mock_api: respx.MockRouter, client: httpx.Client
) -> None:
    install_check = release_live_checks(TAG, REPOSITORY, b"#!/bin/sh\necho 3\n")[0]
    _redirect(mock_api, INSTALL_URL, RAW_INSTALL_URL)
    mock_api.get(RAW_INSTALL_URL).mock(
        return_value=httpx.Response(200, content=b"#!/bin/sh\necho 2\n")
    )

    assert install_check.probe(client, install_check.url) == (
        f"it leads to {RAW_INSTALL_URL}, which serves another install.sh"
    )


# --- the archives ---


def test_an_archive_that_redirects_through_the_release_download_is_live(
    mock_api: respx.MockRouter, client: httpx.Client
) -> None:
    _redirect(mock_api, LATEST_URL, GITHUB_LATEST_URL)
    _redirect(mock_api, GITHUB_LATEST_URL, ASSET_URL)
    _redirect(mock_api, ASSET_URL, STORAGE_URL)
    mock_api.get(STORAGE_URL).mock(return_value=httpx.Response(200, content=b"tgz"))

    assert archive_download_problem(client, LATEST_URL, asset_url=ASSET_URL) is None


def test_the_release_download_matches_in_any_case(
    mock_api: respx.MockRouter, client: httpx.Client
) -> None:
    lower_case_asset = ASSET_URL.replace("Test-Owner", "test-owner")
    _redirect(mock_api, LATEST_URL, lower_case_asset)
    _redirect(mock_api, lower_case_asset, STORAGE_URL)
    mock_api.get(STORAGE_URL).mock(return_value=httpx.Response(200, content=b"tgz"))

    assert archive_download_problem(client, LATEST_URL, asset_url=ASSET_URL) is None


def test_latest_still_on_the_previous_release_is_not_live(
    mock_api: respx.MockRouter, client: httpx.Client
) -> None:
    previous_asset = ASSET_URL.replace(TAG, "v0.2.0")
    _redirect(mock_api, LATEST_URL, GITHUB_LATEST_URL)
    _redirect(mock_api, GITHUB_LATEST_URL, previous_asset)
    _redirect(mock_api, previous_asset, STORAGE_URL)
    mock_api.get(STORAGE_URL).mock(return_value=httpx.Response(200, content=b"tgz"))

    # The signed query of the storage URL is left out of the chain.
    assert archive_download_problem(client, LATEST_URL, asset_url=ASSET_URL) == (
        f"its redirect chain {LATEST_URL} -> {GITHUB_LATEST_URL} -> "
        f"{previous_asset} -> https://release-assets.githubusercontent.com/asset/42 "
        f"(status 200) does not pass through {ASSET_URL}"
    )


def test_a_release_download_that_finds_no_file_is_not_live(
    mock_api: respx.MockRouter, client: httpx.Client
) -> None:
    versioned_url = f"https://peppy.bot/{TAG}/{ARCHIVE}"
    _redirect(mock_api, versioned_url, ASSET_URL)
    mock_api.get(ASSET_URL).mock(return_value=httpx.Response(404))

    assert archive_download_problem(client, versioned_url, asset_url=ASSET_URL) == (
        f"its redirect chain {versioned_url} -> {ASSET_URL} ends with status 404"
    )


def test_a_site_that_serves_no_redirect_is_not_live(
    mock_api: respx.MockRouter, client: httpx.Client
) -> None:
    mock_api.get(LATEST_URL).mock(return_value=httpx.Response(404))

    assert archive_download_problem(client, LATEST_URL, asset_url=ASSET_URL) == (
        f"its redirect chain {LATEST_URL} (status 404) does not pass through "
        f"{ASSET_URL}"
    )


# --- install.sh ---


def test_install_sh_of_the_release_is_live(
    mock_api: respx.MockRouter, client: httpx.Client
) -> None:
    _redirect(mock_api, INSTALL_URL, RAW_INSTALL_URL)
    mock_api.get(RAW_INSTALL_URL).mock(return_value=httpx.Response(200, content=b"v3"))

    assert install_script_problem(client, INSTALL_URL, install_script=b"v3") is None


def test_install_sh_that_answers_an_error_is_not_live(
    mock_api: respx.MockRouter, client: httpx.Client
) -> None:
    _redirect(mock_api, INSTALL_URL, RAW_INSTALL_URL)
    mock_api.get(RAW_INSTALL_URL).mock(return_value=httpx.Response(404))

    assert install_script_problem(client, INSTALL_URL, install_script=b"v3") == (
        f"it leads to {RAW_INSTALL_URL}, which answers 404"
    )


# --- the changelog ---


def test_a_changelog_that_links_to_the_release_page_is_live(
    mock_api: respx.MockRouter, client: httpx.Client
) -> None:
    mock_api.get(CHANGELOG_URL).mock(
        return_value=httpx.Response(
            200, text='<a href="/releases/v0-3-0/">v0.3.0 (Alpha)</a>'
        )
    )

    assert (
        changelog_problem(client, CHANGELOG_URL, release_path="/releases/v0-3-0/")
        is None
    )


@pytest.mark.parametrize(
    ("release_path", "page"),
    [
        ("/releases/v0-3-0/", '<a href="/releases/v0-2-0/">v0.2.0 (Alpha)</a>'),
        ("/releases/v0-3-1/", '<a href="/releases/v0-3-10/">v0.3.10 (Alpha)</a>'),
    ],
    ids=["previous-release", "release-whose-name-starts-the-same"],
)
def test_a_changelog_without_the_release_link_is_not_live(
    mock_api: respx.MockRouter, client: httpx.Client, release_path: str, page: str
) -> None:
    mock_api.get(CHANGELOG_URL).mock(return_value=httpx.Response(200, text=page))

    assert changelog_problem(client, CHANGELOG_URL, release_path=release_path) == (
        f"it has no link to {release_path}"
    )


def test_a_changelog_that_answers_an_error_is_not_live(
    mock_api: respx.MockRouter, client: httpx.Client
) -> None:
    mock_api.get(CHANGELOG_URL).mock(return_value=httpx.Response(502))

    assert (
        changelog_problem(client, CHANGELOG_URL, release_path="/releases/v0-3-0/")
        == "it answers 502"
    )


# --- the wait ---


class ScriptedProbe:
    """A probe that answers the problems it is given, one per read, and counts
    its reads. An exception in the script is raised instead."""

    def __init__(self, *answers: str | None | Exception) -> None:
        self._answers = iter(answers)
        self.reads = 0

    def __call__(self, client: httpx.Client, url: str) -> str | None:
        self.reads += 1
        answer = next(self._answers)
        if isinstance(answer, Exception):
            raise answer
        return answer


def _check(name: str, probe: ScriptedProbe) -> LiveCheck:
    return LiveCheck(url=f"https://example.test/{name}", content=name, probe=probe)


def test_the_wait_reads_each_url_until_it_is_live_and_then_no_more(
    client: httpx.Client,
) -> None:
    time = FakeTime()
    docs = ScriptedProbe("it has no link", "it has no link", None)
    archive = ScriptedProbe(None)

    wait_until_live(
        [_check("docs", docs), _check("archive", archive)],
        client,
        sleep=time.sleep,
        clock=time.clock,
        poll_interval=30.0,
    )

    assert docs.reads == 3
    assert archive.reads == 1
    assert time.sleeps == [30.0, 30.0]


def test_the_wait_reads_again_after_a_url_fails_to_answer(
    client: httpx.Client,
) -> None:
    time = FakeTime()
    docs = ScriptedProbe(httpx.ConnectError("connection refused"), None)

    wait_until_live([_check("docs", docs)], client, sleep=time.sleep, clock=time.clock)

    assert docs.reads == 2


def test_the_wait_stops_at_its_timeout_naming_each_url_not_live(
    client: httpx.Client,
) -> None:
    time = FakeTime()
    docs = ScriptedProbe(*["it has no link to /releases/v0-3-0/"] * 4)
    install = ScriptedProbe(*[httpx.ConnectError("connection refused")] * 4)
    archive = ScriptedProbe(None)

    with pytest.raises(ReleaseError) as excinfo:
        wait_until_live(
            [
                _check("docs", docs),
                _check("install", install),
                _check("archive", archive),
            ],
            client,
            sleep=time.sleep,
            clock=time.clock,
            poll_interval=60.0,
            timeout=180.0,
        )

    assert time.sleeps == [60.0, 60.0, 60.0]
    message = unwrapped(str(excinfo.value))
    assert (
        "after 3 minutes, users still do not get the release from 2 URL(s):" in message
    )
    assert (
        "- https://example.test/docs does not serve docs: it has no link to "
        "/releases/v0-3-0/." in message
    )
    assert (
        "- https://example.test/install does not serve install: reading it "
        "failed: connection refused." in message
    )
    assert "example.test/archive" not in message
    assert "re-run this job" in message
