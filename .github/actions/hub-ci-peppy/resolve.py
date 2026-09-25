#!/usr/bin/env python3
"""Pick the peppy build and the hub commits a hub's CI job runs against.

A change that spans several repositories lands as pull requests that share one
head branch name, the set name. A hub's CI job tests its own checkout with the
other hubs (its siblings) at the head of their branch of that name where they
have one, and at the head of `main` where they do not. The peppy it runs is
the latest release, or a peppy dev build (PEPPY_BUILD_POLICY), or the archive
of a peppy release run when the job is given one. A release run gives the
whole set as an explicit input instead, and the branch lookup is skipped.

Run without arguments, as the action runs it, the script installs that peppy
(unpacked whole under RUNNER_TEMP, its `bin` on the job's PATH), gives the job
a PEPPY_HOME, and writes the repositories.json5 the daemon reads there, before
any daemon starts. It reports the set to the job summary and to the action's
`set` output.

The peppy release (.github/workflows/parallel-release.yml) runs its
subcommand `release-set --tag <version> --output <file>`, which records the
set of hub commits the release tests and then tags: each hub at the commit of
its tag of the version, `peppy-release/<version>`, where it carries it, and at
the head of `main` where it does not. The step's `hubs` output names the hubs
of that set. The release scripts (scripts/functions/hub_ci.py) load this
module for everything else they know of the hubs: the hub list, the parser of
a release set, the hub tag of a release, the grammar of a release version,
and the checks of a hub at its commit.

The decisions are pure functions of their inputs (the event, the refs
`git ls-remote` reports, the REST responses), tested in test_resolve.py. The
I/O around them is kept thin: `git`, `urllib`, `tar` and the files the runner
reads its outputs from. Standard library only: it runs on the runner's python3.
"""

from __future__ import annotations

import argparse
import http.client
import json
import os
import re
import shutil
import subprocess
import sys
import urllib.error
import urllib.parse
import urllib.request
import zipfile
from dataclasses import dataclass
from enum import Enum
from pathlib import Path
from typing import Mapping, Sequence


class ResolveError(Exception):
    """A reason this job cannot run, worded for the job's log."""


# The hubs --------------------------------------------------------------------

ORGANISATION = "Peppy-bot"


class Visibility(Enum):
    PUBLIC = "public"
    PRIVATE = "private"


@dataclass(frozen=True)
class Hub:
    name: str
    # The id of the hub's entry in repositories.json5.
    repository_id: int
    visibility: Visibility
    # What `peppy repo index <checkout> --check` takes on top for this hub, as
    # the hub's own CI checks its index.
    index_arguments: tuple[str, ...] = ()

    @property
    def repository(self) -> str:
        return f"{ORGANISATION}/{self.name}"

    @property
    def clone_url(self) -> str:
        """Where peppy and `git ls-remote` read the hub: over https for a
        public hub, over ssh for a private one, authenticated by the deploy
        key the action loads into an ssh-agent."""
        if self.visibility is Visibility.PRIVATE:
            return f"git@github.com:{self.repository}.git"
        return f"https://github.com/{self.repository}.git"


# Every hub, the one list of them. The public ones are the defaults peppy
# bundles (crates/core-node-internal/assets/default_repositories.json5, which
# test_resolve.py holds this list to); the private one is the collection
# organisations add from the reserved id band. mcp-hub's index check also
# resolves every exposure against the contracts it names, which needs a daemon
# whose caches hold the contracts hub.
HUBS = (
    Hub("nodes-hub", 1000, Visibility.PUBLIC),
    Hub("launchers-hub", 1001, Visibility.PUBLIC),
    Hub("contracts-hub", 1002, Visibility.PUBLIC),
    Hub(
        "mcp-hub",
        1003,
        Visibility.PUBLIC,
        index_arguments=("--validate-mcp-exposures",),
    ),
    Hub("pairings-hub", 1004, Visibility.PUBLIC),
    Hub("private-nodes-hub", 2000, Visibility.PRIVATE),
)
HUBS_BY_NAME = {hub.name: hub for hub in HUBS}

# The branch every hub publishes from, and so the one a sibling hub falls back
# to when it has no branch of the set.
HUB_MAIN_BRANCH = "main"

# The peppy builds ------------------------------------------------------------


class PeppyBuildPolicy(Enum):
    """Which peppy a job installs when it is given no release run."""

    # The archive peppy.bot serves as the latest release.
    LATEST_RELEASE = "latest release"
    # The dev build of peppy's branch of the set when peppy has one, else the
    # dev build of the head of `dev`.
    DEV_BUILDS = "dev builds"


PEPPY_BUILD_POLICY = PeppyBuildPolicy.LATEST_RELEASE


class PeppyBuildKind(Enum):
    """The peppy a job installs, spelled as the `set` output spells it."""

    LATEST_RELEASE = "latest-release"
    DEV_BUILD = "dev-build"
    RELEASE_RUN = "release-run"


PEPPY_REPOSITORY = f"{ORGANISATION}/peppy"
PEPPY_CLONE_URL = f"https://github.com/{PEPPY_REPOSITORY}.git"
PEPPY_DEV_BRANCH = "dev"
# The integration branches of the hubs and of peppy. A pull request from one
# of them is not part of a set.
BRANCHES_WITHOUT_SET_NAME = frozenset({HUB_MAIN_BRANCH, PEPPY_DEV_BRANCH})
# The workflow whose install-archive job builds and uploads the dev build of
# every pull request and of every push to `dev`.
PEPPY_CI_WORKFLOW = "tests.yml"
# The dev build exists for x86_64 alone: install-archive builds on an x86_64
# runner.
DEV_BUILD_ARTIFACT = "peppy-dev-x86_64-unknown-linux-gnu"

GITHUB_API = "https://api.github.com"
USER_AGENT = "peppy-hub-ci"
API_TIMEOUT_SECONDS = 30
DOWNLOAD_TIMEOUT_SECONDS = 300

COMMIT_PATTERN = re.compile(r"[0-9a-f]{40}")


class Arch(Enum):
    X86_64 = "x86_64"
    AARCH64 = "aarch64"

    @property
    def triple(self) -> str:
        return f"{self.value}-unknown-linux-gnu"

    @property
    def archive_name(self) -> str:
        """The file name of peppy's archive for this architecture, in a
        release, on peppy.bot and inside every artifact that carries one."""
        return f"peppy-{self.triple}.tgz"


# `uname -m` as each architecture spells it.
MACHINE_ARCHES = {
    "x86_64": Arch.X86_64,
    "amd64": Arch.X86_64,
    "aarch64": Arch.AARCH64,
    "arm64": Arch.AARCH64,
}


