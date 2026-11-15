"""Tests for functions.release_hubs.

The GitHub API is a respx stand-in, so no case touches the network. The tag
steps of a whole publish, in their order, are in test_parallel_release.py.
The hub set, its parser and the hub tag of a release are resolve.py's, whose
tests (.github/actions/hub-ci-peppy/test_resolve.py) also hold the tag peppy
reads and the tokens of the release workflow; they run on every change.
"""

from __future__ import annotations

import httpx
import pytest
import respx

from functions.cli import ReleaseError
from functions.github import RepoSlug
from functions.release_hubs import read_hub_tag_commit

OWNER = "test-owner"
HUB_TAG = "peppy-release/v0.3.0"


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
