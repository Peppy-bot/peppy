"""The hubs a peppy release tests, and tags once it publishes.

The hub-set job of .github/workflows/parallel-release.yml records the hub set
of the release with .github/actions/hub-ci-peppy/resolve.py: every hub, each at
the commit of its tag of this release (`peppy-release/<tag>`) where it carries
one, and at the head of its `main` where it does not. The stages read the set
with the parser of resolve.py (see hub_ci.py). Every later stage reads the
commits the set records and never a branch, since `main` can move while the
release runs.

The `publish` stage uses the set here: it puts the tag of the release on every
hub, at the commit the set records, before it publishes the release.
"""

from __future__ import annotations

import json
from dataclasses import dataclass
from typing import Any

import httpx

from .cli import ReleaseError, console
from .github import RepoSlug, github_api
from .hub_ci import resolve


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