def latest_release_url(arch: Arch) -> str:
    return f"https://peppy.bot/latest/{arch.archive_name}"


def release_run_artifact_name(arch: Arch) -> str:
    """The artifact a peppy release run uploads the archive of `arch` as."""
    return f"archive-{arch.triple}"


def run_url(run_id: int) -> str:
    return f"https://github.com/{PEPPY_REPOSITORY}/actions/runs/{run_id}"


# The event -------------------------------------------------------------------


@dataclass(frozen=True)
class Trigger:
    """What started the job, as far as the set is concerned."""

    set_name: str | None
    from_fork: bool
    # Why the job has the set name it has, for the job summary.
    explanation: str


def parse_trigger(event_name: str, payload: Mapping) -> Trigger:
    """The set name of an event: the head branch of a pull request.

    A push, a pull request from a fork, a pull request from an integration
    branch and every other event have none.
    """
    if event_name != "pull_request":
        return Trigger(None, False, f"this run is a `{event_name}`")
    try:
        head = payload["pull_request"]["head"]
        head_branch = head["ref"]
        # A fork deleted since the pull request opened has no repository.
        head_repository = (head.get("repo") or {}).get("full_name")
        base_repository = payload["pull_request"]["base"]["repo"]["full_name"]
    except (KeyError, TypeError) as error:
        raise ResolveError(f"the pull_request event lacks {error}") from error
    if head_repository != base_repository:
        return Trigger(None, True, "this pull request comes from a fork")
    if head_branch in BRANCHES_WITHOUT_SET_NAME:
        return Trigger(None, False, f"this pull request comes from `{head_branch}`")
    return Trigger(head_branch, False, "the head branch of this pull request")


# The hubs of the set ---------------------------------------------------------


class HubOrigin(Enum):
    """Where the commit of a hub in the set comes from."""

    CHECKOUT = "this checkout, the hub under test"
    SET_BRANCH = "its branch of the set"
    MAIN = "main"
    EXPLICIT_SET = "the explicit set"
    RELEASE_TAG = "its tag of this peppy release"


# The ref the set records for the hub under test.
CHECKOUT_REF = "checkout"


@dataclass(frozen=True)
class ResolvedHub:
    hub: Hub
    origin: HubOrigin
    # The branch, the explicit set's label, or CHECKOUT_REF.
    ref: str
    commit: str


@dataclass(frozen=True)
class HubSet:
    name: str | None
    # Ordered by repository id.
    hubs: tuple[ResolvedHub, ...]
    # What the job summary says about hubs left out of the set.
    notes: tuple[str, ...]


def hub_under_test(repository: str) -> Hub:
    """The hub whose CI runs, named by GITHUB_REPOSITORY."""
    for hub in HUBS:
        if hub.repository.casefold() == repository.casefold():
            return hub
    names = ", ".join(hub.repository for hub in HUBS)
    raise ResolveError(
        f"`{repository}` is not one of the hubs this action serves: {names}"
    )


def hub_is_readable(hub: Hub, private_key_loaded: bool) -> bool:
    """Whether this job can read `hub`: a private hub needs the deploy key."""
    return hub.visibility is Visibility.PUBLIC or private_key_loaded


def sibling_hubs(
    under_test: Hub, private_key_loaded: bool
) -> tuple[list[Hub], list[Hub]]:
    """The hubs next to the hub under test, by repository id: those this job
    reads, and those it leaves out of the set for want of the deploy key."""
    readable, unreadable = [], []
    for hub in HUBS:
        if hub is under_test:
            continue
        if hub_is_readable(hub, private_key_loaded):
            readable.append(hub)
        else:
            unreadable.append(hub)
    return readable, unreadable


def branches_to_look_up(fallback: str, set_name: str | None) -> list[str]:
    """The branches whose heads decide a repository's commit in the set."""
    return [fallback] if set_name is None else [fallback, set_name]


def parse_ls_remote(output: str) -> dict[str, str]:
    """Branch name to head commit, from the output of `git ls-remote`.

    `git ls-remote` matches its patterns against the tail of a ref, so a
    pattern `refs/heads/main` also matches a branch named `x/refs/heads/main`.
    Keying the result by the exact branch name makes every lookup exact.
    """
    heads = {}
    for line in output.splitlines():
        commit, _, ref = line.partition("\t")
        if ref.startswith("refs/heads/"):
            heads[ref.removeprefix("refs/heads/")] = commit
    return heads


def checkout_hub(hub: Hub, commit: str) -> ResolvedHub:
    return ResolvedHub(hub, HubOrigin.CHECKOUT, CHECKOUT_REF, commit)


def choose_sibling(
    hub: Hub, set_name: str | None, heads: Mapping[str, str]
) -> ResolvedHub:
    """A sibling at its branch of the set when it has one, else at `main`."""
    if set_name is not None and set_name in heads:
        return ResolvedHub(hub, HubOrigin.SET_BRANCH, set_name, heads[set_name])
    if HUB_MAIN_BRANCH not in heads:
        raise ResolveError(f"{hub.name} has no branch `{HUB_MAIN_BRANCH}`")
    return ResolvedHub(hub, HubOrigin.MAIN, HUB_MAIN_BRANCH, heads[HUB_MAIN_BRANCH])


def no_deploy_key_note(hub: Hub, set_name: str | None) -> str:
    note = f"{hub.name} is left out of this run: no deploy key was given."
    if set_name is None:
        return note
    return (
        f"{note} A branch `{set_name}` of {hub.name}, if any, is not part of this run."
    )


def choose_hubs_by_branch(
    under_test: Hub,
    checkout_commit: str,
    set_name: str | None,
    private_key_loaded: bool,
    heads_by_hub: Mapping[Hub, Mapping[str, str]],
) -> HubSet:
    """The set of a job without an explicit set.

    `heads_by_hub` holds the branch heads of every sibling hub.
    """
    readable, unreadable = sibling_hubs(under_test, private_key_loaded)
    siblings = [choose_sibling(hub, set_name, heads_by_hub[hub]) for hub in readable]
    notes = [no_deploy_key_note(hub, set_name) for hub in unreadable]
    return HubSet(
        name=set_name,
        hubs=ordered([checkout_hub(under_test, checkout_commit), *siblings]),
        notes=tuple(notes),
    )


def ordered(hubs: Sequence[ResolvedHub]) -> tuple[ResolvedHub, ...]:
    return tuple(sorted(hubs, key=lambda resolved: resolved.hub.repository_id))


# The explicit set ------------------------------------------------------------


