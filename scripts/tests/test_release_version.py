"""Tests for functions.release_version."""

from __future__ import annotations

import pytest

from functions.cli import ReleaseError
from functions.release_version import (
    FIRST_RELEASE_VERSION,
    ReleaseVersion,
    next_release_version,
)

# --- ReleaseVersion.from_tag ---


@pytest.mark.parametrize(
    ("tag", "expected"),
    [
        ("v0.36.3", ReleaseVersion(minor=36, patch=3)),
        ("v0.0.0", ReleaseVersion(minor=0, patch=0)),
        ("v0.10.0", ReleaseVersion(minor=10, patch=0)),
        # The grammar of resolve.py drops the whitespace around a tag.
        (" v0.36.3\n", ReleaseVersion(minor=36, patch=3)),
    ],
)
def test_from_tag_reads_the_minor_and_patch_versions(
    tag: str, expected: ReleaseVersion
) -> None:
    assert ReleaseVersion.from_tag(tag) == expected


def test_a_version_names_the_tag_it_was_read_from() -> None:
    assert ReleaseVersion.from_tag("v0.36.3").tag == "v0.36.3"


@pytest.mark.parametrize("tag", ["v1.0.0", "v2.36.3"])
def test_from_tag_refuses_a_major_version_other_than_0(tag: str) -> None:
    with pytest.raises(ReleaseError, match="is not v0.<MINOR>.<PATCH> with no leading"):
        ReleaseVersion.from_tag(tag)


@pytest.mark.parametrize("tag", ["v0.036.3", "v0.36.03", "v00.36.3"])
def test_from_tag_refuses_a_leading_zero(tag: str) -> None:
    # The next version names its tag with no leading zero, so it must name
    # the release it follows the same way.
    with pytest.raises(ReleaseError, match="is not v0.<MINOR>.<PATCH> with no leading"):
        ReleaseVersion.from_tag(tag)


@pytest.mark.parametrize("tag", ["v0.36.3-rc1", "0.36.3", "v0.36", "latest", ""])
def test_from_tag_refuses_a_tag_no_release_publishes(tag: str) -> None:
    # The grammar of a release tag is resolve.py's, whose tests hold every form.
    with pytest.raises(ReleaseError, match="is not a peppy release version"):
        ReleaseVersion.from_tag(tag)


# --- next_release_version ---


@pytest.mark.parametrize(
    ("previous", "breaking", "expected"),
    [
        ("v0.36.3", True, "v0.37.0"),
        ("v0.36.3", False, "v0.36.4"),
        ("v0.9.9", True, "v0.10.0"),
        ("v0.9.9", False, "v0.9.10"),
        ("v0.36.0", False, "v0.36.1"),
        ("v0.0.0", True, "v0.1.0"),
        ("v0.0.0", False, "v0.0.1"),
    ],
)
def test_next_release_version_follows_the_latest_release(
    previous: str, breaking: bool, expected: str
) -> None:
    version = next_release_version(ReleaseVersion.from_tag(previous), breaking=breaking)

    assert version.tag == expected


@pytest.mark.parametrize("breaking", [True, False])
def test_the_first_release_is_v0_1_0(breaking: bool) -> None:
    version = next_release_version(None, breaking=breaking)

    assert version == FIRST_RELEASE_VERSION
    assert version.tag == "v0.1.0"
