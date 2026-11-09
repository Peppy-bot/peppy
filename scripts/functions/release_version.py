"""The version of a peppy release, picked from the latest published release.

Every peppy release is v0.<MINOR>.<PATCH>, and the version of a release
follows the latest published release:

- a release whose notes list a breaking change takes the next minor version,
  v0.<MINOR+1>.0;
- any other release takes the next patch version, v0.<MINOR>.<PATCH+1>.

The first release of all, with no published release before it, is v0.1.0.
"""

from __future__ import annotations

from dataclasses import dataclass

from .cli import ReleaseError
from .hub_ci import parse_release_version


@dataclass(frozen=True)
class ReleaseVersion:
    """A peppy release version, v0.<minor>.<patch>."""

    minor: int
    patch: int

    @property
    def tag(self) -> str:
        """The tag of the release, v0.<minor>.<patch>."""
        return f"v0.{self.minor}.{self.patch}"

    @classmethod
    def from_tag(cls, text: str) -> ReleaseVersion:
        """The version the release tag *text* names.

        The grammar of a release tag is resolve.py's (`parse_release_version`).
        A version follows from a tag of the form v0.<MINOR>.<PATCH> with no
        leading zero alone: the next version keeps the major version 0, and
        its tag must name the same release as the git tag it follows.
        """
        tag = parse_release_version(text)
        major, minor, patch = (int(part) for part in tag.removeprefix("v").split("."))
        version = cls(minor=minor, patch=patch)
        if major != 0 or version.tag != tag:
            raise ReleaseError(
                f"the release {tag} is not v0.<MINOR>.<PATCH> with no leading "
                f"zero, the only form the version of the next release follows "
                f"from: v0.<MINOR+1>.0 when its notes list a breaking change, "
                f"v0.<MINOR>.<PATCH+1> when they do not."
            )
        return version


FIRST_RELEASE_VERSION = ReleaseVersion(minor=1, patch=0)


def next_release_version(
    previous: ReleaseVersion | None, *, breaking: bool
) -> ReleaseVersion:
    """The version of the release that follows *previous*, the latest
    published release, or None when no release is published yet.

    *breaking* tells whether the notes of the release list a breaking change.
    """
    if previous is None:
        return FIRST_RELEASE_VERSION
    if breaking:
        return ReleaseVersion(minor=previous.minor + 1, patch=0)
    return ReleaseVersion(minor=previous.minor, patch=previous.patch + 1)