@dataclass(frozen=True)
class PinnedHub:
    """A hub as an explicit set records it."""

    ref: str
    commit: str


EXPLICIT_SET_SHAPE = '{"hubs": {"<hub>": {"ref": "<label>", "commit": "<40 hex>"}}}'


def parse_explicit_set(text: str) -> dict[Hub, PinnedHub]:
    """The hubs an explicit set pins, by hub.

    The set must name every public hub; private-nodes-hub is optional.
    """
    try:
        document = json.loads(text)
    except json.JSONDecodeError as error:
        raise ResolveError(f"the explicit set is not JSON: {error}") from error
    if (
        not isinstance(document, dict)
        or set(document) != {"hubs"}
        or not isinstance(document["hubs"], dict)
    ):
        raise ResolveError(
            f"the explicit set must have the shape {EXPLICIT_SET_SHAPE}, got {text}"
        )
    pins = {
        parse_hub_name(name): parse_pinned_hub(name, entry)
        for name, entry in document["hubs"].items()
    }
    missing = [
        hub.name
        for hub in HUBS
        if hub.visibility is Visibility.PUBLIC and hub not in pins
    ]
    if missing:
        raise ResolveError(
            f"the explicit set leaves out {', '.join(missing)}; it must name "
            "every public hub"
        )
    return pins


def parse_hub_name(name: str) -> Hub:
    hub = HUBS_BY_NAME.get(name)
    if hub is None:
        names = ", ".join(HUBS_BY_NAME)
        raise ResolveError(
            f"the explicit set names an unknown hub `{name}`; the hubs are {names}"
        )
    return hub


def parse_pinned_hub(name: str, entry: object) -> PinnedHub:
    if (
        not isinstance(entry, dict)
        or set(entry) != {"ref", "commit"}
        or not all(isinstance(value, str) for value in entry.values())
        or not entry["ref"]
    ):
        raise ResolveError(
            f'the explicit set entry of {name} must have the shape {{"ref": '
            f'"<label>", "commit": "<40 hex>"}}, got {json.dumps(entry)}'
        )
    if not COMMIT_PATTERN.fullmatch(entry["commit"]):
        raise ResolveError(
            f"the explicit set records `{entry['commit']}` for {name}, which is "
            "not a 40-character lowercase hex commit"
        )
    return PinnedHub(ref=entry["ref"], commit=entry["commit"])


def choose_hubs_from_explicit_set(
    pins: Mapping[Hub, PinnedHub],
    under_test: Hub,
    checkout_commit: str,
    private_key_loaded: bool,
) -> HubSet:
    """The set of a job given an explicit set, which must record the commit
    the job checked out."""
    pinned_under_test = pins.get(under_test)
    if pinned_under_test is None:
        raise ResolveError(
            f"the explicit set leaves out {under_test.name}, the hub under test"
        )
    if pinned_under_test.commit != checkout_commit:
        raise ResolveError(
            f"the checkout of {under_test.name} is at {checkout_commit}, but the "
            f"set records {pinned_under_test.commit}"
        )
    siblings, notes = [], []
    for hub in HUBS:
        if hub is under_test:
            continue
        pin = pins.get(hub)
        if pin is None:
            notes.append(
                f"{hub.name} is left out of this run: the explicit set does not "
                "name it."
            )
        elif not hub_is_readable(hub, private_key_loaded):
            notes.append(
                f"{hub.name} is left out of this run: no deploy key was given, so "
                f"the commit the explicit set records for it, `{pin.commit}`, is "
                "not part of this run."
            )
        else:
            siblings.append(
                ResolvedHub(hub, HubOrigin.EXPLICIT_SET, pin.ref, pin.commit)
            )
    return HubSet(
        name=None,
        hubs=ordered([checkout_hub(under_test, checkout_commit), *siblings]),
        notes=tuple(notes),
    )


# The release set -------------------------------------------------------------

# A peppy release tags, in every hub, the commit it tested, and a release build
# of peppy `v0.31.2` reads the tag `peppy-release/v0.31.2` of each hub it
# bundles (PEPPY_RELEASE_TAG_PREFIX in
# peppy-shared/core-node-api/src/encoding/repo/git_ref.rs, which
# test_resolve.py holds this prefix to).
HUB_RELEASE_TAG_PREFIX = "peppy-release/"

# The one form of version a peppy release publishes and a peppy binary reads
# hub tags for (`PeppyBuild::from_git_tag` in peppy-shared/core-node-api):
# v<MAJOR>.<MINOR>.<PATCH>, with digits only in each part.
RELEASE_VERSION_PATTERN = re.compile(r"v[0-9]+\.[0-9]+\.[0-9]+")


def parse_release_version(text: str) -> str:
    """The release version `text` names, surrounding whitespace dropped."""
    version = text.strip()
    if not RELEASE_VERSION_PATTERN.fullmatch(version):
        raise ResolveError(
            f"`{text}` is not a peppy release version: a release is "
            "v<MAJOR>.<MINOR>.<PATCH>, with digits only in each part (example "
            "v0.31.2). Every published peppy binary reads the hub tags "
            f"{HUB_RELEASE_TAG_PREFIX}<its version>, and a binary of any other "
            "version reads the hubs' main instead of what the release tested."
        )
    return version


def hub_release_tag(version: str) -> str:
    """The tag a peppy release of `version` puts on each hub it tested."""
    return f"{HUB_RELEASE_TAG_PREFIX}{version}"


@dataclass(frozen=True)
class HubRefs:
    """What `git ls-remote` reports of a hub: its branch heads, and its tags,
    each at the commit it names."""

    heads: Mapping[str, str]
    tags: Mapping[str, str]


def release_ref_patterns(version: str) -> list[str]:
    """The refs of a hub the release set is chosen from.

    An annotated tag is an object of its own. `git ls-remote` reports the
    commit it names on a second line, the tag's name followed by `^{}`, and
    only for a pattern that names that line.
    """
    tag_ref = f"refs/tags/{hub_release_tag(version)}"
    return [f"refs/heads/{HUB_MAIN_BRANCH}", tag_ref, f"{tag_ref}^{{}}"]


def parse_ls_remote_tags(output: str) -> dict[str, str]:
    """Tag name to the commit the tag names, from the output of `git ls-remote`.

    A lightweight tag names its commit on its own line; an annotated tag names
    its tag object there, and the commit on its `^{}` line. As in
    `parse_ls_remote`, every tag is keyed by its exact name.
    """
    direct, peeled = {}, {}
    for line in output.splitlines():
        commit, _, ref = line.partition("\t")
        if not ref.startswith("refs/tags/"):
            continue
        name = ref.removeprefix("refs/tags/")
        if name.endswith("^{}"):
            peeled[name.removesuffix("^{}")] = commit
        else:
            direct[name] = commit
    return {name: peeled.get(name, commit) for name, commit in direct.items()}


def choose_release_hub(hub: Hub, version: str, refs: HubRefs) -> ResolvedHub:
    """A hub at the commit of its tag of this release when it carries it, else
    at the head of `main`."""
    tag = hub_release_tag(version)
    if tag in refs.tags:
        return ResolvedHub(hub, HubOrigin.RELEASE_TAG, tag, refs.tags[tag])
    if HUB_MAIN_BRANCH not in refs.heads:
        raise ResolveError(
            f"{hub.name} carries no tag `{tag}` and has no branch `{HUB_MAIN_BRANCH}`"
        )
    return ResolvedHub(
        hub, HubOrigin.MAIN, HUB_MAIN_BRANCH, refs.heads[HUB_MAIN_BRANCH]
    )


def tagged_hubs(hubs: Sequence[ResolvedHub]) -> list[ResolvedHub]:
    """The hubs that are at their tag of the release."""
    return [resolved for resolved in hubs if resolved.origin is HubOrigin.RELEASE_TAG]


def release_tag_note(version: str, tagged: Sequence[ResolvedHub]) -> str:
    """Why the hubs that already carry the tag of `version` are tested where it
    is, and what to do when they fail."""
    names = ", ".join(resolved.hub.name for resolved in tagged)
    verb = "carries" if len(tagged) == 1 else "carry"
    return (
        f"{names} already {verb} `{hub_release_tag(version)}`. Nobody can move "
        f"or delete a hub tag, so every run of {version} tests these hubs at the "
        "commits their tags name. If the checks of a run fail on one of these "
        f"commits, {version} cannot be released: start a new run with the next "
        "patch number."
    )


def choose_release_set(version: str, refs_by_hub: Mapping[Hub, HubRefs]) -> HubSet:
    """The hub commits a release of `version` tests and tags: every hub, each
    at its tag of this release where it carries it, else at the head of
    `main`."""
    hubs = ordered([choose_release_hub(hub, version, refs_by_hub[hub]) for hub in HUBS])
    tagged = tagged_hubs(hubs)
    return HubSet(
        name=None,
        hubs=hubs,
        notes=(release_tag_note(version, tagged),) if tagged else (),
    )


def require_private_hub_key(key_loaded: bool) -> None:
    """A release reads every hub, private-nodes-hub among them."""
    if key_loaded:
        return
    raise ResolveError(
        "the peppy release reads every hub, private-nodes-hub among them, but no "
        "deploy key is loaded: run load-private-hub-key.sh of this action with "
        "PRIVATE_NODES_HUB_DEPLOY_KEY first"
    )


def parse_release_set(text: str) -> HubSet:
    """A release set as `release-set` writes it: an explicit set that names
    every hub."""
    pins = parse_explicit_set(text)
    missing = [hub.name for hub in HUBS if hub not in pins]
    if missing:
        raise ResolveError(
            f"the release set leaves out {', '.join(missing)}; a release tests "
            "and tags every hub"
        )
    return HubSet(
        name=None,
        hubs=ordered(
            [
                ResolvedHub(hub, HubOrigin.EXPLICIT_SET, pin.ref, pin.commit)
                for hub, pin in pins.items()
            ]
        ),
        notes=(),
    )


def index_check_arguments(hub: Hub, checkout: Path) -> list[str]:
    """The arguments of `peppy` that check the repository index of `hub`,
    checked out at `checkout`."""
    return ["repo", "index", str(checkout), "--check", *hub.index_arguments]


def release_set_summary(version: str, hub_set: HubSet) -> str:
    """The job summary of `release-set`: every hub of the set."""
    lines = [
        f"### The hub commits peppy {version} tests and tags",
        "",
        f"Each hub is at `{hub_release_tag(version)}` where it carries that tag, "
        f"and at the head of `{HUB_MAIN_BRANCH}` where it does not. Every later "
        "job of this release uses these commits.",
        "",
        *hub_table(hub_set),
    ]
    if hub_set.notes:
        lines.append("")
        lines.extend(f"- {note}" for note in hub_set.notes)
    return "\n".join(lines) + "\n"


def hub_names(hub_set: HubSet) -> str:
    """The hubs of a set, comma-separated: the `repositories` of a token that
    reaches every one of them."""
    return ",".join(resolved.hub.name for resolved in hub_set.hubs)


# The choice of the peppy build -----------------------------------------------


def choose_peppy_build(
    policy: PeppyBuildPolicy, from_fork: bool, release_run_id: int | None
) -> PeppyBuildKind:
    """The peppy a job installs.

    A release run's archive wins over everything: the release is what starts
    such a job. A pull request from a fork runs without secrets, so it gets
    the latest release whatever the policy.
    """
    if release_run_id is not None:
        return PeppyBuildKind.RELEASE_RUN
    if from_fork or policy is PeppyBuildPolicy.LATEST_RELEASE:
        return PeppyBuildKind.LATEST_RELEASE
    return PeppyBuildKind.DEV_BUILD


def peppy_build_notes(kind: PeppyBuildKind, from_fork: bool) -> tuple[str, ...]:
    if from_fork and kind is PeppyBuildKind.LATEST_RELEASE:
        return (
            "A pull request from a fork gets no secrets, so it always runs the "
            "latest release of peppy.",
        )
    return ()


def runner_arch(machine: str, kind: PeppyBuildKind) -> Arch:
    """The architecture of the archive this runner installs.

    `machine` is what `uname -m` reports. No runner is ever given an archive of
    another architecture than its own.
    """
    arch = MACHINE_ARCHES.get(machine)
    if kind is PeppyBuildKind.DEV_BUILD and arch is not Arch.X86_64:
        raise ResolveError(
            f"peppy dev builds exist for x86_64 only (this runner is `{machine}`); "
            "run this job on an x86_64 runner."
        )
    if arch is None:
        raise ResolveError(f"peppy has no archive for this runner's `{machine}`")
    return arch


def choose_peppy_branch(
    set_name: str | None, heads: Mapping[str, str]
) -> tuple[str, str]:
    """The peppy branch whose dev build a job installs, and its head commit:
    peppy's branch of the set when it has one, else `dev`."""
    if set_name is not None and set_name in heads:
        return set_name, heads[set_name]
    if PEPPY_DEV_BRANCH not in heads:
        raise ResolveError(f"peppy has no branch `{PEPPY_DEV_BRANCH}`")
    return PEPPY_DEV_BRANCH, heads[PEPPY_DEV_BRANCH]


@dataclass(frozen=True)
class CiRun:
    id: int
    status: str
    url: str


def latest_ci_run(runs_response: Mapping, branch: str, commit: str) -> CiRun:
    """The most recent peppy CI run of `commit`, the head of `branch`.

    The most recent run is the one of the highest id: ids grow with every run
    GitHub creates.
    """
    runs = [
        CiRun(id=run["id"], status=run["status"], url=run["html_url"])
        for run in runs_response["workflow_runs"]
    ]
    if runs:
        return max(runs, key=lambda run: run.id)
    if branch == PEPPY_DEV_BRANCH:
        raise ResolveError(
            f"peppy branch `{branch}` has no dev build: no CI run exists for its "
            f"head `{commit}`."
        )
    raise ResolveError(
        f"peppy branch `{branch}` has no dev build; open a pull request for it."
    )


@dataclass(frozen=True)
class Artifact:
    id: int
    expired: bool
    download_url: str


def parse_artifacts(artifacts_response: Mapping, name: str) -> list[Artifact]:
    """The artifacts named `name` in a run's artifact list."""
    return [
        Artifact(
            id=artifact["id"],
            expired=artifact["expired"],
            download_url=artifact["archive_download_url"],
        )
        for artifact in artifacts_response["artifacts"]
        if artifact["name"] == name
    ]


def newest_usable(artifacts: Sequence[Artifact]) -> Artifact | None:
    """The newest artifact that has not expired. A re-run uploads a new
    artifact beside the expired one of the attempt before it."""
    usable = [artifact for artifact in artifacts if not artifact.expired]
    return max(usable, key=lambda artifact: artifact.id) if usable else None


def dev_build_artifact(
    run: CiRun, artifacts: Sequence[Artifact], branch: str, commit: str
) -> Artifact:
    """The dev build `run` uploaded.

    The artifact is used as soon as it is uploaded, whether or not the rest of
    the run has finished; a run that has not uploaded it yet is not waited for.
    """
    artifact = newest_usable(artifacts)
    if artifact is not None:
        return artifact
    if run.status != "completed":
        raise ResolveError(
            f"peppy dev build for `{commit}` is not ready (run `{run.url}` is "
            f"`{run.status}`); re-run this job when it finishes."
        )
    if artifacts:
        raise ResolveError(
            f"the peppy dev build for `{commit}` expired; re-run peppy CI run "
            f"`{run.url}`, or push a new commit to `{branch}` if GitHub no longer "
            "offers the re-run."
        )
    raise ResolveError(
        f"peppy CI run `{run.url}` produced no dev build; fix peppy CI first."
    )


def release_run_artifact(
    url: str, artifacts: Sequence[Artifact], name: str
) -> Artifact:
    """The archive a peppy release run uploaded for this runner."""
    artifact = newest_usable(artifacts)
    if artifact is not None:
        return artifact
    if artifacts:
        raise ResolveError(
            f"the artifact `{name}` of peppy release run `{url}` expired."
        )
    raise ResolveError(f"peppy release run `{url}` has no artifact `{name}`.")


@dataclass(frozen=True)
class PeppyBuild:
    """The peppy build a job installs, and where it comes from."""

    kind: PeppyBuildKind
    # The download URL, or the URL of the run the archive comes from.
    source: str
    # What the build is, for the job summary.
    description: str


@dataclass(frozen=True)
class InstalledPeppy:
    build: PeppyBuild
    # `peppy --version`.
    version: str


# What the job is given -------------------------------------------------------


def pinned_git_entry(resolved: ResolvedHub) -> dict:
    """The repositories.json5 entry of a hub read from its git repository at
    the commit of the set."""
    return {
        "id": resolved.hub.repository_id,
        "type": "git",
        "url": resolved.hub.clone_url,
        "ref": resolved.commit,
    }


def repositories_json(entries: Sequence[dict]) -> str:
    """A repositories.json5 as plain JSON, which JSON5 reads."""
    return json.dumps(list(entries), indent=2) + "\n"


def repositories_file(hub_set: HubSet, hub_path: str) -> str:
    """The repositories.json5 of the job's daemon, ordered by id.

    Every hub is at its bundled id, so the daemon adds none of its defaults on
    top. The hub under test is the checkout itself; every other hub is its
    git repository pinned at the commit of the set.
    """
    return repositories_json(
        [
            {"id": resolved.hub.repository_id, "type": "fs", "path": hub_path}
            if resolved.origin is HubOrigin.CHECKOUT
            else pinned_git_entry(resolved)
            for resolved in hub_set.hubs
        ]
    )


def release_repositories_file(hub_set: HubSet) -> str:
    """The repositories.json5 of a daemon that reads a release set: every hub
    at its id, its git repository pinned at the commit of the set."""
    return repositories_json([pinned_git_entry(resolved) for resolved in hub_set.hubs])


def hubs_document(hub_set: HubSet) -> dict:
    """The hubs of a set as an explicit set spells them."""
    return {
        resolved.hub.name: {"ref": resolved.ref, "commit": resolved.commit}
        for resolved in hub_set.hubs
    }


def set_output(hub_set: HubSet, peppy: InstalledPeppy) -> dict:
    """The `set` output of the action."""
    return {
        "name": hub_set.name,
        "peppy": {
            "build": peppy.build.kind.value,
            "version": peppy.version,
            "source": peppy.build.source,
        },
        "hubs": hubs_document(hub_set),
    }


def release_set_document(hub_set: HubSet) -> dict:
    """A release set in the schema of an explicit set, which the release
    passes to launchers-hub's tests as it is."""
    return {"hubs": hubs_document(hub_set)}


def compact_json(value: object) -> str:
    return json.dumps(value, separators=(",", ":"))


def hub_table(hub_set: HubSet) -> list[str]:
    """The lines of the job summary's table of the hubs of a set."""
    lines = [
        "| Hub | Id | Came from | Branch or ref | Commit |",
        "| --- | --- | --- | --- | --- |",
    ]
    lines.extend(
        f"| {resolved.hub.name} | {resolved.hub.repository_id} "
        f"| {resolved.origin.value} | `{resolved.ref}` | `{resolved.commit}` |"
        for resolved in hub_set.hubs
    )
    return lines


def summary_markdown(
    hub_set: HubSet,
    peppy: InstalledPeppy,
    set_explanation: str,
    peppy_notes: Sequence[str],
) -> str:
    """The job summary: the peppy build, every hub of the set, and what the
    job leaves out and why."""
    if hub_set.name is None:
        heading = f"No set name: {set_explanation}."
    else:
        heading = f"Set `{hub_set.name}`: {set_explanation}."
    lines = [
        "### The peppy and the hubs this job runs",
        "",
        heading,
        "",
        f"**peppy**: `{peppy.version}`, {peppy.build.description} "
        f"({peppy.build.source})",
        "",
        *hub_table(hub_set),
    ]
    notes = [*peppy_notes, *hub_set.notes]
    if notes:
        lines.append("")
        lines.extend(f"- {note}" for note in notes)
    return "\n".join(lines) + "\n"


# I/O -------------------------------------------------------------------------


def git_environment() -> dict[str, str]:
    """The environment of a git command that reads a remote. A job has no
    terminal: an unauthenticated read fails rather than waiting for a
    password."""
    return {
        **os.environ,
        "GIT_TERMINAL_PROMPT": "0",
        "GIT_SSH_COMMAND": "ssh -o BatchMode=yes",
    }


def run_git(arguments: Sequence[str]) -> str:
    """What `git <arguments>` prints; a git that fails stops the job."""
    result = subprocess.run(
        ["git", *arguments], capture_output=True, text=True, env=git_environment()
    )
    if result.returncode != 0:
        raise ResolveError(f"git {' '.join(arguments)} failed: {result.stderr.strip()}")
    return result.stdout


def ls_remote(url: str, patterns: Sequence[str]) -> str:
    """What `git ls-remote` reports for `patterns` in the repository at `url`."""
    return run_git(["ls-remote", url, *patterns])


def ls_remote_heads(url: str, branches: Sequence[str]) -> dict[str, str]:
    """The heads of `branches` in the repository at `url`; a branch that does
    not exist is absent from the result."""
    return parse_ls_remote(
        ls_remote(url, [f"refs/heads/{branch}" for branch in branches])
    )


def ls_remote_release_refs(hub: Hub, version: str) -> HubRefs:
    """The head of `main` of `hub` and its tag of the release of `version`,
    each absent from the result when the hub has none."""
    output = ls_remote(hub.clone_url, release_ref_patterns(version))
    return HubRefs(heads=parse_ls_remote(output), tags=parse_ls_remote_tags(output))


def agent_holds_a_key() -> bool:
    """Whether an ssh-agent this process reaches holds a key, which is what
    load-private-hub-key.sh leaves."""
    try:
        result = subprocess.run(["ssh-add", "-l"], capture_output=True, text=True)
    except FileNotFoundError:
        return False
    return result.returncode == 0


def read_release_set(version: str) -> HubSet:
    """The release set of `version`, read from every hub."""
    require_private_hub_key(agent_holds_a_key())
    return choose_release_set(
        version, {hub: ls_remote_release_refs(hub, version) for hub in HUBS}
    )


def read_release_set_file(path: Path) -> HubSet:
    try:
        text = path.read_text()
    except OSError as error:
        raise ResolveError(f"the release set {path} is unreadable: {error}") from error
    return parse_release_set(text)


def check_out_commit(resolved: ResolvedHub, destination: Path) -> None:
    """Check out `resolved` at its commit in `destination`, fetching that commit
    alone, whatever branch or tag holds it now."""
    shutil.rmtree(destination, ignore_errors=True)
    destination.mkdir(parents=True)
    run_git(["init", "--quiet", str(destination)])
    run_git(
        [
            "-C",
            str(destination),
            "fetch",
            "--quiet",
            "--depth",
            "1",
            resolved.hub.clone_url,
            resolved.commit,
        ]
    )
    run_git(["-C", str(destination), "checkout", "--quiet", "--detach", "FETCH_HEAD"])
    head = run_git(["-C", str(destination), "rev-parse", "HEAD"]).strip()
    if head != resolved.commit:
        raise ResolveError(
            f"the checkout of {resolved.hub.name} is at {head}, not at "
            f"{resolved.commit}"
        )


def checkout_head(hub_path: str) -> str:
    return run_git(["-C", hub_path, "rev-parse", "HEAD"]).strip()


def github_request(url: str, token: str) -> urllib.request.Request:
    """A GitHub REST request. The token goes to GitHub alone: an artifact
    download redirects to a storage URL that is signed on its own and refuses
    a second credential, so the header is not carried across redirects."""
    if not token:
        raise ResolveError(
            "the GitHub API needs a token; the `github-token` input is empty"
        )
    request = urllib.request.Request(
        url,
        headers={
            "Accept": "application/vnd.github+json",
            "X-GitHub-Api-Version": "2022-11-28",
            "User-Agent": USER_AGENT,
        },
    )
    request.add_unredirected_header("Authorization", f"Bearer {token}")
    return request


def open_url(
    request: urllib.request.Request, timeout: float, action: str
) -> http.client.HTTPResponse:
    """The response to `request`. A request that fails stops the job, the
    message naming `action`."""
    try:
        return urllib.request.urlopen(request, timeout=timeout)
    except urllib.error.HTTPError as error:
        raise ResolveError(
            f"{action} failed: HTTP {error.code} {error.reason}"
        ) from error
    except urllib.error.URLError as error:
        raise ResolveError(f"{action} failed: {error.reason}") from error


def github_get_json(path: str, query: Mapping[str, object], token: str) -> dict:
    url = f"{GITHUB_API}{path}"
    if query:
        url = f"{url}?{urllib.parse.urlencode(query)}"
    request = github_request(url, token)
    with open_url(request, API_TIMEOUT_SECONDS, f"GET {url}") as response:
        return json.load(response)


def download(request: urllib.request.Request, destination: Path) -> None:
    action = f"downloading {request.full_url}"
    with (
        open_url(request, DOWNLOAD_TIMEOUT_SECONDS, action) as response,
        destination.open("wb") as file,
    ):
        shutil.copyfileobj(response, file)


def run_artifacts(run_id: int, name: str, token: str) -> list[Artifact]:
    response = github_get_json(
        f"/repos/{PEPPY_REPOSITORY}/actions/runs/{run_id}/artifacts",
        {"name": name, "per_page": 100},
        token,
    )
    return parse_artifacts(response, name)


def download_artifact_archive(
    artifact: Artifact, arch: Arch, token: str, work_dir: Path
) -> Path:
    """Download `artifact`, a zip that holds peppy's archive for `arch`, and
    return the archive."""
    bundle_path = work_dir / f"artifact-{artifact.id}.zip"
    download(github_request(artifact.download_url, token), bundle_path)
    with zipfile.ZipFile(bundle_path) as bundle:
        if arch.archive_name not in bundle.namelist():
            raise ResolveError(
                f"the artifact {artifact.download_url} holds "
                f"{', '.join(bundle.namelist()) or 'nothing'}, not {arch.archive_name}"
            )
        archive = Path(bundle.extract(arch.archive_name, work_dir))
    bundle_path.unlink()
    return archive


@dataclass(frozen=True)
class FetchedPeppy:
    build: PeppyBuild
    archive: Path


def fetch_latest_release(arch: Arch, work_dir: Path) -> FetchedPeppy:
    url = latest_release_url(arch)
    archive = work_dir / arch.archive_name
    download(urllib.request.Request(url, headers={"User-Agent": USER_AGENT}), archive)
    return FetchedPeppy(
        PeppyBuild(PeppyBuildKind.LATEST_RELEASE, url, "the latest release"), archive
    )


def fetch_dev_build(set_name: str | None, token: str, work_dir: Path) -> FetchedPeppy:
    heads = ls_remote_heads(
        PEPPY_CLONE_URL, branches_to_look_up(PEPPY_DEV_BRANCH, set_name)
    )
    branch, commit = choose_peppy_branch(set_name, heads)
    runs = github_get_json(
        f"/repos/{PEPPY_REPOSITORY}/actions/workflows/{PEPPY_CI_WORKFLOW}/runs",
        {"head_sha": commit, "per_page": 100},
        token,
    )
    run = latest_ci_run(runs, branch, commit)
    artifact = dev_build_artifact(
        run, run_artifacts(run.id, DEV_BUILD_ARTIFACT, token), branch, commit
    )
    archive = download_artifact_archive(artifact, Arch.X86_64, token, work_dir)
    return FetchedPeppy(
        PeppyBuild(
            PeppyBuildKind.DEV_BUILD,
            run.url,
            f"the dev build of peppy branch `{branch}` at `{commit}`",
        ),
        archive,
    )


def fetch_release_run(
    run_id: int, arch: Arch, token: str, work_dir: Path
) -> FetchedPeppy:
    name = release_run_artifact_name(arch)
    url = run_url(run_id)
    artifact = release_run_artifact(url, run_artifacts(run_id, name, token), name)
    archive = download_artifact_archive(artifact, arch, token, work_dir)
    return FetchedPeppy(
        PeppyBuild(
            PeppyBuildKind.RELEASE_RUN, url, "the archive of a peppy release run"
        ),
        archive,
    )


def unpack_peppy(archive: Path, destination: Path) -> Path:
    """Unpack the whole archive, which the daemon needs around its binary (the
    bundled zenohd and apptainer), and return the `peppy` binary."""
    shutil.rmtree(destination, ignore_errors=True)
    destination.mkdir(parents=True)
    result = subprocess.run(
        ["tar", "-xzf", str(archive), "-C", str(destination)],
        capture_output=True,
        text=True,
    )
    if result.returncode != 0:
        raise ResolveError(f"unpacking {archive} failed: {result.stderr.strip()}")
    peppy = destination / "bin" / "peppy"
    if not peppy.is_file():
        raise ResolveError(f"the peppy archive {archive} holds no bin/peppy")
    return peppy


def peppy_version(peppy: Path) -> str:
    result = subprocess.run([str(peppy), "--version"], capture_output=True, text=True)
    version = result.stdout.strip()
    if result.returncode != 0 or not version or "\n" in version:
        raise ResolveError(
            f"`{peppy} --version` printed {result.stdout!r} and exited "
            f"{result.returncode}: {result.stderr.strip()}"
        )
    return version


def append_to_runner_file(variable: str, text: str) -> None:
    """Append to one of the files the runner reads commands from
    (GITHUB_ENV, GITHUB_PATH, GITHUB_OUTPUT, GITHUB_STEP_SUMMARY)."""
    path = os.environ.get(variable)
    if not path:
        raise ResolveError(
            f"{variable} is not set; this script runs in a GitHub Actions step"
        )
    with open(path, "a") as handle:
        handle.write(text)


def escape_workflow_command(text: str) -> str:
    """`text` as the data of a workflow command such as `::error::`."""
    return text.replace("%", "%25").replace("\r", "%0D").replace("\n", "%0A")


# The action ------------------------------------------------------------------


@dataclass(frozen=True)
class ActionInputs:
    """The action's inputs and the runner's environment, parsed once."""

    hub_path: str
    explicit_set: dict[Hub, PinnedHub] | None
    release_run_id: int | None
    github_token: str
    private_key_loaded: bool
    under_test: Hub
    trigger: Trigger
    machine: str
    runner_temp: Path


def parse_hub_path(text: str) -> str:
    if not text:
        raise ResolveError("the `hub-path` input is empty")
    return os.path.abspath(text)


def parse_release_run_id(text: str) -> int | None:
    if not text:
        return None
    if not text.isdigit():
        raise ResolveError(f"`peppy-run-id` must be a workflow run id, got `{text}`")
    return int(text)


def parse_flag(name: str, text: str) -> bool:
    if text not in ("true", "false"):
        raise ResolveError(f"{name} must be `true` or `false`, got `{text}`")
    return text == "true"


def read_event_payload(path: str) -> dict:
    try:
        with open(path) as handle:
            return json.load(handle)
    except (OSError, json.JSONDecodeError) as error:
        raise ResolveError(
            f"the event payload {path!r} is unreadable: {error}"
        ) from error


def required_environment(name: str) -> str:
    value = os.environ.get(name, "")
    if not value:
        raise ResolveError(
            f"{name} is not set; this script runs in a GitHub Actions step"
        )
    return value


# The variables action.yml gives this script, on top of the runner's own. An
# input left empty arrives as the empty string.
ACTION_VARIABLES = (
    "INPUT_HUB_PATH",
    "INPUT_SET",
    "INPUT_PEPPY_RUN_ID",
    "INPUT_GITHUB_TOKEN",
    "PRIVATE_HUB_KEY_LOADED",
)


def inputs_from_environment() -> ActionInputs:
    action = {name: os.environ.get(name, "").strip() for name in ACTION_VARIABLES}
    return ActionInputs(
        hub_path=parse_hub_path(action["INPUT_HUB_PATH"]),
        explicit_set=(
            parse_explicit_set(action["INPUT_SET"]) if action["INPUT_SET"] else None
        ),
        release_run_id=parse_release_run_id(action["INPUT_PEPPY_RUN_ID"]),
        github_token=action["INPUT_GITHUB_TOKEN"],
        private_key_loaded=parse_flag(
            "PRIVATE_HUB_KEY_LOADED", action["PRIVATE_HUB_KEY_LOADED"]
        ),
        under_test=hub_under_test(required_environment("GITHUB_REPOSITORY")),
        trigger=parse_trigger(
            required_environment("GITHUB_EVENT_NAME"),
            read_event_payload(required_environment("GITHUB_EVENT_PATH")),
        ),
        machine=os.uname().machine,
        runner_temp=Path(required_environment("RUNNER_TEMP")),
    )


def resolve_hub_set(inputs: ActionInputs, checkout_commit: str) -> HubSet:
    if inputs.explicit_set is not None:
        return choose_hubs_from_explicit_set(
            inputs.explicit_set,
            inputs.under_test,
            checkout_commit,
            inputs.private_key_loaded,
        )
    set_name = inputs.trigger.set_name
    branches = branches_to_look_up(HUB_MAIN_BRANCH, set_name)
    readable, _ = sibling_hubs(inputs.under_test, inputs.private_key_loaded)
    heads_by_hub = {hub: ls_remote_heads(hub.clone_url, branches) for hub in readable}
    return choose_hubs_by_branch(
        inputs.under_test,
        checkout_commit,
        set_name,
        inputs.private_key_loaded,
        heads_by_hub,
    )


def fetch_peppy(
    kind: PeppyBuildKind,
    arch: Arch,
    inputs: ActionInputs,
    set_name: str | None,
    work_dir: Path,
) -> FetchedPeppy:
    if kind is PeppyBuildKind.RELEASE_RUN:
        return fetch_release_run(
            inputs.release_run_id, arch, inputs.github_token, work_dir
        )
    if kind is PeppyBuildKind.DEV_BUILD:
        return fetch_dev_build(set_name, inputs.github_token, work_dir)
    return fetch_latest_release(arch, work_dir)


def install_peppy(
    kind: PeppyBuildKind, arch: Arch, inputs: ActionInputs, set_name: str | None
) -> InstalledPeppy:
    """Fetch the peppy archive, unpack it whole and put its `bin` on the PATH
    of the job's later steps."""
    work_dir = inputs.runner_temp / "peppy-download"
    shutil.rmtree(work_dir, ignore_errors=True)
    work_dir.mkdir(parents=True)
    fetched = fetch_peppy(kind, arch, inputs, set_name, work_dir)
    dist_dir = inputs.runner_temp / "peppy-dist"
    peppy = unpack_peppy(fetched.archive, dist_dir)
    shutil.rmtree(work_dir)
    append_to_runner_file("GITHUB_PATH", f"{dist_dir / 'bin'}\n")
    return InstalledPeppy(build=fetched.build, version=peppy_version(peppy))


def write_repositories_file(peppy_home: Path, text: str) -> None:
    """Write the repositories.json5 a daemon with the data root `peppy_home`
    reads."""
    conf_dir = peppy_home / "conf"
    conf_dir.mkdir(parents=True, exist_ok=True)
    (conf_dir / "repositories.json5").write_text(text)


def write_peppy_home(hub_set: HubSet, inputs: ActionInputs) -> None:
    """Give the job's later steps a PEPPY_HOME whose repositories.json5 holds
    the hubs of the set, for the daemon they start."""
    peppy_home = inputs.runner_temp / "peppy-home"
    write_repositories_file(peppy_home, repositories_file(hub_set, inputs.hub_path))
    append_to_runner_file("GITHUB_ENV", f"PEPPY_HOME={peppy_home}\n")


def report_set(
    hub_set: HubSet, installed: InstalledPeppy, inputs: ActionInputs
) -> None:
    """Write the set to the log, the action's outputs and the job summary."""
    resolved_set = compact_json(set_output(hub_set, installed))
    print(f"Installed {installed.version} ({installed.build.source})")
    print(f"set: {resolved_set}")
    append_to_runner_file(
        "GITHUB_OUTPUT", f"set={resolved_set}\npeppy-version={installed.version}\n"
    )
    set_explanation = (
        "the explicit `set` input names every hub"
        if inputs.explicit_set is not None
        else inputs.trigger.explanation
    )
    append_to_runner_file(
        "GITHUB_STEP_SUMMARY",
        summary_markdown(
            hub_set,
            installed,
            set_explanation,
            peppy_build_notes(installed.build.kind, inputs.trigger.from_fork),
        ),
    )


def run(inputs: ActionInputs) -> None:
    kind = choose_peppy_build(
        PEPPY_BUILD_POLICY, inputs.trigger.from_fork, inputs.release_run_id
    )
    arch = runner_arch(inputs.machine, kind)
    hub_set = resolve_hub_set(inputs, checkout_head(inputs.hub_path))
    installed = install_peppy(kind, arch, inputs, hub_set.name)
    write_peppy_home(hub_set, inputs)
    report_set(hub_set, installed, inputs)


# The release subcommand ------------------------------------------------------


def run_release_set(version: str, output: Path) -> None:
    """Record the release set of `version` in `output`, name its hubs in the
    step's `hubs` output, and report it in the log and the job summary."""
    hub_set = read_release_set(version)
    output.parent.mkdir(parents=True, exist_ok=True)
    output.write_text(json.dumps(release_set_document(hub_set), indent=2) + "\n")
    append_to_runner_file("GITHUB_OUTPUT", f"hubs={hub_names(hub_set)}\n")
    summary = release_set_summary(version, hub_set)
    print(summary, end="")
    print(f"set: {compact_json(release_set_document(hub_set))}")
    append_to_runner_file("GITHUB_STEP_SUMMARY", summary)


# The command line ------------------------------------------------------------


def parse_arguments(argv: Sequence[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description=(
            "Without a subcommand, what the hub-ci-peppy action runs: the set "
            "of a hub's CI job, and its peppy. The subcommand serves the peppy "
            "release."
        )
    )
    commands = parser.add_subparsers(dest="command", metavar="subcommand")
    release_set = commands.add_parser(
        "release-set",
        help="Record the hub commits a peppy release tests and tags.",
    )
    release_set.add_argument(
        "--tag", required=True, help="The peppy release (example v0.31.2)."
    )
    release_set.add_argument(
        "--output", required=True, type=Path, help="Where to write the release set."
    )
    return parser.parse_args(argv)


def run_command(arguments: argparse.Namespace) -> None:
    match arguments.command:
        case None:
            run(inputs_from_environment())
        case "release-set":
            run_release_set(parse_release_version(arguments.tag), arguments.output)


def main(argv: Sequence[str]) -> int:
    arguments = parse_arguments(argv)
    try:
        run_command(arguments)
    except ResolveError as error:
        print(f"::error::{escape_workflow_command(str(error))}")
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
