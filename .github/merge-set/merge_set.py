#!/usr/bin/env python3
"""Merge the pull requests of a set together.

A change that spans several repositories lands as pull requests that share one
head branch name, the set name (see .github/actions/hub-ci-peppy/resolve.py).
The CI of each hub pull request tests it with the branches of the set in peppy
and in the other hubs, so the pull requests of a set are green together and
merge together. Two other repositories are part of a set too, so that their
`main` and the `main` of the hubs change together: public-peppy-libs, the
libraries that the nodes of the hubs build on, and platform, whose simulation
chat drives a stack through the MCP endpoints that peppy and the hubs serve and
shows the viewer page of the simulation. No hub CI reads their branches of a
set: a hub builds the libraries at the commits that its lockfiles pin, and
reads nothing of the platform.

The `merge-set` commit status is a required check of peppy `dev` and of the
`main` of each other repository of a set, and only the merge-set GitHub App
reports it:

- A pull request that is alone, because no other repository has a change on
  its branch, gets a green status, and its own merge button merges it.
- A pull request of a set gets a pending status, so its merge button stays
  blocked, and a comment, the dashboard, that lists every repository of the
  set with the state of the CI run of its head, says what blocks the merge,
  and holds a "Merge" box, and a "Re-run the out-of-date CI" box while a hub
  run of the set is out of date.

A pull request of the set that merged before the rest of it, by its own merge
button or by a merge of the set that stopped part way, stays in the set as
merged: the dashboard lists it with the state `merged`, and the bot keeps its
dashboard in step with the others. A merged pull request from the branch is
part of the set when it was open at the same time as an open pull request of
the set, or as another merged pull request that is part of the set. One that
merged before every pull request of the set opened belongs to an earlier set
of the same name.

Ticking "Merge" asks the bot to merge the set. The bot checks every pull
request of the set, sets the status green on each, checks that GitHub merges
each one, and merges them one after the other, peppy first and platform last.
It squashes a platform pull request into one commit titled as the pull
request, as the platform merges each one, and merges the pull request of each
other repository with a merge commit.
GitHub merges one pull request at a time, so a merge that fails part way
leaves the set partly merged: the bot then blocks the pull requests that did
not merge and reports what merged on every pull request of the set.

The bot acts on a ticked box only for the user who opened every pull request
of the set and has write access to every repository of it, or for a user with
the admin role on every repository of the set. The user who ticked a box is
the one who last edited the dashboard. To any other user who ticks a box, the
bot answers why it does nothing.

A user with the admin role on every repository of the set merges it without
the approvals it lacks, as the rulesets let an admin merge a pull request of
their own. The App is a bypass actor, for pull requests, of the rulesets that
require an approval, so the bot enforces the approvals itself: a user
without that role gets no merge of a set that lacks one. The bot reads, for
each pull request that lacks an approval, whether the App bypasses every
ruleset that requires one. When it does not, GitHub refuses the merge of the
App, so that pull request blocks the set for every user, before the bot merges
any pull request of it. A review that requests changes blocks every user.

A hub run is out of date when a job of it ran with another commit of a branch
of the set than the head of that branch, unless that head differs from the
commit by a clean merge of its base alone. The head of a branch that merged is
the head of the pull request that merged it. GitHub deletes that branch, and a
job then reads the base of its repository in its place: a job that ran that
base at a commit that holds the head that merged ran the change of the branch,
so it is not out of date. A job that ran the branch of the set of a repository
that is no longer part of the set, because that branch was deleted (as when its
pull request closed without a merge), is out of date: the set merges with the
base of that repository, which the job did not run, and a re-run reads that
base. A branch left behind by an earlier set makes no job out of date: it
merged before the set opened, and a re-run reads it all the same. The bot does
not ask a pull request to be up to date with its base, so a branch that merges
its base in does not ask a re-run either: the merge of the set takes in that
base all the same. The bot asks git to merge the commit and the base again, in
a scratch repository (GitMerges), and compares the result with the head, so a
conflict resolved by hand or any other change of the branch counts. A job reads
the branches of peppy and the hubs alone, so a change of the public-peppy-libs
or platform branch of a set makes no hub run out of date. Every job that runs
the hub-ci-peppy action uploads the set it ran with, its set record, named
after the check run of the job. A job that does not run the action reads no
other repository, so it has no record and is never out of date; nor has the
first job of each hub run, which runs the action with `wait-only` to wait for
the peppy dev build of the set and installs nothing. Ticking "Re-run" re-runs
every finished hub run that is out of date, at once: a re-run whose peppy dev
build is not uploaded yet waits for it in that first job.

Claude can review a set and approve it, so that a developer does not wait for
the review of a person without end. When each open pull request of a set was
opened by a member of the team `peppy-devs`, the dashboard has the section
"Review by Claude", and the bot writes that dashboard on a pull request that
is alone too: a pull request that is alone is a set of one. A person has 12
hours to review the set first, from the time each of its open pull requests
is ready for review. During that time the section shows the time left, and
no box. A pull request that joins the set, or that leaves the draft state,
starts the 12 hours again. When a person who did not open a pull request of
the set and has write access to its repository reviews one of them before
the 12 hours end, a person reviews the set and the section gets no box.
Otherwise the section gets a box that asks for the review when the 12 hours
end, and the bot tells the authors of the set in a comment, because an edit
of the dashboard sends no notification. A review of a person after that does
not remove the box. No event of GitHub comes when the time passes, so the
workflow merge-set-wait.yml runs `wait-syncs` every 30 minutes: it starts the
sync of each set that waits, until one hour after the end of its 12 hours.
The bot acts on the box for the same users as on its other boxes. It starts
the workflow `Claude review` of the private repository
peppy-superproject (claude_review.py runs in it) when no pull request of the
set has a conflict with its base and no review of the set is in progress, and
answers with the cause in each other case. The reviewer, a GitHub App of its
own, posts its findings as reviews of the type "comment" and approves each
pull request of the set when no blocking finding is open. The section shows
the state of the review: none asked, in progress, the open blocking findings,
approved, or not completed with the cause. When the head of a pull request
moves away from the commit that the reviewer approved, the bot dismisses that
approval before it writes the `merge-set` status of the new head, unless the
head differs from that commit by a clean merge of its base alone. The bot never
dismisses the review of a person.

GitHub sends every event of each repository of a set to the webhook of the
merge-set App (webhook.py, in AWS Lambda), which hands it to the relay
(`relay`). For an event that can change the set, the relay starts the Merge
set workflow of peppy (merge-set.yml) for the branch of the event. The start
and the end of each CI run, a re-run included, are such events, so the
dashboard follows the state of the CI. So is the end of each run of the PR
title check (pr-title), so the dashboard follows that check when a title
changes, and so are the reviews, so it follows the approvals. So is the end of
each review run of peppy-superproject, so the dashboard follows a review that
did not complete. When the event is a box that a user ticked, the relay also
answers at once on that pull request, with a link to the run it started. The
Merge set workflow runs `sync`, one run at a time for each set. `sync` holds
no state of its own: it reads the whole set from GitHub on every run (the
branches, the pull requests, their checks, CI runs and reviews, the set
records of the hub runs, the review comments and the review runs of the
reviewer, the dashboards and the user who last edited each one), so a run that
GitHub drops from its queue loses nothing: the next run of the set finds the
same ticked box.

The decisions are pure functions of their inputs, tested in test_merge_set.py.
The I/O around them is kept thin, in GitHubGateway and GitMerges. Standard
library only: the sync runs on the runner's python3 and git, and the relay in
the Python runtime of AWS Lambda.
"""

from __future__ import annotations

import argparse
import base64
import io
import json
import os
import re
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.parse
import urllib.request
import zipfile
from collections.abc import Callable, Collection, Mapping, Sequence
from dataclasses import dataclass
from datetime import UTC, datetime, timedelta
from enum import Enum
from pathlib import Path

# resolve.py of the hub-ci-peppy action holds the one model of the hubs, of the
# set name and of the set records, and the GitHub request every script of
# this repository makes.
sys.path.insert(
    0, str(Path(__file__).resolve().parents[1] / "actions" / "hub-ci-peppy")
)
import resolve


class MergeSetError(Exception):
    """A reason this run cannot go on, worded for the run's log."""


class ApiError(MergeSetError):
    """A GitHub API call that failed."""

    def __init__(self, action: str, status: int | None, message: str):
        prefix = f"HTTP {status}: " if status is not None else ""
        super().__init__(f"{action} failed: {prefix}{message}")
        self.status = status
        self.message = message


class MergeRefused(MergeSetError):
    """GitHub did not merge a pull request, worded as GitHub worded it."""


# The repositories -----------------------------------------------------------


class MergeMethod(Enum):
    """How GitHub merges a pull request, as its merge endpoint names it."""

    # The commits of the pull request, under a merge commit.
    MERGE = "merge"
    # One commit that holds every change of the pull request.
    SQUASH = "squash"


@dataclass(frozen=True)
class Repository:
    name: str
    # The branch every pull request of a set targets in this repository.
    integration_branch: str
    # The hub this repository is; None for a repository that is not a hub.
    hub: resolve.Hub | None
    # How the bot merges the pull request of a set in this repository.
    merge_method: MergeMethod = MergeMethod.MERGE

    @property
    def full_name(self) -> str:
        return f"{resolve.ORGANISATION}/{self.name}"


PEPPY = Repository("peppy", resolve.PEPPY_DEV_BRANCH, None)
# The libraries that the nodes of the hubs build on. No hub CI reads its branch
# of a set.
PUBLIC_PEPPY_LIBS = Repository("public-peppy-libs", "main", None)
# The platform, whose simulation chat drives a stack through the MCP endpoints
# that peppy and the hubs serve and shows the viewer page of the simulation. No
# hub CI reads it. Each commit of its `main` is one squashed pull request, so
# that a deploy names one change.
PLATFORM = Repository("platform", "main", None, MergeMethod.SQUASH)

# Every repository a set can span, in the order the bot merges them: peppy
# first, then public-peppy-libs, which the hubs build on, then the hubs in the
# order of their repository ids, then platform, which uses what they serve.
# The order is fixed, so every report of a merge reads the same way.
REPOSITORIES = (
    PEPPY,
    PUBLIC_PEPPY_LIBS,
    *(Repository(hub.name, resolve.HUB_MAIN_BRANCH, hub) for hub in resolve.HUBS),
    PLATFORM,
)
REPOSITORIES_BY_NAME = {repository.name: repository for repository in REPOSITORIES}


def repository_named(name: str) -> Repository:
    repository = REPOSITORIES_BY_NAME.get(name)
    if repository is None:
        names = ", ".join(REPOSITORIES_BY_NAME)
        raise MergeSetError(
            f"`{name}` is not a repository a set spans; the repositories are {names}"
        )
    return repository


def repository_of_full_name(full_name: str) -> Repository | None:
    """The repository that `<organisation>/<name>` names, if a set spans it."""
    organisation, _, name = full_name.partition("/")
    if organisation.casefold() != resolve.ORGANISATION.casefold():
        return None
    return REPOSITORIES_BY_NAME.get(name)


def repository_list() -> str:
    """Every repository of a set and the repository of the Claude review,
    comma-separated: the `repositories` of the token of the sync, which
    reaches all of them."""
    return ",".join(
        [*(repository.name for repository in REPOSITORIES), REVIEW_REPOSITORY]
    )


# The Claude review, as the bot and the reviewer (claude_review.py) both name
# it.

# The private repository whose workflow `Claude review` reviews a set, the
# file of that workflow, and the branch that it runs from: that branch alone
# reaches the environment that holds the key of the reviewer App and the
# Claude login.
REVIEW_REPOSITORY = "peppy-superproject"
REVIEW_WORKFLOW = "claude-review.yml"
REVIEW_WORKFLOW_NAME = "Claude review"
REVIEW_WORKFLOW_REF = "main"
# The GitHub App that posts the findings and the approvals of the review.
REVIEWER_LOGIN = "peppy-claude-review[bot]"
# The team whose members get the offer of the review.
DEVELOPER_TEAM = "peppy-devs"
# The artifact of a review run that did not complete, which gives the cause,
# and the JSON file that it holds: {"cause": "<text>"}.
REVIEW_OUTCOME_ARTIFACT = "claude-review-outcome"
REVIEW_OUTCOME_FILE = "outcome.json"
# The name of each review run (the `run-name` of the workflow): the set, and
# the pull request whose dashboard asked for the review. The relay reads the
# set of the end of a run from it, and the bot finds the runs of a set by it.
REVIEW_RUN_NAME = re.compile(
    rf"\A{re.escape(REVIEW_WORKFLOW_NAME)} (?P<set>\S+) from "
    r"(?P<repository>[A-Za-z0-9._-]+)#(?P<number>[0-9]+)\Z"
)


def review_run_name(set_name: str, repository: Repository, number: int) -> str:
    return f"{REVIEW_WORKFLOW_NAME} {set_name} from {repository.name}#{number}"


@dataclass(frozen=True)
class ReviewRunName:
    """What the name of a review run says."""

    set_name: str
    # The repository and the number of the pull request whose dashboard
    # asked for the review.
    repository: Repository
    number: int


def parse_review_run_name(title: str) -> ReviewRunName | None:
    """The name of a review run; None for a name of another form, or one that
    names a repository that no set spans."""
    match = REVIEW_RUN_NAME.fullmatch(title)
    if match is None:
        return None
    repository = REPOSITORIES_BY_NAME.get(match["repository"])
    if repository is None:
        return None
    return ReviewRunName(match["set"], repository, int(match["number"]))


# The gate --------------------------------------------------------------------

# The commit status the rulesets require, which only the merge-set App reports.
STATUS_CONTEXT = "merge-set"
# The most characters GitHub keeps of a commit status description.
STATUS_DESCRIPTION_LIMIT = 140


@dataclass(frozen=True)
class Gate:
    """The `merge-set` status of a head commit."""

    state: str
    description: str
    target_url: str | None


def status_description(text: str) -> str:
    if len(text) <= STATUS_DESCRIPTION_LIMIT:
        return text
    return text[: STATUS_DESCRIPTION_LIMIT - 1] + "…"


def alone_gate(set_name: str) -> Gate:
    return Gate(
        "success",
        status_description(
            f"Alone: no other repository has a change on the branch {set_name}"
        ),
        None,
    )


def member_gate(set_name: str, dashboard_url: str) -> Gate:
    return Gate(
        "pending",
        status_description(
            f"Part of the set {set_name}: merge it with the others from the "
            "merge-set comment"
        ),
        dashboard_url,
    )


def merging_gate(set_name: str, requester: str, run_url: str) -> Gate:
    return Gate(
        "success",
        status_description(f"Merging with the set {set_name}, as @{requester} asked"),
        run_url,
    )


# The event -------------------------------------------------------------------


@dataclass(frozen=True)
class Event:
    """The pull request or the branch an event of a repository is about."""

    repository: Repository
    branch: str
    pull_request: int | None
    from_fork: bool

    @property
    def set_name(self) -> str | None:
        return set_name_of(self.branch, self.from_fork)


def set_name_of(branch: str, from_fork: bool) -> str | None:
    """The set of a branch: the branch, unless it is in a fork or is an
    integration branch, which no set is named after."""
    if from_fork or branch in resolve.BRANCHES_WITHOUT_SET_NAME:
        return None
    return branch


def parse_pull_request_number(text: str) -> int | None:
    if not text:
        return None
    if not text.isdigit():
        raise MergeSetError(f"a pull request number is digits, got `{text}`")
    return int(text)


def parse_event(
    repository: str, branch: str, head_repository: str, pull_request: str
) -> Event:
    """The event the relay names in the inputs of the Merge set workflow."""
    if not branch:
        raise MergeSetError("the event names no branch")
    event_repository = repository_named(repository)
    return Event(
        repository=event_repository,
        branch=branch,
        pull_request=parse_pull_request_number(pull_request),
        from_fork=head_repository.casefold() != event_repository.full_name.casefold(),
    )


@dataclass(frozen=True)
class RelaySubject:
    """What an event payload says about the pull request or branch it is
    about. A comment names its pull request alone: the relay reads the branch
    of that pull request."""

    pull_request: int | None
    branch: str | None
    head_repository: str | None
    # The repository of the set that the event is about, when it is not the
    # repository of the event: the end of a review run of REVIEW_REPOSITORY
    # is about the pull request whose dashboard asked for the review.
    repository: Repository | None = None


def relay_subject(
    event_name: str, payload: Mapping, bot_login: str
) -> RelaySubject | None:
    """The subject of a webhook event that can change a set; None for every
    other event. `bot_login` is the login of the merge-set App, which alone
    writes the dashboards."""
    try:
        match event_name:
            case "pull_request":
                return pull_request_subject(payload)
            case "pull_request_review":
                return review_subject(payload)
            case "issue_comment":
                return dashboard_edit_subject(payload, bot_login)
            case "workflow_run":
                return workflow_run_subject(payload)
            case "delete":
                return deleted_branch_subject(payload)
            case _:
                return None
    except (KeyError, TypeError, IndexError) as error:
        raise MergeSetError(f"the `{event_name}` event lacks {error}") from error


# The actions of a pull request that can change its set: it opens, gets a new
# head, closes, reopens, becomes a draft or ready for review, or moves onto
# another base branch (an edit whose `changes` name the base).
PULL_REQUEST_ACTIONS = frozenset(
    {
        "opened",
        "reopened",
        "synchronize",
        "closed",
        "ready_for_review",
        "converted_to_draft",
        "edited",
    }
)

# The actions of a review that change the approvals of its pull request.
REVIEW_ACTIONS = frozenset({"submitted", "dismissed"})

# GitHub sends `requested` when a CI run starts, and not when it is re-run: the
# start of a re-run comes as `in_progress`, of a run attempt after the first.
# The `in_progress` of a first attempt adds nothing to its `requested`, so the
# relay drops it.
CI_RUN_ACTIONS = frozenset({"requested", "in_progress", "completed"})


def is_into_default_branch(payload: Mapping) -> bool:
    """Whether the pull request of the event is into the default branch of its
    repository: the one branch that the rulesets require the `merge-set` check
    on."""
    return (
        payload["pull_request"]["base"]["ref"]
        == payload["repository"]["default_branch"]
    )


def pull_request_subject(payload: Mapping) -> RelaySubject | None:
    action = payload["action"]
    if action not in PULL_REQUEST_ACTIONS or not is_into_default_branch(payload):
        return None
    if action == "edited" and (payload.get("changes") or {}).get("base") is None:
        return None
    return subject_of_pull_request(payload["pull_request"])


def subject_of_pull_request(item: Mapping) -> RelaySubject:
    head = item["head"]
    return RelaySubject(
        pull_request=item["number"],
        branch=head["ref"],
        head_repository=(head.get("repo") or {}).get("full_name"),
    )


def review_subject(payload: Mapping) -> RelaySubject | None:
    if payload["action"] not in REVIEW_ACTIONS or not is_into_default_branch(payload):
        return None
    return subject_of_pull_request(payload["pull_request"])


def dashboard_edit_subject(payload: Mapping, bot_login: str) -> RelaySubject | None:
    """A user's edit of a dashboard, where the user ticks its boxes. The edits
    of the bot itself start nothing."""
    comment = payload["comment"]
    if (
        payload["action"] != "edited"
        or payload["issue"].get("pull_request") is None
        or comment["user"]["login"] != bot_login
        or payload["sender"]["type"] == "Bot"
        or not comment["body"].startswith(DASHBOARD_MARKER)
    ):
        return None
    return RelaySubject(payload["issue"]["number"], None, None)


def workflow_run_subject(payload: Mapping) -> RelaySubject | None:
    """The start or the end of a CI run, the end of a run of the PR title
    check, or the end of a review run."""
    workflow = payload["workflow"]["name"]
    if is_review_repository(payload["repository"]["full_name"]):
        if workflow != REVIEW_WORKFLOW_NAME:
            return None
        return review_run_subject(payload)
    if workflow == CI_WORKFLOW:
        return ci_run_subject(payload)
    if workflow == TITLE_CHECK_WORKFLOW:
        return title_check_run_subject(payload)
    return None


def ci_run_subject(payload: Mapping) -> RelaySubject | None:
    """The start or the end of a CI run of a pull request."""
    action = payload["action"]
    run = payload["workflow_run"]
    if (
        run["event"] != "pull_request"
        or action not in CI_RUN_ACTIONS
        or (action == "in_progress" and run["run_attempt"] == 1)
    ):
        return None
    return subject_of_run(run)


def title_check_run_subject(payload: Mapping) -> RelaySubject | None:
    """The end of a run of the PR title check of a pull request, which runs on
    `pull_request_target`. The relay hands on the end alone: the check ends
    within seconds of its start, and the sync reads its state when it runs."""
    run = payload["workflow_run"]
    if payload["action"] != "completed" or run["event"] != "pull_request_target":
        return None
    return subject_of_run(run)


def is_review_repository(full_name: str) -> bool:
    return (
        full_name.casefold() == f"{resolve.ORGANISATION}/{REVIEW_REPOSITORY}".casefold()
    )


def review_run_subject(payload: Mapping) -> RelaySubject | None:
    """The end of a review run: the set and the pull request whose dashboard
    asked for the review, as the name of the run gives them. The start of a
    review adds nothing: the sync that starts the run shows it in progress."""
    if payload["action"] != "completed":
        return None
    named = parse_review_run_name(payload["workflow_run"]["display_title"])
    if named is None:
        return None
    return RelaySubject(
        pull_request=named.number,
        branch=named.set_name,
        head_repository=named.repository.full_name,
        repository=named.repository,
    )


def subject_of_run(run: Mapping) -> RelaySubject:
    """The pull request of a workflow run. GitHub lists none for a run of a pull
    request from a fork, nor for a run on `pull_request_target`, so the subject
    of such a run is its head branch alone."""
    pull_requests = run.get("pull_requests") or []
    return RelaySubject(
        pull_request=pull_requests[0]["number"] if pull_requests else None,
        branch=run["head_branch"],
        head_repository=(run.get("head_repository") or {}).get("full_name"),
    )


def deleted_branch_subject(payload: Mapping) -> RelaySubject | None:
    if payload["ref_type"] != "branch":
        return None
    return RelaySubject(None, payload["ref"], payload["repository"]["full_name"])


# The pull requests and the set ------------------------------------------------


@dataclass(frozen=True)
class PullRequest:
    repository: Repository
    number: int
    url: str
    title: str
    # The login of the user who opened the pull request; None when GitHub
    # names no user.
    author: str | None
    is_open: bool
    created_at: datetime
    # None while it has not merged.
    merged_at: datetime | None
    draft: bool
    head_commit: str
    head_branch: str
    base_branch: str
    from_fork: bool

    @property
    def merged(self) -> bool:
        return self.merged_at is not None

    @property
    def label(self) -> str:
        return f"{self.repository.name}#{self.number}"

    @property
    def reference(self) -> str:
        """The pull request as GitHub links it from any repository."""
        return f"{self.repository.full_name}#{self.number}"

    @property
    def link(self) -> str:
        """The label of the pull request, linked to it."""
        return f"[{self.label}]({self.url})"


# The form of a time in the REST API, `2026-10-28T08:13:47Z`.
GITHUB_TIME_FORMAT = "%Y-%m-%dT%H:%M:%S%z"


def parse_github_time(text: str) -> datetime:
    return datetime.strptime(text, GITHUB_TIME_FORMAT)


def parse_pull_request(repository: Repository, item: Mapping) -> PullRequest:
    """A pull request as the REST API returns it, in a list or alone."""
    head_repository = (item["head"].get("repo") or {}).get("full_name")
    merged_at = item.get("merged_at")
    return PullRequest(
        repository=repository,
        number=item["number"],
        url=item["html_url"],
        title=item["title"],
        author=(item.get("user") or {}).get("login"),
        is_open=item["state"] == "open",
        created_at=parse_github_time(item["created_at"]),
        merged_at=None if merged_at is None else parse_github_time(merged_at),
        draft=bool(item.get("draft")),
        head_commit=item["head"]["sha"],
        head_branch=item["head"]["ref"],
        base_branch=item["base"]["ref"],
        from_fork=head_repository != item["base"]["repo"]["full_name"],
    )


@dataclass(frozen=True)
class Member:
    """A repository of the set that has a change on the branch of the set
    that did not merge yet."""

    repository: Repository
    # The head of the branch of the set in this repository.
    head: str
    # Its open pull requests from the branch. A member that can merge has one.
    open_pull_requests: tuple[PullRequest, ...]

    @property
    def pull_request(self) -> PullRequest | None:
        if len(self.open_pull_requests) == 1:
            return self.open_pull_requests[0]
        return None


@dataclass(frozen=True)
class MergedMember:
    """A repository of the set whose change merged before the rest of the
    set. GitHub deleted its branch of the set, or the branch is left at the
    head that merged."""

    repository: Repository
    # Its pull requests from the branch that merged into its integration
    # branch as part of the set, in the order they merged.
    pull_requests: tuple[PullRequest, ...]

    @property
    def head(self) -> str:
        """The head of the newest merge, which holds the older ones."""
        return self.pull_requests[-1].head_commit


@dataclass(frozen=True)
class SetState:
    name: str
    # Ordered as REPOSITORIES.
    members: tuple[Member, ...]
    # Ordered as REPOSITORIES. A repository is a member or merged, not both.
    merged: tuple[MergedMember, ...]
    # The repositories that still hold the branch of a pull request that
    # merged it before every open pull request of the set opened: a branch
    # left behind by an earlier set of the same name, not a change of this
    # set.
    left_behind: tuple[Repository, ...]

    @property
    def is_alone(self) -> bool:
        """Whether fewer than two repositories have a change of the set left
        to merge. The merged members do not count."""
        return len(self.members) < 2

    def open_pull_requests(self) -> list[PullRequest]:
        return [
            pull_request
            for member in self.members
            for pull_request in member.open_pull_requests
        ]

    def merged_pull_requests(self) -> list[PullRequest]:
        return [
            pull_request
            for merged in self.merged
            for pull_request in merged.pull_requests
        ]

    def member_of(self, repository: Repository) -> Member | None:
        return next(
            (member for member in self.members if member.repository is repository),
            None,
        )


def is_left_behind(head: str, pull_requests: Sequence[PullRequest]) -> bool:
    """Whether the branch at `head` is the branch of a merged pull request, as
    that pull request merged it. GitHub deletes the branch of a merged pull
    request in every repository of a set, so a branch still there is one the
    deletion missed, not a change that waits for its merge."""
    closed = [
        pull_request for pull_request in pull_requests if not pull_request.is_open
    ]
    if not closed:
        return False
    newest = max(closed, key=lambda pull_request: pull_request.number)
    return newest.merged and newest.head_commit == head


def merged_while_the_set_was_open(
    open_pull_requests: Sequence[PullRequest],
    merged_pull_requests: Sequence[PullRequest],
) -> list[PullRequest]:
    """The merged pull requests that are part of the set, in the order they
    merged: each one merged after an open pull request of the set opened, or
    after another merged pull request that is part of the set opened, so the
    two were open at the same time. A set with no open pull request has
    none."""
    if not open_pull_requests:
        return []
    set_opened = min(pull_request.created_at for pull_request in open_pull_requests)
    newest_first = sorted(
        merged_pull_requests,
        key=lambda pull_request: pull_request.merged_at,
        reverse=True,
    )
    in_the_set = []
    for pull_request in newest_first:
        if pull_request.merged_at < set_opened:
            break
        in_the_set.append(pull_request)
        set_opened = min(set_opened, pull_request.created_at)
    return in_the_set[::-1]


def choose_set(
    name: str,
    heads: Mapping[Repository, str | None],
    pull_requests: Mapping[Repository, Sequence[PullRequest]],
) -> SetState:
    """The set named `name`, from the head of its branch in each repository
    (None where there is no such branch) and the pull requests from that
    branch in each repository, open, closed and merged."""
    own = {
        repository: [
            pull_request
            for pull_request in pull_requests[repository]
            if not pull_request.from_fork and pull_request.head_branch == name
        ]
        for repository in REPOSITORIES
    }
    open_ones = {
        repository: tuple(
            pull_request for pull_request in own[repository] if pull_request.is_open
        )
        for repository in REPOSITORIES
    }
    merged_in_the_set = merged_while_the_set_was_open(
        [pull_request for found in open_ones.values() for pull_request in found],
        [
            pull_request
            for repository in REPOSITORIES
            for pull_request in own[repository]
            if pull_request.merged
            and pull_request.base_branch == repository.integration_branch
        ],
    )
    members, merged, left_behind = [], [], []
    for repository in REPOSITORIES:
        head = heads[repository]
        merged_here = tuple(
            pull_request
            for pull_request in merged_in_the_set
            if pull_request.repository is repository
        )
        if head is not None and (
            open_ones[repository] or not is_left_behind(head, own[repository])
        ):
            members.append(Member(repository, head, open_ones[repository]))
        elif merged_here:
            merged.append(MergedMember(repository, merged_here))
        elif head is not None:
            left_behind.append(repository)
    return SetState(name, tuple(members), tuple(merged), tuple(left_behind))


# The checks, the reviews and the rules --------------------------------------


@dataclass(frozen=True)
class RequiredCheck:
    context: str
    # The App that must report the check; None when any may.
    integration_id: int | None


@dataclass(frozen=True)
class BranchRules:
    """What the rulesets of an integration branch require of a pull request
    before it merges, the `merge-set` check aside."""

    required_checks: tuple[RequiredCheck, ...]
    # Whether the pull request must be up to date with its base.
    strict: bool
    # The rulesets whose pull request rule requires an approval, by id.
    approval_rulesets: tuple[int, ...]


def requires_approval(pull_request_rule: Mapping) -> bool:
    parameters = pull_request_rule["parameters"]
    return bool(
        parameters.get("required_approving_review_count")
        or parameters.get("require_code_owner_review")
        or parameters.get("required_reviewers")
    )


def parse_branch_rules(rules: Sequence[Mapping]) -> BranchRules:
    """The rules of a branch as `GET /repos/{repo}/rules/branches/{branch}`
    lists them, one entry per rule of each ruleset that applies."""
    checks: list[RequiredCheck] = []
    strict = False
    approval_rulesets: list[int] = []
    for rule in rules:
        if (
            rule["type"] == "pull_request"
            and requires_approval(rule)
            and rule["ruleset_id"] not in approval_rulesets
        ):
            approval_rulesets.append(rule["ruleset_id"])
        if rule["type"] != "required_status_checks":
            continue
        parameters = rule["parameters"]
        strict = strict or bool(parameters.get("strict_required_status_checks_policy"))
        for check in parameters["required_status_checks"]:
            required = RequiredCheck(check["context"], check.get("integration_id"))
            if required.context != STATUS_CONTEXT and required not in checks:
                checks.append(required)
    return BranchRules(tuple(checks), strict, tuple(approval_rulesets))


# The values of `current_user_can_bypass` that let the App merge a pull request
# the ruleset blocks.
BYPASSING_VALUES = frozenset({"always", "pull_requests_only", "exempt"})


@dataclass(frozen=True)
class RulesetBypass:
    """Whether the App bypasses a ruleset when it merges a pull request."""

    name: str
    bypassed: bool


def parse_ruleset_bypass(ruleset: Mapping) -> RulesetBypass:
    """A ruleset as `GET /repos/{repo}/rulesets/{id}` gives it to the App,
    whose bypass `current_user_can_bypass` names. A ruleset that does not name
    it is not bypassed."""
    return RulesetBypass(
        name=ruleset["name"],
        bypassed=ruleset.get("current_user_can_bypass") in BYPASSING_VALUES,
    )


class CheckState(Enum):
    PASSED = "passed"
    FAILED = "failed"
    RUNNING = "running"
    MISSING = "missing"


@dataclass(frozen=True)
class CheckRun:
    id: int
    name: str
    app_id: int | None
    completed: bool
    conclusion: str | None


@dataclass(frozen=True)
class CommitStatus:
    context: str
    state: str
    description: str
    target_url: str | None
    creator: str | None


# The check run conclusions GitHub accepts for a required check. A job that
# its `if` skips concludes `skipped`.
PASSING_CONCLUSIONS = frozenset({"success", "neutral", "skipped"})


def parse_check_runs(items: Sequence[Mapping]) -> list[CheckRun]:
    return [
        CheckRun(
            id=item["id"],
            name=item["name"],
            app_id=(item.get("app") or {}).get("id"),
            completed=item["status"] == "completed",
            conclusion=item.get("conclusion"),
        )
        for item in items
    ]


def parse_statuses(items: Sequence[Mapping]) -> list[CommitStatus]:
    """The statuses of a commit, newest first, as GitHub lists them."""
    return [
        CommitStatus(
            context=item["context"],
            state=item["state"],
            description=item.get("description") or "",
            target_url=item.get("target_url"),
            creator=(item.get("creator") or {}).get("login"),
        )
        for item in items
    ]


def run_state(completed: bool, conclusion: str | None) -> CheckState:
    """The state of a check run or of a workflow run."""
    if not completed:
        return CheckState.RUNNING
    if conclusion in PASSING_CONCLUSIONS:
        return CheckState.PASSED
    return CheckState.FAILED


def required_check_state(
    check: RequiredCheck, runs: Sequence[CheckRun], statuses: Sequence[CommitStatus]
) -> CheckState:
    """The state of a required check on a commit: its newest check run, else
    its newest commit status."""
    matching = [
        run
        for run in runs
        if run.name == check.context
        and (check.integration_id is None or run.app_id == check.integration_id)
    ]
    if matching:
        newest = max(matching, key=lambda run: run.id)
        return run_state(newest.completed, newest.conclusion)
    status = next(
        (status for status in statuses if status.context == check.context), None
    )
    if status is None:
        return CheckState.MISSING
    if status.state == "success":
        return CheckState.PASSED
    if status.state == "pending":
        return CheckState.RUNNING
    return CheckState.FAILED


def current_gate(statuses: Sequence[CommitStatus], bot_login: str) -> Gate | None:
    """The newest `merge-set` status the bot reported on a commit."""
    for status in statuses:
        if status.context == STATUS_CONTEXT and status.creator == bot_login:
            return Gate(status.state, status.description, status.target_url)
    return None


class ReviewDecision(Enum):
    APPROVED = "APPROVED"
    CHANGES_REQUESTED = "CHANGES_REQUESTED"
    REVIEW_REQUIRED = "REVIEW_REQUIRED"
    # No rule asks for a review.
    NOT_REQUIRED = "NOT_REQUIRED"


def parse_review_decision(value: str | None) -> ReviewDecision:
    """The `reviewDecision` of a pull request, which GraphQL gives as null
    when no rule asks for a review."""
    if value is None:
        return ReviewDecision.NOT_REQUIRED
    try:
        return ReviewDecision(value)
    except ValueError as error:
        raise MergeSetError(f"`{value}` is not a review decision") from error


# The CI runs -----------------------------------------------------------------

# The name of the one CI workflow of each repository of a set, whose runs the
# relay hands on.
CI_WORKFLOW = "Tests"
# The name of the workflow of the PR title check of each repository of a set
# (.github/actions/pr-title), whose ends the relay hands on: a change of a
# title can pass or fail that check.
TITLE_CHECK_WORKFLOW = "pr-title"


@dataclass(frozen=True)
class CiRun:
    """The newest run of the CI workflow of the head commit of a pull
    request."""

    url: str
    # Passed, failed or running.
    state: CheckState


def ci_workflow_ids(
    repository: Repository, workflows: Sequence[Mapping]
) -> frozenset[int]:
    """The ids of the workflows of the repository named as the CI workflow,
    as the workflow_run trigger finds them: by the name of the workflow. A run
    carries the `run-name` of its workflow as its own name, so the runs are
    matched by the id of their workflow."""
    ids = frozenset(
        workflow["id"] for workflow in workflows if workflow["name"] == CI_WORKFLOW
    )
    if not ids:
        raise MergeSetError(
            f"{repository.name} has no workflow named {CI_WORKFLOW}: the "
            "merge-set bot reads the CI of a pull request from it"
        )
    return ids


def latest_ci_run(
    runs: Sequence[Mapping], ci_workflows: frozenset[int]
) -> CiRun | None:
    """The newest run of the CI workflow among the runs of a head commit;
    None when the CI has not started on it. A re-run keeps its run id."""
    ci_runs = [run for run in runs if run["workflow_id"] in ci_workflows]
    if not ci_runs:
        return None
    newest = max(ci_runs, key=lambda run: run["id"])
    return CiRun(
        url=newest["html_url"],
        state=run_state(newest["status"] == "completed", newest.get("conclusion")),
    )


# The set records of the hub runs ---------------------------------------------


@dataclass(frozen=True)
class TestedRef:
    ref: str
    commit: str


@dataclass(frozen=True)
class TestedSet:
    """What one job of a hub run ran with, as its set record says."""

    hubs: Mapping[str, TestedRef]
    # The peppy branch and commit of the dev build it ran; None for any other
    # peppy, which no branch of a set gives.
    peppy: TestedRef | None


def parse_tested_ref(owner: str, entry: object) -> TestedRef:
    if (
        not isinstance(entry, Mapping)
        or not isinstance(entry.get("ref"), str)
        or not isinstance(entry.get("commit"), str)
        or not resolve.COMMIT_PATTERN.fullmatch(entry["commit"])
    ):
        raise MergeSetError(
            f"the set record names {owner} as {json.dumps(entry)}, not as "
            '{"ref": "<branch>", "commit": "<40 hex>"}'
        )
    return TestedRef(entry["ref"], entry["commit"])


def parse_tested_set(text: str) -> TestedSet:
    """A set record, the `set` output of the hub-ci-peppy action."""
    try:
        document = json.loads(text)
    except json.JSONDecodeError as error:
        raise MergeSetError(f"the set record is not JSON: {error}") from error
    if (
        not isinstance(document, Mapping)
        or not isinstance(document.get("hubs"), Mapping)
        or not isinstance(document.get("peppy"), Mapping)
    ):
        raise MergeSetError(f"the set record has no `hubs` and `peppy`: {text}")
    peppy = document["peppy"]
    return TestedSet(
        hubs={
            name: parse_tested_ref(name, entry)
            for name, entry in document["hubs"].items()
        },
        peppy=(
            parse_tested_ref("peppy", peppy)
            if peppy.get("build") == resolve.PeppyBuildKind.DEV_BUILD.value
            else None
        ),
    )


@dataclass(frozen=True)
class JobRecord:
    job: str
    # None when the record expired: nothing tells what the job ran with.
    tested: TestedSet | None


@dataclass(frozen=True)
class HubRun:
    """The latest run of one workflow of the head commit of a hub pull
    request, with the set record of each of its latest jobs that has one."""

    repository: Repository
    id: int
    name: str
    url: str
    completed: bool
    records: tuple[JobRecord, ...]


@dataclass(frozen=True)
class Artifact:
    name: str
    expired: bool
    download_url: str


def latest_run_of_each_workflow(runs: Sequence[Mapping]) -> list[Mapping]:
    """The newest run of each workflow, by workflow id. A re-run keeps its run
    id, and a new run of the same commit (a reopened pull request) gets a
    higher one."""
    newest: dict[int, Mapping] = {}
    for run in runs:
        known = newest.get(run["workflow_id"])
        if known is None or run["id"] > known["id"]:
            newest[run["workflow_id"]] = run
    return sorted(newest.values(), key=lambda run: run["id"])


def recorded_jobs(
    jobs: Sequence[Mapping], artifacts: Sequence[Artifact]
) -> list[tuple[str, Artifact]]:
    """Each latest job of a run that uploaded a set record, with its record.
    A job re-run in a later attempt gets a new id, so the id of each latest
    job names the record of the attempt whose result counts."""
    by_name = {artifact.name: artifact for artifact in artifacts}
    recorded = []
    for job in jobs:
        artifact = by_name.get(resolve.set_record_artifact_name(job["id"]))
        if artifact is not None:
            recorded.append((job["name"], artifact))
    return recorded


def parse_artifacts(items: Sequence[Mapping]) -> list[Artifact]:
    return [
        Artifact(
            name=item["name"],
            expired=item["expired"],
            download_url=item["archive_download_url"],
        )
        for item in items
    ]


def file_of_bundle(bundle: bytes, name: str, artifact: str) -> str:
    """The file `name` in the zip GitHub serves an artifact as. `artifact`
    names the artifact for the error."""
    with zipfile.ZipFile(io.BytesIO(bundle)) as archive:
        if name not in archive.namelist():
            raise MergeSetError(
                f"the {artifact} artifact holds {', '.join(archive.namelist())}, "
                f"not {name}"
            )
        return archive.read(name).decode()


def set_record_of_bundle(bundle: bytes) -> str:
    """The set record in the zip GitHub serves an artifact as."""
    return file_of_bundle(bundle, resolve.SET_RECORD_FILE, "set record")


@dataclass(frozen=True)
class Mismatch:
    """A member of the set a job ran at another commit than the head of its
    branch. A job that ran `main` at the commit a branch of the set still
    points at ran the same content, so it is no mismatch."""

    repository: Repository
    tested: TestedRef
    # The head of the branch, or the head that merged for a merged member.
    current: str
    # Whether the member merged.
    merged: bool


@dataclass(frozen=True)
class DepartedBranch:
    """A repository a job ran at its branch of the set, whose branch then left
    the set (see branch_left_the_set). The set merges with its integration
    branch, which the job did not run."""

    repository: Repository
    tested: TestedRef


@dataclass(frozen=True)
class StaleJob:
    run: HubRun
    job: str
    mismatches: tuple[Mismatch, ...]
    departed: tuple[DepartedBranch, ...]
    record_expired: bool


def tested_ref_of(tested: TestedSet, repository: Repository) -> TestedRef | None:
    """What a job ran of `repository`; None when it did not read it (a job
    without the deploy key of the private hub, or with no peppy dev build),
    and always None for a repository that is neither peppy nor a hub, whose
    branch of a set no job reads."""
    if repository.hub is not None:
        return tested.hubs.get(repository.name)
    if repository == PEPPY:
        return tested.peppy
    return None


def mismatch_of(
    tested: TestedSet, repository: Repository, head: str, merged: bool
) -> Mismatch | None:
    """What a job ran of a repository of the set, when it ran another commit
    than `head`."""
    recorded = tested_ref_of(tested, repository)
    if recorded is None or recorded.commit == head:
        return None
    return Mismatch(repository, recorded, head, merged)


# Whether a commit of a repository holds another one: (repository, commit,
# ancestor).
Holds = Callable[[Repository, str, str], bool]


def ran_the_base_after_the_merge(mismatch: Mismatch, holds: Holds) -> bool:
    """Whether a job ran the integration branch of a merged member at a
    commit that holds the head that merged: GitHub deleted the branch of the
    set, and the job read the integration branch in its place."""
    repository = mismatch.repository
    return (
        mismatch.merged
        and mismatch.tested.ref == repository.integration_branch
        and holds(repository, mismatch.tested.commit, mismatch.current)
    )


def mismatches_of(
    tested: TestedSet,
    state: SetState,
    under_test: Repository,
    is_clean_base_merge: Callable[[Mismatch], bool],
    holds: Holds,
) -> tuple[Mismatch, ...]:
    found = [
        *(
            mismatch_of(tested, member.repository, member.head, merged=False)
            for member in state.members
            if member.repository is not under_test
        ),
        *(
            mismatch_of(tested, merged.repository, merged.head, merged=True)
            for merged in state.merged
        ),
    ]
    return tuple(
        mismatch
        for mismatch in found
        if mismatch is not None
        and not ran_the_base_after_the_merge(mismatch, holds)
        and not is_clean_base_merge(mismatch)
    )


def branch_left_the_set(state: SetState, repository: Repository) -> bool:
    """Whether the repository has neither a branch of the set nor a change of
    the set that merged: its branch of the set was deleted, as when its pull
    request closed without a merge. A branch left behind by an earlier set did
    not leave the set: it merged before the set opened, and a re-run reads it
    all the same."""
    return (
        state.member_of(repository) is None
        and all(merged.repository is not repository for merged in state.merged)
        and repository not in state.left_behind
    )


def departed_branches_of(
    tested: TestedSet, state: SetState
) -> tuple[DepartedBranch, ...]:
    """What a job ran at the branch of the set of each repository whose
    branch then left the set, in the order of REPOSITORIES."""
    departed = []
    for repository in REPOSITORIES:
        recorded = tested_ref_of(tested, repository)
        if recorded is None or recorded.ref != state.name:
            continue
        if branch_left_the_set(state, repository):
            departed.append(DepartedBranch(repository, recorded))
    return tuple(departed)


def stale_jobs(
    state: SetState,
    runs: Sequence[HubRun],
    is_clean_base_merge: Callable[[Mismatch], bool],
    holds: Holds,
) -> list[StaleJob]:
    """The jobs of the runs of a hub pull request that did not run with the
    head of every branch of the set, or that ran a branch that then left the
    set. A job that ran a branch at an older commit is not stale when
    `is_clean_base_merge` tells that the head differs from that commit by a
    clean merge of its base alone, nor, for a merged member, when `holds`
    tells that the job ran its integration branch at a commit that holds the
    head that merged."""
    stale = []
    for run in runs:
        for record in run.records:
            if record.tested is None:
                stale.append(StaleJob(run, record.job, (), (), record_expired=True))
                continue
            mismatches = mismatches_of(
                record.tested, state, run.repository, is_clean_base_merge, holds
            )
            departed = departed_branches_of(record.tested, state)
            if mismatches or departed:
                stale.append(
                    StaleJob(
                        run, record.job, mismatches, departed, record_expired=False
                    )
                )
    return stale


@dataclass(frozen=True)
class Comparison:
    """Two commits as `GET /repos/{repo}/compare/{base}...{head}` compares
    them."""

    # The commits of base that head does not hold.
    behind_by: int
    # The newest commit that both hold.
    merge_base: str


def parse_comparison(item: Mapping) -> Comparison:
    return Comparison(item["behind_by"], item["merge_base_commit"]["sha"])


# The readiness of the set ---------------------------------------------------


@dataclass(frozen=True)
class PullRequestReport:
    """What decides whether a pull request of the set can merge, and the CI
    of its head commit, which the dashboard shows."""

    pull_request: PullRequest
    # Whether it merges into its base without conflicts; None while GitHub
    # has not computed it.
    mergeable: bool | None
    review: ReviewDecision
    checks: tuple[tuple[RequiredCheck, CheckState], ...]
    # None while the CI has not started on the head commit.
    ci: CiRun | None
    # Behind its base while its rules require it up to date.
    behind: bool
    stale: tuple[StaleJob, ...]
    # The rulesets that require the approval it lacks and that the App does
    # not bypass, by name. GitHub refuses the merge of the App until the pull
    # request gets that approval, so no admin merges it without one.
    unbypassed_approval_rulesets: tuple[str, ...]


class BlockerKind(Enum):
    # Blocks every user.
    BLOCKING = "blocking"
    # A missing approval that only rulesets the App bypasses require, so an
    # admin of every repository of the set merges without it.
    MISSING_APPROVAL = "missing approval"
    # A hub job that ran with another commit of a branch of the set, or with a
    # branch that left the set, or whose set record expired: a re-run of its
    # run clears it.
    OUT_OF_DATE_CI = "out-of-date CI"


@dataclass(frozen=True)
class Blocker:
    repository: Repository
    text: str
    kind: BlockerKind = BlockerKind.BLOCKING

    @property
    def waived_for_admins(self) -> bool:
        return self.kind is BlockerKind.MISSING_APPROVAL

    @property
    def out_of_date_ci(self) -> bool:
        return self.kind is BlockerKind.OUT_OF_DATE_CI


def short(commit: str) -> str:
    return commit[:7]


def run_link(run: HubRun) -> str:
    return f"[{run.name}]({run.url})"


def stale_job_text(label: str, stale: StaleJob) -> str:
    where = f"The job `{stale.job}` of {run_link(stale.run)} of {label}"
    if stale.record_expired:
        return (
            f"{where} has an expired set record, so nothing tells what it ran "
            "with: re-run it."
        )
    ran = "; ".join(
        [
            *(mismatch_text(mismatch) for mismatch in stale.mismatches),
            *(departed_branch_text(departed) for departed in stale.departed),
        ]
    )
    return f"{where} ran with {ran}: re-run it."


def tested_ref_text(repository: Repository, tested: TestedRef) -> str:
    return f"{repository.name} at `{tested.ref}` `{short(tested.commit)}`"


def mismatch_text(mismatch: Mismatch) -> str:
    ran = tested_ref_text(mismatch.repository, mismatch.tested)
    if mismatch.merged:
        return f"{ran}, and its branch of the set merged at `{short(mismatch.current)}`"
    return f"{ran}, and its branch of the set is now at `{short(mismatch.current)}`"


def departed_branch_text(departed: DepartedBranch) -> str:
    ran = tested_ref_text(departed.repository, departed.tested)
    return f"{ran}, whose branch is no longer in the set"


CHECK_STATE_TEXTS = {
    CheckState.FAILED: "failed",
    CheckState.RUNNING: "is running",
    CheckState.MISSING: "has not reported yet",
}


def missing_approval_text(label: str, unbypassed_rulesets: Sequence[str]) -> str:
    if not unbypassed_rulesets:
        return f"{label} needs an approving review."
    noun = "ruleset" if len(unbypassed_rulesets) == 1 else "rulesets"
    names = ", ".join(f"`{name}`" for name in unbypassed_rulesets)
    return (
        f"{label} needs an approving review. No admin merges it without one: "
        f"the merge-set App is not a bypass actor of the {noun} {names}."
    )


def missing_approval_kind(unbypassed_rulesets: Sequence[str]) -> BlockerKind:
    """GitHub refuses the merge of the App while a ruleset that it does not
    bypass requires the approval, so no admin merges without it."""
    if unbypassed_rulesets:
        return BlockerKind.BLOCKING
    return BlockerKind.MISSING_APPROVAL


def pull_request_blockers(report: PullRequestReport) -> list[Blocker]:
    pull_request = report.pull_request
    repository = pull_request.repository
    label = pull_request.label
    blockers = []

    def block(text: str, kind: BlockerKind = BlockerKind.BLOCKING) -> None:
        blockers.append(Blocker(repository, text, kind))

    if pull_request.base_branch != repository.integration_branch:
        block(
            f"{label} targets `{pull_request.base_branch}`: retarget it onto "
            f"`{repository.integration_branch}`."
        )
    if pull_request.draft:
        block(f"{label} is a draft: mark it ready for review.")
    merge_text = merge_state_text(pull_request, report.mergeable)
    if merge_text is not None:
        block(merge_text)
    if report.behind:
        block(
            f"{label} is behind `{pull_request.base_branch}`, and the ruleset of "
            "that branch requires it up to date: update the branch."
        )
    if report.review is ReviewDecision.CHANGES_REQUESTED:
        block(f"A reviewer requested changes on {label}.")
    if report.review is ReviewDecision.REVIEW_REQUIRED:
        block(
            missing_approval_text(label, report.unbypassed_approval_rulesets),
            missing_approval_kind(report.unbypassed_approval_rulesets),
        )
    for check, state in report.checks:
        if state is not CheckState.PASSED:
            block(
                f"The required check `{check.context}` of {label} {CHECK_STATE_TEXTS[state]}."
            )
    for stale in report.stale:
        block(stale_job_text(label, stale), BlockerKind.OUT_OF_DATE_CI)
    return blockers


def merge_state_text(pull_request: PullRequest, mergeable: bool | None) -> str | None:
    """What keeps a pull request from merging into its base, as GitHub
    computed it; None when it merges without conflicts."""
    if mergeable is False:
        return (
            f"{pull_request.label} has conflicts with `{pull_request.base_branch}`: "
            "resolve them."
        )
    if mergeable is None:
        return (
            f"GitHub has not yet computed whether {pull_request.label} merges "
            f"cleanly into `{pull_request.base_branch}`."
        )
    return None


def pull_request_count_text(set_name: str, member: Member) -> str | None:
    """What is wrong with the open pull requests of a member from the branch
    of the set; None when it has one."""
    repository = member.repository
    if not member.open_pull_requests:
        return (
            f"{repository.name} has a change on the branch `{set_name}` "
            f"(`{short(member.head)}`) but no open pull request from it: "
            "open one, or delete the branch."
        )
    if member.pull_request is None:
        labels = ", ".join(
            pull_request.label for pull_request in member.open_pull_requests
        )
        return (
            f"{repository.name} has more than one open pull request from "
            f"`{set_name}` ({labels}): close all of them but one."
        )
    return None


def member_blockers(
    set_name: str, member: Member, reports: Mapping[str, PullRequestReport]
) -> list[Blocker]:
    count_text = pull_request_count_text(set_name, member)
    if count_text is not None:
        return [Blocker(member.repository, count_text)]
    return pull_request_blockers(reports[member.pull_request.label])


def set_blockers(
    state: SetState, reports: Mapping[str, PullRequestReport]
) -> list[Blocker]:
    """What keeps the set from merging, by member in the order of
    REPOSITORIES. Reports are keyed by pull request label."""
    return [
        blocker
        for member in state.members
        for blocker in member_blockers(state.name, member, reports)
    ]


def blockers_for(blockers: Sequence[Blocker], admin: bool) -> list[Blocker]:
    """What keeps the set from merging for a user, an admin of every
    repository of the set or not."""
    return [
        blocker for blocker in blockers if not (admin and blocker.waived_for_admins)
    ]


def stale_runs(reports: Mapping[str, PullRequestReport]) -> list[HubRun]:
    """Every run with an out-of-date job, each once, in the order of the
    reports."""
    runs: dict[tuple[str, int], HubRun] = {}
    for report in reports.values():
        for stale in report.stale:
            runs.setdefault((stale.run.repository.name, stale.run.id), stale.run)
    return list(runs.values())


# The dashboard ---------------------------------------------------------------

DASHBOARD_MARKER = "<!-- peppy-merge-set -->"


class Action(Enum):
    MERGE = "merge"
    RERUN = "rerun"
    REVIEW = "review"


def box_line(action: Action, text: str) -> str:
    """An unticked box. The marker, not the text, names what the box asks."""
    return f"- [ ] <!-- merge-set:{action.value} --> {text}"


TICKED_BOX = re.compile(
    r"^[ \t]*[-*] \[[xX]\] <!-- merge-set:(?P<action>[a-z]+) -->", re.MULTILINE
)


def ticked_actions(body: str) -> list[Action]:
    """The actions whose box is ticked in a dashboard, in the order of the
    boxes, each once."""
    actions = []
    for match in TICKED_BOX.finditer(body):
        try:
            action = Action(match["action"])
        except ValueError:
            continue
        if action not in actions:
            actions.append(action)
    return actions


def number_link(pull_request: PullRequest) -> str:
    return f"[#{pull_request.number}]({pull_request.url})"


def pull_request_cell(member: Member) -> str:
    if member.pull_request is not None:
        return number_link(member.pull_request)
    if not member.open_pull_requests:
        return "no open pull request"
    return f"{len(member.open_pull_requests)} open pull requests"


def ci_cell(member: Member, reports: Mapping[str, PullRequestReport]) -> str:
    """The CI of the head of a member, linked to its run. A member without
    one open pull request has no report, and the Pull request cell says why."""
    if member.pull_request is None:
        return ""
    ci = reports[member.pull_request.label].ci
    if ci is None:
        return "not started"
    return f"[{ci.state.value}]({ci.url})"


def render_set_dashboard(
    state: SetState,
    reports: Mapping[str, PullRequestReport],
    set_review: SetReview | None,
) -> str:
    """The dashboard of a set. Reports are keyed by pull request label. It
    offers the Claude review when `set_review` is not None."""
    blockers = set_blockers(state, reports)
    stale_run_count = len(stale_runs(reports))
    lines = [
        DASHBOARD_MARKER,
        f"### Set `{state.name}`",
        "",
        (
            f"These pull requests share the branch name `{state.name}`, so they "
            "merge together: the CI of each hub pull request tests it with the "
            "branches of the set in peppy and in the other hubs, and the "
            "`merge-set` check keeps the merge button of each one blocked. Tick "
            "the first box below to merge all of them. The bot acts on a ticked "
            f"box only for {WHO_MAY_ASK}."
        ),
        "",
        "| Repository | Pull request | Head | CI | State |",
        "| --- | --- | --- | --- | --- |",
        *dashboard_rows(state, reports, blockers),
    ]
    if state.left_behind:
        lines.append("")
        lines.extend(
            f"{repository.name} still has the branch `{state.name}` of a pull "
            "request that merged it before the pull requests of this set "
            "opened, so it is not part of the set."
            for repository in state.left_behind
        )
    lines.append("")
    lines.extend(readiness_lines(blockers))
    lines.extend(
        [
            "",
            box_line(
                Action.MERGE,
                f"Merge the {len(state.members)} pull requests of this set together",
            ),
        ]
    )
    if stale_run_count:
        runs = "1 run" if stale_run_count == 1 else f"{stale_run_count} runs"
        lines.append(
            box_line(Action.RERUN, f"Re-run the out-of-date CI of this set ({runs})")
        )
    if set_review is not None:
        lines.extend(["", *review_section(set_review, alone=False)])
    return "\n".join(lines) + "\n"


def merged_pull_requests_cell(merged: MergedMember) -> str:
    return ", ".join(number_link(pull_request) for pull_request in merged.pull_requests)


def dashboard_rows(
    state: SetState,
    reports: Mapping[str, PullRequestReport],
    blockers: Sequence[Blocker],
) -> list[str]:
    """A row of the table of the dashboard for each repository of the set,
    merged or not, in the order of REPOSITORIES."""
    rows = {
        member.repository: (
            f"| {member.repository.name} | {pull_request_cell(member)} "
            f"| `{short(member.head)}` | {ci_cell(member, reports)} "
            f"| {member_state(member.repository, blockers)} |"
        )
        for member in state.members
    }
    rows.update(
        {
            merged.repository: (
                f"| {merged.repository.name} | {merged_pull_requests_cell(merged)} "
                f"| `{short(merged.head)}` |  | merged |"
            )
            for merged in state.merged
        }
    )
    return [rows[repository] for repository in REPOSITORIES if repository in rows]


def member_state(repository: Repository, blockers: Sequence[Blocker]) -> str:
    """The State cell of a member: what its own blockers leave it, with the
    cause when a re-run alone clears them."""
    own = [blocker for blocker in blockers if blocker.repository is repository]
    if not own:
        return "ready"
    hard = blockers_for(own, admin=True)
    if not hard:
        return "not approved"
    if all(blocker.out_of_date_ci for blocker in hard):
        return "CI out of date"
    return "blocked"


ADMIN_WAIVER = (
    "An admin of every repository of the set merges it without the approvals."
)
# What makes the hub runs of a set current, as the dashboard and the reports
# word it.
CURRENT_HUB_RUNS = (
    "each hub run ran with the head of every branch of the set that it reads, "
    "or with a commit that differs from that head by a clean merge of its base "
    "alone, or, for a branch that merged, with a commit of its base that holds "
    "that head, and with no branch that is no longer in the set"
)


def readiness_lines(blockers: Sequence[Blocker]) -> list[str]:
    """What the dashboard says of the merge: ready, ready for an admin alone,
    or what blocks it."""
    if not blockers:
        return [
            (
                "**Ready to merge.** Each pull request is approved, its required "
                f"checks passed, and {CURRENT_HUB_RUNS}."
            )
        ]
    hard = blockers_for(blockers, admin=True)
    if not hard:
        return [
            (
                "**Ready to merge for an admin.** Each required check passed and "
                f"{CURRENT_HUB_RUNS}, but approvals are missing. {ADMIN_WAIVER}"
            ),
            "",
            *(f"- {blocker.text}" for blocker in blockers),
        ]
    unapproved = [blocker for blocker in blockers if blocker.waived_for_admins]
    lines = [
        "**What blocks the merge**",
        "",
        *(f"- {blocker.text}" for blocker in hard),
    ]
    if unapproved:
        lines.extend(["", f"Approvals are missing too. {ADMIN_WAIVER}", ""])
        lines.extend(f"- {blocker.text}" for blocker in unapproved)
    return lines


def merged_links(state: SetState) -> str:
    return ", ".join(pull_request.link for pull_request in state.merged_pull_requests())


def render_alone_dashboard(state: SetState, set_review: SetReview | None) -> str:
    """The dashboard of a set that is alone. The bot writes it on the merged
    pull requests of the set too, so with merged members it names the pull
    request that is alone and the ones that merged. It offers the Claude
    review when `set_review` is not None."""
    lines = [DASHBOARD_MARKER, f"### Set `{state.name}`", ""]
    if not state.merged:
        lines.append(
            f"No other repository has a change on the branch `{state.name}`, so "
            "this pull request is alone: its own merge button merges it."
        )
    else:
        alone = ", ".join(
            pull_request.link for pull_request in state.open_pull_requests()
        )
        lines.extend(
            [
                f"No other repository has a change on the branch `{state.name}` "
                f"left to merge, so {alone} is alone: its own merge button "
                "merges it.",
                "",
                f"Merged from the set: {merged_links(state)}.",
            ]
        )
    if set_review is not None:
        lines.extend(["", *review_section(set_review, alone=True)])
    return "\n".join(lines) + "\n"


# The Claude review -----------------------------------------------------------


@dataclass(frozen=True)
class Review:
    """A review of a pull request, as `GET /repos/{repo}/pulls/{n}/reviews`
    lists it, oldest first."""

    id: int
    # None when GitHub names no user.
    author: str | None
    # Whether a bot wrote it, such as a GitHub App.
    by_bot: bool
    # APPROVED, CHANGES_REQUESTED, COMMENTED, DISMISSED or PENDING.
    state: str
    # The head of the pull request that the review is of.
    commit: str | None
    # None while its author did not submit it (PENDING).
    submitted_at: datetime | None


def parse_reviews(items: Sequence[Mapping]) -> list[Review]:
    return [parse_review(item) for item in items]


def parse_review(item: Mapping) -> Review:
    user = item.get("user") or {}
    submitted_at = item.get("submitted_at")
    return Review(
        id=item["id"],
        author=user.get("login"),
        by_bot=user.get("type") == "Bot",
        state=item["state"],
        commit=item.get("commit_id"),
        submitted_at=None if submitted_at is None else parse_github_time(submitted_at),
    )


def reviewer_approvals(reviews: Sequence[Review]) -> list[Review]:
    """The approvals of the reviewer that stand, oldest first: GitHub counts
    each one until it is dismissed."""
    return [
        review
        for review in reviews
        if review.author == REVIEWER_LOGIN and review.state == "APPROVED"
    ]


# Whether the head of a repository differs from a commit by a clean merge of
# its base alone: (repository, commit, head).
CleanBaseMerge = Callable[[Repository, str, str], bool]


def stale_reviewer_approvals(
    pull_request: PullRequest,
    reviews: Sequence[Review],
    differs_by_clean_base_merge: CleanBaseMerge,
) -> list[Review]:
    """The approvals of the reviewer at a commit that is not the head of the
    pull request, and from which the head differs by more than a clean merge
    of its base: the reviewer did not read the change."""
    head = pull_request.head_commit
    return [
        approval
        for approval in reviewer_approvals(reviews)
        if approval.commit != head
        and (
            approval.commit is None
            or not differs_by_clean_base_merge(
                pull_request.repository, approval.commit, head
            )
        )
    ]


def dismissal_message(pull_request: PullRequest) -> str:
    return (
        f"The head of {pull_request.label} moved to "
        f"`{short(pull_request.head_commit)}`, which Claude did not read. Tick "
        "the review box of the merge-set comment to have Claude review the new "
        "commits."
    )


@dataclass(frozen=True)
class ReviewComment:
    """A comment of a review thread, as `GET /repos/{repo}/pulls/{n}/comments`
    lists it, oldest first."""

    id: int
    # None when GitHub names no user.
    author: str | None
    body: str
    # The first comment of the thread; None for the first comment itself.
    in_reply_to: int | None
    path: str
    # The line of the comment in the file at the head; None for a comment on
    # a line that the head no longer has.
    line: int | None
    url: str


def parse_review_comments(items: Sequence[Mapping]) -> list[ReviewComment]:
    return [
        ReviewComment(
            id=item["id"],
            author=(item.get("user") or {}).get("login"),
            body=item.get("body") or "",
            in_reply_to=item.get("in_reply_to_id"),
            path=item["path"],
            line=item.get("line"),
            url=item["html_url"],
        )
        for item in items
    ]


class FindingLevel(Enum):
    # Holds the approval of the set while it is open.
    BLOCKING = "blocking"
    # Holds nothing.
    NOTE = "note"


@dataclass(frozen=True)
class FindingRecord:
    """The record that the reviewer puts at the start of the first comment of
    the thread of each of its findings, and edits when a later round closes
    the finding."""

    id: str
    level: FindingLevel
    open: bool


FINDING_MARKER = re.compile(r"\A<!-- claude-review:finding (?P<record>\{[^\n]*?\}) -->")
FINDING_ID = re.compile(r"[0-9a-f]{12}")


def finding_marker(record: FindingRecord) -> str:
    document = {"id": record.id, "level": record.level.value, "open": record.open}
    return f"<!-- claude-review:finding {json.dumps(document, sort_keys=True)} -->"


def parse_finding_record(body: str) -> FindingRecord | None:
    """The record at the start of a comment; None for a comment without one,
    or with one of another form."""
    match = FINDING_MARKER.match(body)
    if match is None:
        return None
    try:
        document = json.loads(match["record"])
    except json.JSONDecodeError:
        return None
    if not isinstance(document, Mapping):
        return None
    finding_id, level, is_open = (
        document.get("id"),
        document.get("level"),
        document.get("open"),
    )
    if (
        not isinstance(finding_id, str)
        or not FINDING_ID.fullmatch(finding_id)
        or not isinstance(is_open, bool)
        or level not in {member.value for member in FindingLevel}
    ):
        return None
    return FindingRecord(finding_id, FindingLevel(level), is_open)


def findings_of(
    comments: Sequence[ReviewComment],
) -> list[tuple[ReviewComment, FindingRecord]]:
    """The first comment of the thread of each finding of the reviewer, with
    its record, oldest first. Anyone can write a record; only the reviewer
    writes as the reviewer."""
    found = []
    for comment in comments:
        if comment.author != REVIEWER_LOGIN or comment.in_reply_to is not None:
            continue
        record = parse_finding_record(comment.body)
        if record is not None:
            found.append((comment, record))
    return found


def open_blocking_count(comments: Sequence[ReviewComment]) -> int:
    return sum(
        1
        for _, record in findings_of(comments)
        if record.open and record.level is FindingLevel.BLOCKING
    )


@dataclass(frozen=True)
class ReviewRun:
    """A run of the workflow of the Claude review."""

    id: int
    url: str
    completed: bool
    # None while it runs.
    conclusion: str | None

    @property
    def succeeded(self) -> bool:
        return self.completed and self.conclusion == "success"


def parse_review_run(run: Mapping) -> ReviewRun:
    return ReviewRun(
        id=run["id"],
        url=run["html_url"],
        completed=run["status"] == "completed",
        conclusion=run.get("conclusion"),
    )


def latest_review_run(runs: Sequence[Mapping], set_name: str) -> ReviewRun | None:
    """The newest run of the review workflow for the set, among the runs as
    `GET /repos/{repo}/actions/workflows/{file}/runs` lists them; None when
    there is none. A re-run keeps its run id."""
    of_the_set = [
        run
        for run in runs
        if (named := parse_review_run_name(run.get("display_title") or "")) is not None
        and named.set_name == set_name
    ]
    if not of_the_set:
        return None
    return parse_review_run(max(of_the_set, key=lambda run: run["id"]))


def parse_review_outcome(text: str) -> str:
    """The cause in the outcome file of a review run that did not complete."""
    try:
        document = json.loads(text)
    except json.JSONDecodeError as error:
        raise MergeSetError(f"the review outcome is not JSON: {error}") from error
    if not isinstance(document, Mapping) or not isinstance(document.get("cause"), str):
        raise MergeSetError(f'the review outcome is not {{"cause": "<text>"}}: {text}')
    return document["cause"]


@dataclass(frozen=True)
class PullRequestReview:
    """The Claude review of one open pull request of the set."""

    pull_request: PullRequest
    # The commit of the approval of the reviewer that stands; None without
    # one.
    approved_at: str | None
    open_blocking: int
    # Whether the reviewer posted a review on it, at any commit.
    reviewed: bool
    # The review decision of GitHub, read when the reviewer approved it: a
    # pull request that changes a file with a code owner still needs the
    # approval of that owner. None without an approval of the reviewer.
    decision: ReviewDecision | None


# The wait for a person -------------------------------------------------------

# A person has this long to review a set of developers, from the time each of
# its open pull requests is ready for review, before Claude can review it.
PERSON_REVIEW_WAIT = timedelta(hours=12)

# The workflow of peppy that starts the sync of each set that waits for a
# person, at this interval, so that the dashboard shows the time left and
# shows the box of the review when the wait ends: no event of GitHub comes
# when the time passes. It goes on for WAIT_SYNC_MARGIN after the end of the
# wait, so that a scheduled run that GitHub delays or drops does not keep the
# box hidden.
WAIT_SYNC_WORKFLOW = "merge-set-wait.yml"
WAIT_SYNC_INTERVAL = timedelta(minutes=30)
WAIT_SYNC_MARGIN = 2 * WAIT_SYNC_INTERVAL

# The mark of the dashboard that counts down the wait: the sync that finds
# it and offers the review tells the authors of the set.
WAIT_MARKER = "<!-- claude-review:wait -->"


@dataclass(frozen=True)
class PersonReview:
    """A review that a person submitted on an open pull request of the set:
    a user who is no bot, did not open a pull request of the set, and has
    write access to the repository of the pull request."""

    pull_request: PullRequest
    author: str
    submitted_at: datetime


@dataclass(frozen=True)
class DraftWait:
    """A pull request of the set is a draft, so the wait has not started."""

    drafts: tuple[PullRequest, ...]


@dataclass(frozen=True)
class PersonWait:
    """A person can review the set until `ends_at`; Claude cannot yet."""

    ends_at: datetime
    left: timedelta


@dataclass(frozen=True)
class ReviewedByPerson:
    """A person reviewed the set before the end of the wait, so a person
    reviews it and Claude does not. `review` is the first such review."""

    review: PersonReview


@dataclass(frozen=True)
class ClaudeOffered:
    """No person reviewed the set before the end of the wait: Claude can
    review it."""


ReviewOffer = DraftWait | PersonWait | ReviewedByPerson | ClaudeOffered


def opened_by_developers(
    pull_requests: Sequence[PullRequest], is_developer: Callable[[str], bool]
) -> bool:
    """Whether a member of DEVELOPER_TEAM opened each of the pull requests,
    so that their dashboard has the section of the Claude review. A pull
    request from a fork is in no set."""
    return bool(pull_requests) and all(
        pull_request.author is not None and is_developer(pull_request.author)
        for pull_request in pull_requests
    )


def wait_end(
    pull_requests: Sequence[PullRequest],
    ready_at: Callable[[PullRequest], datetime],
) -> datetime | None:
    """When the wait for a person ends: PERSON_REVIEW_WAIT after the last of
    the pull requests became ready for review, so that a person has that time
    for each one. None while one of them is a draft. `ready_at` gives when a
    pull request that is not a draft became ready for review."""
    if any(pull_request.draft for pull_request in pull_requests):
        return None
    return (
        max(ready_at(pull_request) for pull_request in pull_requests)
        + PERSON_REVIEW_WAIT
    )


def is_by_a_person(review: Review, authors: Collection[str | None]) -> bool:
    """Whether a user who is no bot and is not one of `authors` submitted
    the review."""
    return (
        review.author is not None
        and not review.by_bot
        and review.submitted_at is not None
        and review.author not in authors
    )


def person_reviews(
    pull_requests: Sequence[PullRequest],
    reviews_of: Mapping[str, Sequence[Review]],
    permission: Callable[[Repository, str], str],
) -> list[PersonReview]:
    """The reviews of persons on the pull requests, whose reviews are in
    `reviews_of` by label. `permission` gives the permission of a user on a
    repository."""
    authors = {pull_request.author for pull_request in pull_requests}
    return [
        PersonReview(pull_request, review.author, review.submitted_at)
        for pull_request in pull_requests
        for review in reviews_of[pull_request.label]
        if is_by_a_person(review, authors)
        and permission(pull_request.repository, review.author) in WRITE_PERMISSIONS
    ]


def review_offer(
    pull_requests: Sequence[PullRequest],
    ends_at: datetime | None,
    reviews: Sequence[PersonReview],
    now: datetime,
) -> ReviewOffer:
    """Whether Claude can review the open pull requests of a set at `now`,
    from the end of the wait for a person (None while one of them is a
    draft) and the reviews of persons on them. A review after the end of the
    wait does not take the offer back."""
    in_time = [
        review for review in reviews if ends_at is None or review.submitted_at < ends_at
    ]
    if in_time:
        return ReviewedByPerson(min(in_time, key=lambda review: review.submitted_at))
    if ends_at is None:
        return DraftWait(
            tuple(pull_request for pull_request in pull_requests if pull_request.draft)
        )
    if now < ends_at:
        return PersonWait(ends_at, ends_at - now)
    return ClaudeOffered()


def hours_text(duration: timedelta) -> str:
    return f"{int(duration.total_seconds() // 3600)} hours"


def utc_text(time: datetime) -> str:
    return time.astimezone(UTC).strftime("%Y-%m-%d %H:%M UTC")


def duration_text(duration: timedelta) -> str:
    """A duration in hours and whole minutes, as `11 h 5 min`."""
    hours, minutes = divmod(int(duration.total_seconds() // 60), 60)
    parts = [f"{hours} h"] if hours else []
    if minutes:
        parts.append(f"{minutes} min")
    return " ".join(parts) or "less than 1 min"


def section_subject(alone: bool) -> str:
    """What the section of the Claude review calls the set."""
    return "this pull request" if alone else "this set"


def wait_rule_text(alone: bool) -> str:
    subject = section_subject(alone)
    ready = "it is" if alone else "each of its pull requests is"
    return (
        f"A person has {hours_text(PERSON_REVIEW_WAIT)} to review {subject} from "
        f"the time {ready} ready for review. Claude can review {subject} only if "
        "no person reviewed it in that time."
    )


def wait_state_text(
    offer: DraftWait | PersonWait | ReviewedByPerson, alone: bool
) -> str:
    """Where the wait for a person is, for each offer but ClaudeOffered."""
    match offer:
        case DraftWait(drafts):
            links = ", ".join(pull_request.link for pull_request in drafts)
            verb = "is a draft" if len(drafts) == 1 else "are drafts"
            return f"{links} {verb}, so that time has not started."
        case PersonWait(ends_at, left):
            return f"**{duration_text(left)} left**, until {utc_text(ends_at)}."
        case ReviewedByPerson(review):
            return (
                f"`{review.author}` reviewed {review.pull_request.link}, so a "
                f"person reviews {section_subject(alone)}, not Claude."
            )


@dataclass(frozen=True)
class SetReview:
    """The Claude review of a set whose dashboard has its section."""

    # Whether Claude can review the set.
    offer: ReviewOffer
    # Ordered as the open pull requests of the set.
    pull_requests: tuple[PullRequestReview, ...]
    # The newest review run of the set; None when there is none.
    run: ReviewRun | None
    # What a run that did not complete gives as its cause; None when it gave
    # none, or when the run completed.
    cause: str | None

    @property
    def approved(self) -> bool:
        return bool(self.pull_requests) and all(
            review.approved_at is not None for review in self.pull_requests
        )

    @property
    def in_progress(self) -> bool:
        return self.run is not None and not self.run.completed


def counted(count: int, noun: str) -> str:
    return f"{count} {noun}" if count == 1 else f"{count} {noun}s"


def review_section(set_review: SetReview, alone: bool) -> list[str]:
    """The section "Review by Claude" of a dashboard: the wait for a person,
    then the offer of Claude, its box, and the state of the review."""
    lines = ["### Review by Claude", "", wait_rule_text(alone), ""]
    offer = set_review.offer
    if isinstance(offer, ClaudeOffered):
        return [*lines, *offered_review_lines(set_review, alone)]
    lines.append(wait_line(offer, alone))
    if set_review.in_progress:
        lines.extend(["", *review_state_lines(set_review)])
    return lines


def wait_line(offer: DraftWait | PersonWait | ReviewedByPerson, alone: bool) -> str:
    """The state of the wait for a person, as the dashboard shows it. The
    line that counts down the wait has WAIT_MARKER."""
    text = wait_state_text(offer, alone)
    if not isinstance(offer, PersonWait):
        return text
    minutes = int(WAIT_SYNC_INTERVAL.total_seconds() // 60)
    return f"{WAIT_MARKER}{text} The bot updates this time every {minutes} minutes."


def offered_review_lines(set_review: SetReview, alone: bool) -> list[str]:
    """The offer of the review of Claude, its box, and the state of the
    review."""
    pull_requests = counted(len(set_review.pull_requests), "pull request")
    subject = section_subject(alone)
    posts_on = "this pull request" if alone else "each pull request of the set"
    approves = "this pull request" if alone else f"the {pull_requests}"
    box = (
        "Ask Claude to review this pull request"
        if alone
        else f"Ask Claude to review the {pull_requests} of this set"
    )
    return [
        (
            f"No person reviewed {subject} in that time, so Claude can review it "
            "and approve it. This is your choice: a person can also review and "
            f"approve {subject} as usual."
        ),
        "",
        (
            f"To choose Claude, tick the box below. Claude posts its findings on "
            f"{posts_on}. Correct each finding or answer in its thread, then "
            "tick the box again. When no blocking finding is open, Claude "
            f"approves {approves}."
        ),
        "",
        box_line(Action.REVIEW, box),
        "",
        *review_state_lines(set_review),
    ]


def review_state_lines(set_review: SetReview) -> list[str]:
    run = set_review.run
    if set_review.in_progress:
        return [f"**Review in progress** in [this run]({run.url})."]
    if run is not None and not run.succeeded:
        cause = set_review.cause or (
            f"The run ended as `{run.conclusion}` before it gave a cause."
        )
        return [
            f"**Review not completed.** {cause} See [the run]({run.url}), and "
            "tick the box to ask again."
        ]
    reviews = set_review.pull_requests
    if set_review.approved:
        approvals = ", ".join(
            f"{review.pull_request.link} at `{short(review.approved_at)}`"
            for review in reviews
        )
        lines = [f"**Approved by Claude**, at the commits that it read: {approvals}."]
        lines.extend(
            f"- {review.pull_request.label} changes a file with a code owner, so "
            "it also needs the approval of that owner."
            for review in reviews
            if review.decision is ReviewDecision.REVIEW_REQUIRED
        )
        return lines
    with_findings = [review for review in reviews if review.open_blocking]
    if with_findings:
        total = sum(review.open_blocking for review in with_findings)
        where = ", ".join(
            f"{review.pull_request.link}: {review.open_blocking}"
            for review in with_findings
        )
        return [
            f"**{counted(total, 'open blocking finding')}** ({where}). Correct "
            "each finding or answer in its thread, then tick the box again."
        ]
    if any(review.reviewed for review in reviews):
        unapproved = ", ".join(
            review.pull_request.link for review in reviews if review.approved_at is None
        )
        return [
            "**Not approved.** No blocking finding is open, but Claude did not "
            f"read the head of {unapproved}: tick the box to ask for a new review."
        ]
    return ["**No review asked.**"]


def review_refusal_reasons(
    state: SetState,
    set_review: SetReview | None,
    mergeable: Mapping[str, bool | None],
) -> list[str]:
    """Why the bot does not start a review of the set; none when it starts
    one. `mergeable` tells, for each open pull request by label, whether it
    merges into its base without conflicts (None while GitHub computes it)."""
    if set_review is None:
        return [
            "Claude reviews only a set whose every open pull request a member of "
            f"`{DEVELOPER_TEAM}` opened."
        ]
    if not isinstance(set_review.offer, ClaudeOffered):
        return [
            f"{wait_rule_text(state.is_alone)} "
            f"{wait_state_text(set_review.offer, state.is_alone)}"
        ]
    if set_review.in_progress:
        return [
            f"A review of this set is in progress in [this run]({set_review.run.url})."
        ]
    reasons = [
        text
        for member in state.members
        if (text := pull_request_count_text(state.name, member)) is not None
    ]
    reasons.extend(
        text
        for pull_request in state.open_pull_requests()
        if (text := merge_state_text(pull_request, mergeable[pull_request.label]))
        is not None
    )
    if not reasons and set_review.approved:
        reasons.append(
            "Claude already approved each pull request of this set at its head."
        )
    return reasons


def review_inputs(state: SetState, request: Request) -> dict[str, str]:
    """The inputs of the review workflow: the set, the dashboard that asked
    for the review and its user, and the head of each open pull request that
    the review is of, which the reviewer checks before it reads them and
    again before it posts."""
    asked_on = request.dashboard.pull_request
    heads = {
        pull_request.repository.name: pull_request.head_commit
        for pull_request in state.open_pull_requests()
    }
    return {
        "set": state.name,
        "repository": asked_on.repository.name,
        "pull-request": str(asked_on.number),
        "requester": request.requester,
        "heads": json.dumps(heads, sort_keys=True),
    }


# The answer of the bot when the review workflow is not on its branch.
REVIEW_WORKFLOW_MISSING = (
    f"The workflow `{REVIEW_WORKFLOW_NAME}` is not on `{REVIEW_WORKFLOW_REF}` of "
    f"{REVIEW_REPOSITORY} yet, so no review can start."
)


def render_review_refusal(
    requesters: Sequence[str], set_name: str, reasons: Sequence[str]
) -> str:
    lines = [
        f"{mention(requesters)}, Claude did not start a review of the set "
        f"`{set_name}`, because:",
        "",
        *(f"- {reason}" for reason in reasons),
    ]
    return "\n".join(lines) + "\n"


def render_review_started(
    requesters: Sequence[str], set_name: str, run: ReviewRun
) -> str:
    return (
        f"{mention(requesters)}, Claude reviews the set `{set_name}` in "
        f"[this run]({run.url}). It posts its findings on each pull request of "
        "the set, and the merge-set comment shows the state of the review.\n"
    )


def wait_ended(set_review: SetReview, dashboards: Mapping[str, Dashboard]) -> bool:
    """Whether the wait for a person ended since the dashboards of the set
    were written: one of them counts down the wait, and Claude can now
    review the set."""
    return isinstance(set_review.offer, ClaudeOffered) and any(
        WAIT_MARKER in dashboard.body for dashboard in dashboards.values()
    )


def render_wait_ended(state: SetState) -> str:
    authors = []
    for pull_request in state.open_pull_requests():
        if pull_request.author not in authors:
            authors.append(pull_request.author)
    subject = "this pull request" if state.is_alone else f"the set `{state.name}`"
    return (
        f"{mention(authors)}, no person reviewed {subject} in "
        f"{hours_text(PERSON_REVIEW_WAIT)}, so Claude can review it now. To ask "
        'for the review, tick the box in the section "Review by Claude" of the '
        "merge-set comment.\n"
    )


# The requests ----------------------------------------------------------------


@dataclass(frozen=True)
class Dashboard:
    pull_request: PullRequest
    id: int
    node_id: str
    url: str
    body: str


@dataclass(frozen=True)
class Request:
    action: Action
    requester: str
    dashboard: Dashboard


def parse_editor(node: Mapping | None) -> str | None:
    """The user who last edited a comment; None when it was a bot or when
    nobody edited it since it was written."""
    editor = (node or {}).get("editor")
    if not editor or editor.get("__typename") != "User":
        return None
    return editor["login"]


def mention(logins: Sequence[str]) -> str:
    return ", ".join(f"@{login}" for login in logins)


def requesters_of(requests: Sequence[Request]) -> list[str]:
    logins = []
    for request in requests:
        if request.requester not in logins:
            logins.append(request.requester)
    return logins


WRITE_PERMISSIONS = frozenset({"admin", "maintain", "write"})
ADMIN_PERMISSION = "admin"
# The permission of a user who is no collaborator of the repository.
NO_PERMISSION = "none"
# The users the bot acts for when they tick a box, as the dashboard and the
# refusals word it.
WHO_MAY_ASK = (
    "the user who opened every pull request of the set and has write access "
    "to every repository of it, or an admin of every repository of the set"
)


def is_admin_of_the_set(state: SetState, permissions: Mapping[Repository, str]) -> bool:
    """Whether a user, whose permission on each repository of the set is in
    `permissions`, has the admin role on every repository of the set."""
    return all(
        permissions[member.repository] == ADMIN_PERMISSION for member in state.members
    )


@dataclass(frozen=True)
class AccessRefusal:
    """Why the bot does not act for a user who ticked a box: the pull requests
    of the set the user did not open, and the repositories of the set the user
    has no write access to."""

    not_opened: tuple[PullRequest, ...]
    no_write_access: tuple[Repository, ...]


def access_refusal(
    state: SetState, login: str, permissions: Mapping[Repository, str]
) -> AccessRefusal | None:
    """Why the bot does not act for the user `login`, whose permission on each
    repository of the set is in `permissions`; None when it acts. It acts for
    an admin of every repository of the set, and for the user who opened
    every pull request of the set and has write access to every repository
    of it."""
    if is_admin_of_the_set(state, permissions):
        return None
    refusal = AccessRefusal(
        not_opened=tuple(
            pull_request
            for pull_request in state.open_pull_requests()
            if pull_request.author != login
        ),
        no_write_access=tuple(
            member.repository
            for member in state.members
            if permissions[member.repository] not in WRITE_PERMISSIONS
        ),
    )
    if not refusal.not_opened and not refusal.no_write_access:
        return None
    return refusal


# The merge -------------------------------------------------------------------

# GitHub computes whether a pull request merges in the background, after it is
# asked and after each change of the pull request, its base or its checks:
# the bot asks this many times, this far apart, before it takes the answer.
MERGE_STATE_ATTEMPTS = 5
MERGE_STATE_INTERVAL_SECONDS = 2
# The `mergeable_state` of a pull request GitHub has not computed yet.
MERGE_STATE_UNKNOWN = "unknown"
# The `mergeable_state` of a pull request a rule blocks. Right after the bot
# sets the gate green, GitHub can still answer it from before.
MERGE_STATE_BLOCKED = "blocked"
# The `mergeable_state` values of a pull request GitHub merges: `unstable`
# has a failed check that no rule requires, `has_hooks` a pre-receive hook.
MERGEABLE_STATES = frozenset({"clean", "unstable", "has_hooks"})


@dataclass(frozen=True)
class MergedPullRequest:
    pull_request: PullRequest
    commit: str


@dataclass(frozen=True)
class MergeFailure:
    """The pull request GitHub did not merge, and GitHub's reason."""

    pull_request: PullRequest
    reason: str


@dataclass(frozen=True)
class MergeOutcome:
    merged: tuple[MergedPullRequest, ...]
    # None when every pull request merged.
    failure: MergeFailure | None
    not_tried: tuple[PullRequest, ...]

    @property
    def unmerged(self) -> tuple[PullRequest, ...]:
        if self.failure is None:
            return self.not_tried
        return (self.failure.pull_request, *self.not_tried)


@dataclass(frozen=True)
class MergeCommit:
    """The commit that merges a pull request of the set."""

    method: MergeMethod
    # None for the title GitHub gives a merge commit.
    title: str | None
    message: str


def merge_commit(
    set_name: str, pull_request: PullRequest, pull_requests: Sequence[PullRequest]
) -> MergeCommit:
    """A merge commit carries the title of the pull request first in its
    message; a squashed commit is titled as the merge button of GitHub titles
    it. Both name the other pull requests of the set."""
    others = ", ".join(
        other.reference for other in pull_requests if other is not pull_request
    )
    together = f"Merged together with the set `{set_name}`: {others}."
    method = pull_request.repository.merge_method
    if method is MergeMethod.SQUASH:
        return MergeCommit(
            method, f"{pull_request.title} (#{pull_request.number})", together
        )
    return MergeCommit(method, None, f"{pull_request.title}\n\n{together}")


def merged_line(merged: MergedPullRequest, unapproved: Sequence[PullRequest]) -> str:
    """The line of a merge report for a pull request that merged."""
    line = f"- {merged.pull_request.label} merged as `{short(merged.commit)}`"
    if merged.pull_request in unapproved:
        return f"{line} without an approval."
    return f"{line}."


def render_merge_report(
    set_name: str,
    requesters: Sequence[str],
    outcome: MergeOutcome,
    unapproved: Sequence[PullRequest],
) -> str:
    """The report of a merge, on every pull request of the set. `unapproved`
    are the pull requests an admin merged without an approval."""
    merged_lines = [merged_line(merged, unapproved) for merged in outcome.merged]
    if outcome.failure is None:
        lines = [
            f"The set `{set_name}` merged, as {mention(requesters)} asked:",
            "",
            *merged_lines,
        ]
        return "\n".join(lines) + "\n"
    failure = outcome.failure
    lines = [
        (
            f"The merge of the set `{set_name}`, which {mention(requesters)} asked "
            "for, stopped:"
        ),
        "",
        *merged_lines,
        f"- {failure.pull_request.label} did not merge: {failure.reason}",
        *(
            f"- {pull_request.label} was not tried."
            for pull_request in outcome.not_tried
        ),
        "",
        (
            "The pull requests that did not merge are blocked again. Remove the "
            "cause, then merge the rest: tick the box of their merge-set comment "
            "again, or, when one pull request is left alone, use its merge button."
        ),
    ]
    return "\n".join(lines) + "\n"


def render_merged_dashboard(state: SetState, outcome: MergeOutcome) -> str:
    """The dashboard of the pull requests of the set that merged, after a
    merge of the set. `state` is the set before the merge: its merged
    members merged before it."""
    labels = ", ".join(merged.pull_request.label for merged in outcome.merged)
    text = f"{DASHBOARD_MARKER}\n### Set `{state.name}`\n\nMerged together: {labels}."
    if state.merged:
        before = ", ".join(
            pull_request.label for pull_request in state.merged_pull_requests()
        )
        text += f" Merged before them: {before}."
    if outcome.unmerged:
        rest = ", ".join(pull_request.label for pull_request in outcome.unmerged)
        text += f" Not merged: {rest}."
    return text + "\n"


def render_refusal(
    requesters: Sequence[str], set_name: str, reasons: Sequence[str]
) -> str:
    return (
        "\n".join(
            [
                f"{mention(requesters)}, the set `{set_name}` did not merge, because:",
                "",
                *(f"- {reason}" for reason in reasons),
            ]
        )
        + "\n"
    )


def unmergeable_reasons(states: Sequence[tuple[PullRequest, str]]) -> list[str]:
    return [
        f"GitHub does not merge {pull_request.label} yet: its merge state is `{state}`."
        for pull_request, state in states
    ]


def access_refusal_reasons(refusal: AccessRefusal) -> str:
    reasons = []
    if refusal.not_opened:
        reasons.append(
            "you did not open "
            + ", ".join(pull_request.label for pull_request in refusal.not_opened)
        )
    if refusal.no_write_access:
        reasons.append(
            "you have no write access to "
            + ", ".join(repository.name for repository in refusal.no_write_access)
        )
    return ", and ".join(reasons)


# What the bot does for each box, as its refusals word it.
ACTION_TEXTS = {
    Action.MERGE: "merge the set",
    Action.RERUN: "re-run the CI of the set",
    Action.REVIEW: "start a Claude review of the set",
}


def render_access_refusal(action: Action, refusals: Mapping[str, AccessRefusal]) -> str:
    what = ACTION_TEXTS[action]
    lines = [
        f"@{login}, the bot did not {what} for you: "
        f"{access_refusal_reasons(refusal)}. It does that only for {WHO_MAY_ASK}."
        for login, refusal in refusals.items()
    ]
    return "\n\n".join(lines) + "\n"


def render_rerun_report(
    requesters: Sequence[str],
    set_name: str,
    rerun: Sequence[HubRun],
    running: Sequence[HubRun],
) -> str:
    lines = [f"{mention(requesters)}, the out-of-date CI of the set `{set_name}`:", ""]
    if rerun:
        lines.append("Re-run now:")
        lines.extend(f"- {run_link(run)} of {run.repository.name}" for run in rerun)
    if running:
        if rerun:
            lines.append("")
        lines.append("Still running, so not re-run (tick the box again when it ends):")
        lines.extend(f"- {run_link(run)} of {run.repository.name}" for run in running)
    return "\n".join(lines) + "\n"


def render_nothing_to_rerun(requesters: Sequence[str], set_name: str) -> str:
    return (
        f"{mention(requesters)}, no CI of the set `{set_name}` is out of date: "
        f"{CURRENT_HUB_RUNS}.\n"
    )


# The GitHub API --------------------------------------------------------------


class GitHubApi:
    """The REST and GraphQL calls of the bot, with one installation token of
    the merge-set App."""

    PAGE_SIZE = 100

    def __init__(self, token: str):
        self.token = token

    def request(
        self,
        method: str,
        path: str,
        query: Mapping[str, object] | None = None,
        body: object = None,
    ) -> object:
        url = f"{resolve.GITHUB_API}{path}"
        if query:
            url = f"{url}?{urllib.parse.urlencode(query)}"
        request = resolve.github_request(url, self.token)
        request.method = method
        if body is not None:
            request.data = json.dumps(body).encode()
            request.add_header("Content-Type", "application/json")
        text = read_response(request, resolve.API_TIMEOUT_SECONDS, f"{method} {url}")
        return json.loads(text) if text else None

    def pages(
        self,
        path: str,
        query: Mapping[str, object] | None = None,
        key: str | None = None,
    ) -> list:
        """Every item of a list endpoint. `key` names the list in a response
        that wraps it in an object."""
        items: list = []
        page = 1
        while True:
            response = self.request(
                "GET", path, {**(query or {}), "per_page": self.PAGE_SIZE, "page": page}
            )
            batch = response if key is None else response[key]
            items.extend(batch)
            if len(batch) < self.PAGE_SIZE:
                return items
            page += 1

    def graphql(self, query: str, variables: Mapping[str, object]) -> Mapping:
        response = self.request(
            "POST", "/graphql", body={"query": query, "variables": variables}
        )
        if response.get("errors"):
            raise ApiError("GraphQL query", None, json.dumps(response["errors"]))
        return response["data"]

    def download(self, url: str) -> bytes:
        return read_response(
            resolve.github_request(url, self.token),
            resolve.DOWNLOAD_TIMEOUT_SECONDS,
            f"downloading {url}",
        )


def read_response(
    request: urllib.request.Request, timeout: float, action: str
) -> bytes:
    """The body of the response to `request`. A call that fails raises an
    ApiError that names `action`."""
    try:
        with urllib.request.urlopen(request, timeout=timeout) as response:
            return response.read()
    except urllib.error.HTTPError as error:
        raise ApiError(action, error.code, error_message(error)) from error
    except urllib.error.URLError as error:
        raise ApiError(action, None, str(error.reason)) from error
    except resolve.REQUEST_ERRORS as error:
        # urlopen wraps only the failures of sending the request in a
        # URLError: a timeout or a reset connection while it waits for the
        # response, or a response that breaks off, comes as itself.
        raise ApiError(action, None, str(error)) from error


def error_message(error: urllib.error.HTTPError) -> str:
    """The message GitHub puts in the body of a refused call, else the HTTP
    reason: for a body that breaks off, that is no JSON object, or that has
    no message."""
    reason = str(error.reason)
    try:
        body = error.read()
    except resolve.REQUEST_ERRORS:
        return reason
    try:
        document = json.loads(body)
    except (json.JSONDecodeError, UnicodeDecodeError):
        return reason
    if not isinstance(document, Mapping):
        return reason
    message = document.get("message")
    if not isinstance(message, str) or not message:
        return reason
    return message


# The newest time the pull request left the draft state.
READY_FOR_REVIEW_QUERY = """
query($owner: String!, $name: String!, $number: Int!) {
  repository(owner: $owner, name: $name) {
    pullRequest(number: $number) {
      timelineItems(last: 1, itemTypes: [READY_FOR_REVIEW_EVENT]) {
        nodes { ... on ReadyForReviewEvent { createdAt } }
      }
    }
  }
}
"""


def ready_for_review_time(
    pull_request: PullRequest, events: Sequence[Mapping]
) -> datetime:
    """When the pull request, which is not a draft, became ready for review:
    the time of the newest of `events`, the times it left the draft state,
    or the time it opened when it was never a draft."""
    if not events:
        return pull_request.created_at
    return parse_github_time(events[-1]["createdAt"])


REVIEW_DECISION_QUERY = """
query($owner: String!, $name: String!, $number: Int!) {
  repository(owner: $owner, name: $name) {
    pullRequest(number: $number) { reviewDecision }
  }
}
"""

COMMENT_EDITOR_QUERY = """
query($id: ID!) {
  node(id: $id) {
    ... on IssueComment { editor { __typename login } }
  }
}
"""


def quoted(branch: str) -> str:
    """A branch name as a segment of an API path."""
    return urllib.parse.quote(branch, safe="/")


def pull_request_path(repository: Repository, number: int) -> str:
    return f"/repos/{repository.full_name}/pulls/{number}"


def comment_path(dashboard: Dashboard) -> str:
    repository = dashboard.pull_request.repository
    return f"/repos/{repository.full_name}/issues/comments/{dashboard.id}"


def issue_comments_path(repository: Repository, number: int) -> str:
    return f"/repos/{repository.full_name}/issues/{number}/comments"


class GitHubGateway:
    """Everything the sync reads from and writes to GitHub."""

    def __init__(self, api: GitHubApi, bot_login: str, git: GitMerges):
        self.api = api
        self.bot_login = bot_login
        self.git = git
        self.clean_base_merges: dict[tuple[str, str, str], bool] = {}
        self.held_commits: dict[tuple[str, str, str], bool] = {}
        self.rules: dict[str, BranchRules] = {}
        self.bypasses: dict[tuple[str, int], RulesetBypass] = {}
        self.permissions: dict[tuple[str, str], str] = {}
        self.ci_workflows: dict[str, frozenset[int]] = {}
        self.developers: dict[str, bool] = {}

    # Reading the set.

    def branch_head(self, repository: Repository, branch: str) -> str | None:
        try:
            ref = self.api.request(
                "GET", f"/repos/{repository.full_name}/git/ref/heads/{quoted(branch)}"
            )
        except ApiError as error:
            if error.status == 404:
                return None
            raise
        # A ref that names no branch exactly lists the refs it prefixes.
        if not isinstance(ref, Mapping) or ref.get("ref") != f"refs/heads/{branch}":
            return None
        return ref["object"]["sha"]

    def pull_requests_from(
        self, repository: Repository, branch: str
    ) -> list[PullRequest]:
        items = self.api.pages(
            f"/repos/{repository.full_name}/pulls",
            {"state": "all", "head": f"{resolve.ORGANISATION}:{branch}"},
        )
        return [parse_pull_request(repository, item) for item in items]

    def pull_request(self, repository: Repository, number: int) -> PullRequest:
        item = self.api.request("GET", pull_request_path(repository, number))
        return parse_pull_request(repository, item)

    def open_pull_requests(self, repository: Repository) -> list[PullRequest]:
        items = self.api.pages(
            f"/repos/{repository.full_name}/pulls", {"state": "open"}
        )
        return [parse_pull_request(repository, item) for item in items]

    def ready_at(self, pull_request: PullRequest) -> datetime:
        """When the pull request, which is not a draft, became ready for
        review."""
        data = self.api.graphql(
            READY_FOR_REVIEW_QUERY,
            {
                "owner": resolve.ORGANISATION,
                "name": pull_request.repository.name,
                "number": pull_request.number,
            },
        )
        timeline = data["repository"]["pullRequest"]["timelineItems"]
        return ready_for_review_time(pull_request, timeline["nodes"])

    def merge_state(self, pull_request: PullRequest) -> tuple[bool | None, str]:
        """Whether the pull request merges without conflicts (None while GitHub
        computes it), and its `mergeable_state`."""
        item = self.api.request(
            "GET", pull_request_path(pull_request.repository, pull_request.number)
        )
        return item.get("mergeable"), item.get("mergeable_state") or MERGE_STATE_UNKNOWN

    def branch_rules(self, repository: Repository) -> BranchRules:
        if repository.name not in self.rules:
            self.rules[repository.name] = parse_branch_rules(
                self.api.pages(
                    f"/repos/{repository.full_name}/rules/branches/"
                    f"{quoted(repository.integration_branch)}"
                )
            )
        return self.rules[repository.name]

    def ruleset_bypass(self, repository: Repository, ruleset_id: int) -> RulesetBypass:
        """Whether the App bypasses a ruleset that applies to the repository,
        one of its own or one of the organisation."""
        key = (repository.name, ruleset_id)
        if key not in self.bypasses:
            self.bypasses[key] = parse_ruleset_bypass(
                self.api.request(
                    "GET", f"/repos/{repository.full_name}/rulesets/{ruleset_id}"
                )
            )
        return self.bypasses[key]

    def check_runs(self, repository: Repository, commit: str) -> list[CheckRun]:
        return parse_check_runs(
            self.api.pages(
                f"/repos/{repository.full_name}/commits/{commit}/check-runs",
                {"filter": "latest"},
                key="check_runs",
            )
        )

    def statuses(self, repository: Repository, commit: str) -> list[CommitStatus]:
        return parse_statuses(
            self.api.pages(f"/repos/{repository.full_name}/commits/{commit}/statuses")
        )

    def review_decision(self, pull_request: PullRequest) -> ReviewDecision:
        data = self.api.graphql(
            REVIEW_DECISION_QUERY,
            {
                "owner": resolve.ORGANISATION,
                "name": pull_request.repository.name,
                "number": pull_request.number,
            },
        )
        return parse_review_decision(
            data["repository"]["pullRequest"]["reviewDecision"]
        )

    def ci_workflows_of(self, repository: Repository) -> frozenset[int]:
        if repository.name not in self.ci_workflows:
            self.ci_workflows[repository.name] = ci_workflow_ids(
                repository,
                self.api.pages(
                    f"/repos/{repository.full_name}/actions/workflows", key="workflows"
                ),
            )
        return self.ci_workflows[repository.name]

    def ci_run(self, pull_request: PullRequest) -> CiRun | None:
        return latest_ci_run(
            self.pull_request_runs(pull_request),
            self.ci_workflows_of(pull_request.repository),
        )

    def compare(self, repository: Repository, base: str, head: str) -> Comparison:
        """The comparison of `base`, a branch or a commit, with the commit
        `head`. It asks for one commit a page: the bot reads none of the
        commits of the comparison."""
        return parse_comparison(
            self.api.request(
                "GET",
                f"/repos/{repository.full_name}/compare/{quoted(base)}...{head}",
                {"per_page": 1},
            )
        )

    def is_behind(self, pull_request: PullRequest) -> bool:
        comparison = self.compare(
            pull_request.repository, pull_request.base_branch, pull_request.head_commit
        )
        return comparison.behind_by > 0

    def is_clean_base_merge(self, mismatch: Mismatch) -> bool:
        """Whether the head of the branch differs from the commit the job ran
        with by a clean merge of its base alone."""
        return self.differs_by_clean_base_merge(
            mismatch.repository, mismatch.tested.commit, mismatch.current
        )

    def differs_by_clean_base_merge(
        self, repository: Repository, tested: str, head: str
    ) -> bool:
        """Whether `head` differs from the commit `tested` by a clean merge of
        the base of the repository alone. Each pair is checked once a sync:
        the jobs of a run share the commits they ran with."""
        key = (repository.name, tested, head)
        if key not in self.clean_base_merges:
            self.clean_base_merges[key] = self.check_clean_base_merge(
                repository, tested, head
            )
        return self.clean_base_merges[key]

    def check_clean_base_merge(
        self, repository: Repository, tested: str, head: str
    ) -> bool:
        """Whether git merges the commit `tested` and the newest commit of
        the base that the head holds, without a conflict, into the tree of
        the head. A merge that the bot cannot check counts as no clean merge:
        a job that ran `tested` stays out of date, and an approval of
        `tested` is dismissed."""
        try:
            base = self.compare(repository, repository.integration_branch, head)
            merge_base = self.compare(repository, base.merge_base, tested)
        except ApiError as error:
            # GitHub knows no such commit, or no history that the two share.
            if error.status == 404:
                return False
            raise
        try:
            return self.git.is_clean_merge(
                repository, merge_base.merge_base, tested, base.merge_base, head
            )
        except GitError as error:
            warn(
                f"The bot counts {repository.name} at {short(tested)} and at "
                f"{short(head)} as different: it did not check whether "
                f"{short(head)} differs from {short(tested)} by a clean merge of "
                f"`{repository.integration_branch}` alone. {error}"
            )
            return False

    def holds(self, repository: Repository, commit: str, ancestor: str) -> bool:
        """Whether `commit` holds `ancestor`. Each pair is checked once a
        sync: the jobs of a run share the commits they ran with."""
        key = (repository.name, commit, ancestor)
        if key not in self.held_commits:
            self.held_commits[key] = self.check_holds(repository, commit, ancestor)
        return self.held_commits[key]

    def check_holds(self, repository: Repository, commit: str, ancestor: str) -> bool:
        """Whether `ancestor` has no commit that `commit` lacks."""
        try:
            comparison = self.compare(repository, ancestor, commit)
        except ApiError as error:
            # GitHub knows no such commit, or no history that the two share.
            if error.status == 404:
                return False
            raise
        return comparison.behind_by == 0

    def pull_request_runs(self, pull_request: PullRequest) -> list[Mapping]:
        """Every workflow run of the head commit of the pull request, for the
        pull_request event."""
        return self.api.pages(
            f"/repos/{pull_request.repository.full_name}/actions/runs",
            {"head_sha": pull_request.head_commit, "event": "pull_request"},
            key="workflow_runs",
        )

    def hub_runs(self, pull_request: PullRequest) -> list[HubRun]:
        runs = latest_run_of_each_workflow(self.pull_request_runs(pull_request))
        return [self.hub_run(pull_request.repository, run) for run in runs]

    def hub_run(self, repository: Repository, run: Mapping) -> HubRun:
        path = f"/repos/{repository.full_name}/actions/runs/{run['id']}"
        jobs = self.api.pages(f"{path}/jobs", {"filter": "latest"}, key="jobs")
        artifacts = parse_artifacts(
            self.api.pages(f"{path}/artifacts", key="artifacts")
        )
        return HubRun(
            repository=repository,
            id=run["id"],
            name=f"{run['name']} #{run['run_number']}",
            url=run["html_url"],
            completed=run["status"] == "completed",
            records=tuple(
                JobRecord(job, None if artifact.expired else self.tested_set(artifact))
                for job, artifact in recorded_jobs(jobs, artifacts)
            ),
        )

    def tested_set(self, artifact: Artifact) -> TestedSet:
        return parse_tested_set(
            set_record_of_bundle(self.api.download(artifact.download_url))
        )

    # The gate.

    def gate(self, pull_request: PullRequest) -> Gate | None:
        return current_gate(
            self.statuses(pull_request.repository, pull_request.head_commit),
            self.bot_login,
        )

    def post_gate(self, pull_request: PullRequest, gate: Gate) -> None:
        body = {
            "state": gate.state,
            "context": STATUS_CONTEXT,
            "description": gate.description,
        }
        if gate.target_url is not None:
            body["target_url"] = gate.target_url
        self.api.request(
            "POST",
            f"/repos/{pull_request.repository.full_name}/statuses/{pull_request.head_commit}",
            body=body,
        )

    # The comments.

    def dashboard(self, pull_request: PullRequest) -> Dashboard | None:
        """The oldest dashboard the bot wrote on the pull request."""
        path = issue_comments_path(pull_request.repository, pull_request.number)
        for comment in self.api.pages(path):
            if self.is_dashboard(comment):
                return self.dashboard_of(pull_request, comment)
        return None

    def is_dashboard(self, comment: Mapping) -> bool:
        """Whether the bot wrote the comment as a dashboard. Anyone can write
        the marker; only the bot can write as the bot."""
        written_by_the_bot = comment["user"]["login"] == self.bot_login
        return written_by_the_bot and comment["body"].startswith(DASHBOARD_MARKER)

    @staticmethod
    def dashboard_of(pull_request: PullRequest, comment: Mapping) -> Dashboard:
        return Dashboard(
            pull_request=pull_request,
            id=comment["id"],
            node_id=comment["node_id"],
            url=comment["html_url"],
            body=comment["body"],
        )

    def comment_body(self, dashboard: Dashboard) -> str:
        return self.api.request("GET", comment_path(dashboard))["body"]

    def comment_editor(self, dashboard: Dashboard) -> str | None:
        data = self.api.graphql(COMMENT_EDITOR_QUERY, {"id": dashboard.node_id})
        return parse_editor(data["node"])

    def create_comment(self, pull_request: PullRequest, body: str) -> str:
        """Write a comment on the pull request, and return its URL."""
        comment = self.api.request(
            "POST",
            issue_comments_path(pull_request.repository, pull_request.number),
            body={"body": body},
        )
        return comment["html_url"]

    def update_comment(self, dashboard: Dashboard, body: str) -> None:
        self.api.request("PATCH", comment_path(dashboard), body={"body": body})

    # The actions.

    def permission(self, repository: Repository, login: str) -> str:
        key = (repository.name, login)
        if key not in self.permissions:
            self.permissions[key] = self.read_permission(repository, login)
        return self.permissions[key]

    def read_permission(self, repository: Repository, login: str) -> str:
        try:
            response = self.api.request(
                "GET",
                f"/repos/{repository.full_name}/collaborators/{login}/permission",
            )
        except ApiError as error:
            if error.status != 404:
                raise
            return NO_PERMISSION
        return response["permission"]

    def merge(self, pull_request: PullRequest, commit: MergeCommit) -> str:
        """Merge the pull request at its head commit, and return the merge
        commit. A call that fails is checked against the pull request: GitHub
        can merge it and still fail the response."""
        path = pull_request_path(pull_request.repository, pull_request.number)
        body = {
            "merge_method": commit.method.value,
            "sha": pull_request.head_commit,
            "commit_message": commit.message,
        }
        if commit.title is not None:
            body["commit_title"] = commit.title
        try:
            response = self.api.request("PUT", f"{path}/merge", body=body)
            return response["sha"]
        except ApiError as error:
            item = self.api.request("GET", path)
            if item.get("merged"):
                return item["merge_commit_sha"]
            raise MergeRefused(error.message) from error

    def rerun(self, run: HubRun) -> None:
        self.api.request(
            "POST", f"/repos/{run.repository.full_name}/actions/runs/{run.id}/rerun"
        )

    # The Claude review.

    def reviews(self, pull_request: PullRequest) -> list[Review]:
        path = pull_request_path(pull_request.repository, pull_request.number)
        return parse_reviews(self.api.pages(f"{path}/reviews"))

    def review_comments(self, pull_request: PullRequest) -> list[ReviewComment]:
        path = pull_request_path(pull_request.repository, pull_request.number)
        return parse_review_comments(self.api.pages(f"{path}/comments"))

    def dismiss_review(
        self, pull_request: PullRequest, review: Review, message: str
    ) -> None:
        path = pull_request_path(pull_request.repository, pull_request.number)
        self.api.request(
            "PUT",
            f"{path}/reviews/{review.id}/dismissals",
            body={"message": message, "event": "DISMISS"},
        )

    def is_developer(self, login: str) -> bool:
        """Whether the user is an active member of DEVELOPER_TEAM."""
        if login not in self.developers:
            self.developers[login] = self.read_team_membership(login)
        return self.developers[login]

    def read_team_membership(self, login: str) -> bool:
        try:
            membership = self.api.request(
                "GET",
                f"/orgs/{resolve.ORGANISATION}/teams/{DEVELOPER_TEAM}/memberships/"
                f"{urllib.parse.quote(login, safe='')}",
            )
        except ApiError as error:
            if error.status == 404:
                return False
            raise
        return membership.get("state") == "active"

    def review_runs(self) -> list[Mapping]:
        """The newest page of runs of the review workflow, newest first. A
        review run of a set further back is old: the reviews and the review
        comments on its pull requests tell what it gave. No run while the
        workflow is not on the review repository."""
        path = (
            f"/repos/{resolve.ORGANISATION}/{REVIEW_REPOSITORY}/actions/workflows/"
            f"{REVIEW_WORKFLOW}/runs"
        )
        try:
            response = self.api.request(
                "GET",
                path,
                {"event": "workflow_dispatch", "per_page": GitHubApi.PAGE_SIZE},
            )
        except ApiError as error:
            if error.status == 404:
                return []
            raise
        return response["workflow_runs"]

    def review_cause(self, run: ReviewRun) -> str | None:
        """The cause that a review run that did not complete gives in its
        outcome artifact; None when it uploaded none, when the artifact
        expired, or when the bot cannot read it."""
        path = (
            f"/repos/{resolve.ORGANISATION}/{REVIEW_REPOSITORY}/actions/runs/"
            f"{run.id}/artifacts"
        )
        artifacts = parse_artifacts(self.api.pages(path, key="artifacts"))
        artifact = next(
            (
                artifact
                for artifact in artifacts
                if artifact.name == REVIEW_OUTCOME_ARTIFACT and not artifact.expired
            ),
            None,
        )
        if artifact is None:
            return None
        try:
            return parse_review_outcome(
                file_of_bundle(
                    self.api.download(artifact.download_url),
                    REVIEW_OUTCOME_FILE,
                    REVIEW_OUTCOME_ARTIFACT,
                )
            )
        except (MergeSetError, zipfile.BadZipFile) as error:
            warn(f"The bot did not read the cause of the review run {run.url}: {error}")
            return None

    def start_review(self, inputs: Mapping[str, str]) -> ReviewRun:
        """Start the review workflow, and return its run."""
        response = self.api.request(
            "POST",
            f"/repos/{resolve.ORGANISATION}/{REVIEW_REPOSITORY}/actions/workflows/"
            f"{REVIEW_WORKFLOW}/dispatches",
            body={
                "ref": REVIEW_WORKFLOW_REF,
                "inputs": inputs,
                "return_run_details": True,
            },
        )
        run_id, url = dispatched_run(response, REVIEW_WORKFLOW)
        return ReviewRun(run_id, url, completed=False, conclusion=None)

    @staticmethod
    def now() -> datetime:
        return datetime.now(UTC)

    @staticmethod
    def sleep(seconds: float) -> None:
        time.sleep(seconds)


def warn(text: str) -> None:
    """A warning on the run of the sync, which goes on."""
    print(f"::warning::{resolve.escape_workflow_command(text)}")


# The merges git checks -------------------------------------------------------

# The exit code of `git merge-tree` for a merge with conflicts.
MERGE_TREE_CONFLICTS = 1
# A git command that runs longer fails, well within the 15 minutes of the
# sync job.
GIT_TIMEOUT_SECONDS = 120


class GitError(MergeSetError):
    """A git command that failed."""


def github_remote(repository: Repository) -> str:
    return f"https://github.com/{repository.full_name}.git"


class GitMerges:
    """The merges the sync asks git for. Each one runs in a scratch
    repository that fetches the commits it merges from GitHub, with their
    trees and without their files: git fetches the files the merge reads,
    those that both sides changed, as it reads them."""

    def __init__(
        self, token: str, remote_of: Callable[[Repository], str] = github_remote
    ):
        self.remote_of = remote_of
        credentials = base64.b64encode(f"x-access-token:{token}".encode()).decode()
        self.environment = {
            **resolve.git_environment(),
            # The git configuration of the host changes no merge, and the
            # token of the App is the one credential that goes to GitHub.
            "GIT_CONFIG_NOSYSTEM": "1",
            "GIT_CONFIG_GLOBAL": os.devnull,
            "GIT_CONFIG_COUNT": "1",
            "GIT_CONFIG_KEY_0": "http.https://github.com/.extraHeader",
            "GIT_CONFIG_VALUE_0": f"AUTHORIZATION: basic {credentials}",
        }

    def is_clean_merge(
        self,
        repository: Repository,
        merge_base: str,
        ours: str,
        theirs: str,
        head: str,
    ) -> bool:
        """Whether `head` holds the merge of `ours` and `theirs` from
        `merge_base` and nothing else: git merges the two without a
        conflict, into the tree of `head`."""
        with tempfile.TemporaryDirectory(prefix="merge-set-") as scratch:
            self.git(scratch, "init", "--quiet", "--bare")
            self.git(scratch, "remote", "add", "origin", self.remote_of(repository))
            self.git(scratch, "config", "remote.origin.promisor", "true")
            self.git(scratch, "config", "remote.origin.partialCloneFilter", "blob:none")
            self.git(
                scratch,
                "fetch",
                "--quiet",
                "--no-tags",
                "--depth=1",
                "--filter=blob:none",
                "origin",
                *dict.fromkeys((merge_base, ours, theirs, head)),
            )
            merge = (
                "merge-tree",
                "--write-tree",
                f"--merge-base={merge_base}",
                ours,
                theirs,
            )
            merged = self.run(scratch, *merge)
            if merged.returncode == MERGE_TREE_CONFLICTS:
                return False
            if merged.returncode != 0:
                raise git_failure(merge, merged)
            head_tree = self.git(scratch, "rev-parse", f"{head}^{{tree}}")
            return merged.stdout.splitlines()[0] == head_tree.strip()

    def git(self, scratch: str, *arguments: str) -> str:
        """What git prints; a git that fails raises a GitError."""
        result = self.run(scratch, *arguments)
        if result.returncode != 0:
            raise git_failure(arguments, result)
        return result.stdout

    def run(self, scratch: str, *arguments: str) -> subprocess.CompletedProcess:
        command = ["git", "-C", scratch, *arguments]
        try:
            return subprocess.run(
                command,
                capture_output=True,
                text=True,
                env=self.environment,
                timeout=GIT_TIMEOUT_SECONDS,
                check=False,
            )
        except subprocess.TimeoutExpired as error:
            raise GitError(
                f"`git {' '.join(arguments)}` did not end within "
                f"{GIT_TIMEOUT_SECONDS} seconds"
            ) from error


def git_failure(
    arguments: Sequence[str], result: subprocess.CompletedProcess
) -> GitError:
    return GitError(f"`git {' '.join(arguments)}` failed: {result.stderr.strip()}")


# The sync --------------------------------------------------------------------


def read_set(gateway: GitHubGateway, name: str) -> SetState:
    heads = {
        repository: gateway.branch_head(repository, name) for repository in REPOSITORIES
    }
    # A repository without the branch can hold a merged pull request of the
    # set: GitHub deletes the branch of a merged pull request.
    pull_requests = {
        repository: gateway.pull_requests_from(repository, name)
        for repository in REPOSITORIES
    }
    return choose_set(name, heads, pull_requests)


def read_report(
    gateway: GitHubGateway, state: SetState, pull_request: PullRequest
) -> PullRequestReport:
    repository = pull_request.repository
    rules = gateway.branch_rules(repository)
    runs = gateway.check_runs(repository, pull_request.head_commit)
    statuses = gateway.statuses(repository, pull_request.head_commit)
    mergeable, _ = computed_merge_state(gateway, pull_request, {MERGE_STATE_UNKNOWN})
    review = gateway.review_decision(pull_request)
    return PullRequestReport(
        pull_request=pull_request,
        mergeable=mergeable,
        review=review,
        checks=tuple(
            (check, required_check_state(check, runs, statuses))
            for check in rules.required_checks
        ),
        ci=gateway.ci_run(pull_request),
        behind=rules.strict and gateway.is_behind(pull_request),
        stale=(
            tuple(
                stale_jobs(
                    state,
                    gateway.hub_runs(pull_request),
                    gateway.is_clean_base_merge,
                    gateway.holds,
                )
            )
            if repository.hub is not None
            else ()
        ),
        unbypassed_approval_rulesets=(
            unbypassed_approval_rulesets(gateway, repository, rules)
            if review is ReviewDecision.REVIEW_REQUIRED
            else ()
        ),
    )


def unbypassed_approval_rulesets(
    gateway: GitHubGateway, repository: Repository, rules: BranchRules
) -> tuple[str, ...]:
    """The rulesets that require an approval on the integration branch of the
    repository and that the App does not bypass, by name."""
    bypasses = [
        gateway.ruleset_bypass(repository, ruleset_id)
        for ruleset_id in rules.approval_rulesets
    ]
    return tuple(bypass.name for bypass in bypasses if not bypass.bypassed)


def read_reports(
    gateway: GitHubGateway, state: SetState
) -> dict[str, PullRequestReport]:
    """A report of each member with one open pull request, by its label."""
    return {
        member.pull_request.label: read_report(gateway, state, member.pull_request)
        for member in state.members
        if member.pull_request is not None
    }


def read_dashboards(gateway: GitHubGateway, state: SetState) -> dict[str, Dashboard]:
    """The dashboard of each open and merged pull request of the set that has
    one, by its label."""
    dashboards = {}
    for pull_request in [*state.open_pull_requests(), *state.merged_pull_requests()]:
        dashboard = gateway.dashboard(pull_request)
        if dashboard is not None:
            dashboards[pull_request.label] = dashboard
    return dashboards


def read_requests(
    gateway: GitHubGateway, dashboards: Mapping[str, Dashboard]
) -> list[Request]:
    """The ticked boxes of the dashboards, each with the user who ticked it:
    the one who last edited the dashboard. A box no user ticked is dropped;
    the next write of its dashboard clears it."""
    requests = []
    for dashboard in dashboards.values():
        actions = ticked_actions(dashboard.body)
        if not actions:
            continue
        editor = gateway.comment_editor(dashboard)
        if editor is None:
            continue
        requests.extend(Request(action, editor, dashboard) for action in actions)
    return requests


def permissions_in_the_set(
    gateway: GitHubGateway, state: SetState, login: str
) -> dict[Repository, str]:
    """The permission of the user on each repository of the set."""
    return {
        member.repository: gateway.permission(member.repository, login)
        for member in state.members
    }


def access_refusals(
    gateway: GitHubGateway, state: SetState, logins: Sequence[str]
) -> dict[str, AccessRefusal]:
    """Why the bot does not act for each of the users it refuses. A user it
    acts for has no entry."""
    refusals = {}
    for login in logins:
        refusal = access_refusal(
            state, login, permissions_in_the_set(gateway, state, login)
        )
        if refusal is not None:
            refusals[login] = refusal
    return refusals


def set_gate(gateway: GitHubGateway, pull_request: PullRequest, gate: Gate) -> None:
    """Report `gate` on the head of the pull request, unless it is there
    already."""
    if gateway.gate(pull_request) == gate:
        return
    gateway.post_gate(pull_request, gate)


def write_dashboard(
    gateway: GitHubGateway,
    pull_request: PullRequest,
    existing: Dashboard | None,
    body: str,
) -> str:
    """Write `body` as the dashboard of the pull request, and return the URL
    of the dashboard. A dashboard someone edited since this run read it keeps
    that edit: the edit, most likely a ticked box, started a sync that waits
    behind this one and acts on it."""
    if existing is None:
        return gateway.create_comment(pull_request, body)
    if existing.body == body:
        return existing.url
    if gateway.comment_body(existing) != existing.body:
        return existing.url
    gateway.update_comment(existing, body)
    return existing.url


def dashboard_url(
    dashboards: Mapping[str, Dashboard], pull_request: PullRequest
) -> str:
    dashboard = dashboards.get(pull_request.label)
    return dashboard.url if dashboard is not None else pull_request.url


def computed_merge_state(
    gateway: GitHubGateway,
    pull_request: PullRequest,
    waiting_states: frozenset[str] | set[str],
) -> tuple[bool | None, str]:
    """Whether the pull request merges without conflicts and its
    `mergeable_state`, asked again while GitHub has not computed whether it
    merges or while its state is one of `waiting_states`. The last answer
    stands once every attempt is spent."""
    for attempt in range(MERGE_STATE_ATTEMPTS):
        mergeable, state = gateway.merge_state(pull_request)
        if mergeable is not None and state not in waiting_states:
            break
        if attempt + 1 < MERGE_STATE_ATTEMPTS:
            gateway.sleep(MERGE_STATE_INTERVAL_SECONDS)
    return mergeable, state


def mergeable_states(unapproved: bool) -> frozenset[str]:
    """The merge states of a pull request the bot merges. GitHub reports a
    pull request that lacks an approval as blocked, and the App, a bypass
    actor of the rulesets that require one, merges it all the same."""
    if unapproved:
        return MERGEABLE_STATES | {MERGE_STATE_BLOCKED}
    return MERGEABLE_STATES


def final_merge_state(
    gateway: GitHubGateway, pull_request: PullRequest, unapproved: bool
) -> str:
    """The merge state of a pull request right after the bot set its gate
    green. GitHub can answer blocked from before the gate turned green, so the
    bot asks again while it does, unless the pull request lacks an approval
    and stays blocked for that alone."""
    waiting = (
        {MERGE_STATE_UNKNOWN}
        if unapproved
        else {MERGE_STATE_UNKNOWN, MERGE_STATE_BLOCKED}
    )
    return computed_merge_state(gateway, pull_request, waiting)[1]


def merge_in_order(
    gateway: GitHubGateway, set_name: str, pull_requests: Sequence[PullRequest]
) -> MergeOutcome:
    """Merge the pull requests one after the other, and stop at the first one
    GitHub does not merge."""
    merged = []
    for index, pull_request in enumerate(pull_requests):
        try:
            commit = gateway.merge(
                pull_request, merge_commit(set_name, pull_request, pull_requests)
            )
        except MergeRefused as refusal:
            return MergeOutcome(
                merged=tuple(merged),
                failure=MergeFailure(pull_request, str(refusal)),
                not_tried=tuple(pull_requests[index + 1 :]),
            )
        merged.append(MergedPullRequest(pull_request, commit))
    return MergeOutcome(tuple(merged), None, ())


def block_again(
    gateway: GitHubGateway,
    set_name: str,
    pull_requests: Sequence[PullRequest],
    dashboards: Mapping[str, Dashboard],
) -> None:
    for pull_request in pull_requests:
        gateway.post_gate(
            pull_request, member_gate(set_name, dashboard_url(dashboards, pull_request))
        )


def merge_the_set(
    gateway: GitHubGateway,
    state: SetState,
    reports: Mapping[str, PullRequestReport],
    dashboards: Mapping[str, Dashboard],
    request: Request,
    requesters: Sequence[str],
    run_url: str,
) -> None:
    asked_on = request.dashboard.pull_request
    admin = is_admin_of_the_set(
        state, permissions_in_the_set(gateway, state, request.requester)
    )
    blockers = blockers_for(set_blockers(state, reports), admin)
    if blockers:
        gateway.create_comment(
            asked_on,
            render_refusal(
                requesters, state.name, [blocker.text for blocker in blockers]
            ),
        )
        return
    pull_requests = [member.pull_request for member in state.members]
    unapproved = [
        pull_request
        for pull_request in pull_requests
        if reports[pull_request.label].review is ReviewDecision.REVIEW_REQUIRED
    ]
    for pull_request in pull_requests:
        gateway.post_gate(
            pull_request, merging_gate(state.name, request.requester, run_url)
        )
    merge_states = [
        (
            pull_request,
            final_merge_state(gateway, pull_request, pull_request in unapproved),
        )
        for pull_request in pull_requests
    ]
    unmergeable = [
        (pull_request, merge_state)
        for pull_request, merge_state in merge_states
        if merge_state not in mergeable_states(pull_request in unapproved)
    ]
    if unmergeable:
        block_again(gateway, state.name, pull_requests, dashboards)
        gateway.create_comment(
            asked_on,
            render_refusal(requesters, state.name, unmergeable_reasons(unmergeable)),
        )
        return
    outcome = merge_in_order(gateway, state.name, pull_requests)
    block_again(gateway, state.name, outcome.unmerged, dashboards)
    report = render_merge_report(state.name, requesters, outcome, unapproved)
    # The merged pull requests of the set get the report too: a box ticked on
    # the dashboard of one of them asked for the merge.
    for pull_request in [*pull_requests, *state.merged_pull_requests()]:
        gateway.create_comment(pull_request, report)
    write_merged_dashboards(
        gateway,
        [
            *(merged.pull_request for merged in outcome.merged),
            *state.merged_pull_requests(),
        ],
        dashboards,
        render_merged_dashboard(state, outcome),
    )


def rerun_out_of_date(
    gateway: GitHubGateway,
    state: SetState,
    reports: Mapping[str, PullRequestReport],
    request: Request,
    requesters: Sequence[str],
) -> None:
    asked_on = request.dashboard.pull_request
    runs = stale_runs(reports)
    if not runs:
        gateway.create_comment(
            asked_on, render_nothing_to_rerun(requesters, state.name)
        )
        return
    rerun = [run for run in runs if run.completed]
    running = [run for run in runs if not run.completed]
    for run in rerun:
        gateway.rerun(run)
    gateway.create_comment(
        asked_on, render_rerun_report(requesters, state.name, rerun, running)
    )


def start_review(
    gateway: GitHubGateway,
    state: SetState,
    request: Request,
    requesters: Sequence[str],
) -> ReviewRun | None:
    """Start the Claude review of the set, or answer why not; the run it
    started, if any."""
    asked_on = request.dashboard.pull_request
    mergeable = {
        pull_request.label: computed_merge_state(
            gateway, pull_request, {MERGE_STATE_UNKNOWN}
        )[0]
        for pull_request in state.open_pull_requests()
    }
    reasons = review_refusal_reasons(
        state, read_set_review(gateway, state, started=None), mergeable
    )
    if not reasons:
        try:
            run = gateway.start_review(review_inputs(state, request))
        except ApiError as error:
            if error.status != 404:
                raise
            reasons = [REVIEW_WORKFLOW_MISSING]
        else:
            gateway.create_comment(
                asked_on, render_review_started(requesters, state.name, run)
            )
            return run
    gateway.create_comment(
        asked_on, render_review_refusal(requesters, state.name, reasons)
    )
    return None


def handle_requests(
    gateway: GitHubGateway,
    state: SetState,
    dashboards: Mapping[str, Dashboard],
    requests: Sequence[Request],
    run_url: str,
) -> ReviewRun | None:
    """Act on each action once, for the first user who asked for it and whom
    the bot acts for: an admin of every repository of the set, or the user
    who opened every pull request of the set and has write access to every
    repository of it. Every other user who asked for it gets the reason the
    bot does not act for them, and the reports name the users it acts for
    alone. The re-run goes first. Both read the reports from before the
    re-run, so a merge asked for at the same time stops on the CI that the
    re-run finds out of date. The review goes last, and a set that is alone
    has the review box alone. Returns the review run that it started, if
    any."""
    actions = (
        (Action.REVIEW,)
        if state.is_alone
        else (Action.RERUN, Action.MERGE, Action.REVIEW)
    )
    reports = None if state.is_alone else read_reports(gateway, state)
    started = None
    for action in actions:
        asked = [request for request in requests if request.action is action]
        if not asked:
            continue
        refusals = access_refusals(gateway, state, requesters_of(asked))
        if refusals:
            refused = next(
                request for request in asked if request.requester in refusals
            )
            gateway.create_comment(
                refused.dashboard.pull_request, render_access_refusal(action, refusals)
            )
        allowed = [request for request in asked if request.requester not in refusals]
        if not allowed:
            continue
        requesters = requesters_of(allowed)
        match action:
            case Action.RERUN:
                rerun_out_of_date(gateway, state, reports, allowed[0], requesters)
            case Action.MERGE:
                merge_the_set(
                    gateway, state, reports, dashboards, allowed[0], requesters, run_url
                )
            case Action.REVIEW:
                started = start_review(gateway, state, allowed[0], requesters)
    return started


def write_merged_dashboards(
    gateway: GitHubGateway,
    pull_requests: Sequence[PullRequest],
    dashboards: Mapping[str, Dashboard],
    body: str,
) -> None:
    """Write `body` as the dashboard of each merged pull request that has
    one. The bot writes no new dashboard on a merged pull request."""
    for pull_request in pull_requests:
        dashboard = dashboards.get(pull_request.label)
        if dashboard is not None:
            write_dashboard(gateway, pull_request, dashboard, body)


def read_set_review(
    gateway: GitHubGateway, state: SetState, started: ReviewRun | None
) -> SetReview | None:
    """The Claude review of the set; None when its dashboard does not offer
    it. `started` is the review run that this sync started, if any: GitHub
    can list the runs of a workflow without the newest one for a while. The
    approvals of the reviewer that stand are read after the dismissal of the
    stale ones, so each one is of the head or of a commit that the head
    differs from by a clean merge of its base alone."""
    open_pull_requests = state.open_pull_requests()
    if not opened_by_developers(open_pull_requests, gateway.is_developer):
        return None
    reviews_of = {
        pull_request.label: gateway.reviews(pull_request)
        for pull_request in open_pull_requests
    }
    offer = review_offer(
        open_pull_requests,
        wait_end(open_pull_requests, gateway.ready_at),
        person_reviews(open_pull_requests, reviews_of, gateway.permission),
        gateway.now(),
    )
    pull_requests = []
    for pull_request in open_pull_requests:
        reviews = reviews_of[pull_request.label]
        approvals = reviewer_approvals(reviews)
        approved_at = approvals[-1].commit if approvals else None
        pull_requests.append(
            PullRequestReview(
                pull_request=pull_request,
                approved_at=approved_at,
                open_blocking=open_blocking_count(
                    gateway.review_comments(pull_request)
                ),
                reviewed=any(review.author == REVIEWER_LOGIN for review in reviews),
                decision=(
                    gateway.review_decision(pull_request)
                    if approved_at is not None
                    else None
                ),
            )
        )
    run = started or latest_review_run(gateway.review_runs(), state.name)
    cause = (
        gateway.review_cause(run)
        if run is not None and run.completed and not run.succeeded
        else None
    )
    return SetReview(offer, tuple(pull_requests), run, cause)


def dismiss_stale_approvals(gateway: GitHubGateway, state: SetState) -> None:
    """Dismiss each approval of the reviewer of a commit that the head of its
    pull request moved away from by more than a clean merge of its base. The
    sync does it first, before it writes any gate: a new head has no
    `merge-set` status until then, so no pull request merges with a commit
    that the reviewer did not read. The reviews of persons stay."""
    for pull_request in state.open_pull_requests():
        for approval in stale_reviewer_approvals(
            pull_request,
            gateway.reviews(pull_request),
            gateway.differs_by_clean_base_merge,
        ):
            gateway.dismiss_review(
                pull_request, approval, dismissal_message(pull_request)
            )


def publish(
    gateway: GitHubGateway,
    state: SetState,
    dashboards: Mapping[str, Dashboard],
    started_review: ReviewRun | None = None,
) -> None:
    """Write the dashboard and the gate of each open pull request of the set,
    and the same dashboard on each merged pull request of the set that has
    one, so every dashboard of the set says the same. A pull request that is
    alone gets a dashboard when it has the section of the Claude review, and
    keeps the one it has otherwise. When the wait for a person ended since
    `dashboards` were written, tell the authors of the set that Claude can
    review it: an edit of a comment sends no notification."""
    set_review = read_set_review(gateway, state, started_review)
    if state.is_alone:
        body = render_alone_dashboard(state, set_review)
        for pull_request in state.open_pull_requests():
            set_gate(gateway, pull_request, alone_gate(state.name))
            dashboard = dashboards.get(pull_request.label)
            if dashboard is not None or set_review is not None:
                write_dashboard(gateway, pull_request, dashboard, body)
    else:
        body = render_set_dashboard(state, read_reports(gateway, state), set_review)
        for pull_request in state.open_pull_requests():
            url = write_dashboard(
                gateway, pull_request, dashboards.get(pull_request.label), body
            )
            set_gate(gateway, pull_request, member_gate(state.name, url))
    write_merged_dashboards(gateway, state.merged_pull_requests(), dashboards, body)
    if set_review is not None and wait_ended(set_review, dashboards):
        gateway.create_comment(state.open_pull_requests()[0], render_wait_ended(state))


def no_set_gate(pull_request: PullRequest) -> Gate:
    if pull_request.from_fork:
        return Gate("success", "From a fork, so not part of a set", None)
    return Gate(
        "success",
        status_description(f"From {pull_request.head_branch}, so not part of a set"),
        None,
    )


def sync(gateway: GitHubGateway, event: Event, run_url: str) -> None:
    set_name = event.set_name
    if set_name is None:
        if event.pull_request is None:
            return
        pull_request = gateway.pull_request(event.repository, event.pull_request)
        if pull_request.is_open:
            set_gate(gateway, pull_request, no_set_gate(pull_request))
        return
    state = read_set(gateway, set_name)
    dismiss_stale_approvals(gateway, state)
    # The bodies as this run first read them: a dashboard someone edits
    # during the run is left for the sync that edit starts.
    dashboards = read_dashboards(gateway, state)
    requests = read_requests(gateway, dashboards)
    started_review = None
    if requests:
        started_review = handle_requests(gateway, state, dashboards, requests, run_url)
        state = read_set(gateway, set_name)
    publish(gateway, state, dashboards, started_review)


# The relay -------------------------------------------------------------------

# The workflow of peppy that the relay starts, and the branch it runs from:
# that branch alone reaches the `merge-set` environment, which holds the key of
# the App for the sync.
SYNC_WORKFLOW = "merge-set.yml"
SYNC_WORKFLOW_REF = resolve.PEPPY_DEV_BRANCH


def dispatch_inputs(repository: Repository, subject: RelaySubject) -> dict[str, str]:
    """The inputs of the Merge set workflow for the subject of an event."""
    if subject.branch is None:
        raise MergeSetError("the subject of the event names no branch")
    return {
        "repository": repository.name,
        "branch": subject.branch,
        # A pull request from a fork deleted since it opened has no head
        # repository: it is from a fork all the same.
        "head-repository": subject.head_repository or "",
        "pull-request": ""
        if subject.pull_request is None
        else str(subject.pull_request),
    }


@dataclass(frozen=True)
class TickedBoxes:
    """The boxes that an edit of a merge-set comment ticked, and the user who
    made the edit."""

    requester: str
    actions: tuple[Action, ...]


def ticked_boxes_of_edit(payload: Mapping) -> TickedBoxes:
    """The boxes ticked in the comment after the edit and not before it. An
    edit that ticked no box (it unticked one, or changed other text) gives
    none. An edit event without the text from before the edit did not change
    the text, so it ticked no box."""
    try:
        body = payload["comment"]["body"]
        requester = payload["sender"]["login"]
        before = ((payload.get("changes") or {}).get("body") or {}).get("from", body)
    except (KeyError, TypeError) as error:
        raise MergeSetError(f"the `issue_comment` event lacks {error}") from error
    ticked_before = ticked_actions(before)
    return TickedBoxes(
        requester=requester,
        actions=tuple(
            action for action in ticked_actions(body) if action not in ticked_before
        ),
    )


def request_text(action: Action, set_name: str) -> str:
    match action:
        case Action.MERGE:
            return f"merge the set `{set_name}`"
        case Action.RERUN:
            return f"re-run the out-of-date CI of the set `{set_name}`"
        case Action.REVIEW:
            return f"have Claude review the set `{set_name}`"


def render_request_received(ticked: TickedBoxes, set_name: str, run_url: str) -> str:
    requests = " and to ".join(
        request_text(action, set_name) for action in ticked.actions
    )
    return (
        f"@{ticked.requester}, the merge-set bot got your request to {requests}. "
        f"[This run]({run_url}) acts on it, and the bot reports the result here.\n"
    )


def dispatched_run(response: object, workflow: str) -> tuple[int, str]:
    """The id and the URL of the run that a dispatch of `workflow` with
    `return_run_details` started."""
    run_id = response.get("workflow_run_id") if isinstance(response, Mapping) else None
    url = response.get("html_url") if isinstance(response, Mapping) else None
    if not isinstance(run_id, int) or not isinstance(url, str):
        raise MergeSetError(
            f"the dispatch of {workflow} names no run: {json.dumps(response)}"
        )
    return run_id, url


def start_sync(api: GitHubApi, inputs: Mapping[str, str]) -> str:
    """Start the Merge set workflow of peppy, and return the URL of its run."""
    response = api.request(
        "POST",
        f"/repos/{resolve.PEPPY_REPOSITORY}/actions/workflows/{SYNC_WORKFLOW}/dispatches",
        body={"ref": SYNC_WORKFLOW_REF, "inputs": inputs, "return_run_details": True},
    )
    return dispatched_run(response, SYNC_WORKFLOW)[1]


@dataclass(frozen=True)
class TokenScope:
    """The repositories and the permissions of an installation token of the
    merge-set App."""

    repositories: tuple[str, ...]
    permissions: tuple[tuple[str, str], ...]


# The token that starts the Merge set workflow: it can start the workflows of
# peppy and do nothing else.
SYNC_START_SCOPE = TokenScope((PEPPY.name,), (("actions", "write"),))


def pull_request_scope(repository: Repository) -> TokenScope:
    """The token that reads the pull request of a dashboard edit and answers
    a ticked box on it: it can write the pull requests of the repository of
    the event and do nothing else."""
    return TokenScope((repository.name,), (("pull_requests", "write"),))


def relay(
    event_name: str,
    payload: Mapping,
    bot_login: str,
    api_for: Callable[[TokenScope], GitHubApi],
) -> str:
    """Start the Merge set workflow of peppy for a webhook event that can
    change a set, and answer the boxes that the event ticked. `api_for` gives
    the API of a token of the scope it is given. Returns what the relay did,
    for the log of the webhook."""
    subject = relay_subject(event_name, payload, bot_login)
    if subject is None:
        return f"The `{event_name}` event cannot change a set."
    full_name = (payload.get("repository") or {}).get("full_name", "")
    repository = subject.repository or repository_of_full_name(full_name)
    if repository is None:
        return f"No set spans the repository `{full_name}`."
    if subject.branch is None:
        subject = subject_of_pull_request(
            api_for(pull_request_scope(repository)).request(
                "GET", pull_request_path(repository, subject.pull_request)
            )
        )
    inputs = dispatch_inputs(repository, subject)
    run_url = start_sync(api_for(SYNC_START_SCOPE), inputs)
    started = f"Started {SYNC_WORKFLOW} of peppy for {json.dumps(inputs)}: {run_url}"
    if event_name != "issue_comment":
        return started
    ticked = ticked_boxes_of_edit(payload)
    if not ticked.actions:
        return started
    api_for(pull_request_scope(repository)).request(
        "POST",
        issue_comments_path(repository, subject.pull_request),
        body={"body": render_request_received(ticked, subject.branch, run_url)},
    )
    return (
        f"{started}. Answered the request of @{ticked.requester} "
        f"on #{subject.pull_request}."
    )


# The wait syncs --------------------------------------------------------------


def open_sets(gateway: GitHubGateway) -> dict[str, list[PullRequest]]:
    """The open pull requests of each set, by set name, in the order of
    REPOSITORIES."""
    sets: dict[str, list[PullRequest]] = {}
    for repository in REPOSITORIES:
        for pull_request in gateway.open_pull_requests(repository):
            name = set_name_of(pull_request.head_branch, pull_request.from_fork)
            if name is not None:
                sets.setdefault(name, []).append(pull_request)
    return sets


def waiting_sets(gateway: GitHubGateway) -> list[PullRequest]:
    """An open pull request of each set whose sync shows a new state of the
    wait for a person: the wait goes on, or it ended less than
    WAIT_SYNC_MARGIN ago and no person reviewed the set, so the sync shows the
    box of the review and tells the authors. Every other change of the wait
    is an event of GitHub that starts a sync: a pull request that opens, that
    leaves the draft state or that closes, and a review."""
    now = gateway.now()
    waiting = []
    for pull_requests in open_sets(gateway).values():
        if not opened_by_developers(pull_requests, gateway.is_developer):
            continue
        ends_at = wait_end(pull_requests, gateway.ready_at)
        if ends_at is None or now >= ends_at + WAIT_SYNC_MARGIN:
            continue
        reviews_of = {
            pull_request.label: gateway.reviews(pull_request)
            for pull_request in pull_requests
        }
        offer = review_offer(
            pull_requests,
            ends_at,
            person_reviews(pull_requests, reviews_of, gateway.permission),
            now,
        )
        if not isinstance(offer, ReviewedByPerson):
            waiting.append(pull_requests[0])
    return waiting


def start_wait_syncs(gateway: GitHubGateway, dispatch_api: GitHubApi) -> list[str]:
    """Start the sync of each set of `waiting_sets`, with the API of a token
    that can start the workflows of peppy, and return the URLs of the
    runs."""
    return [
        start_sync(
            dispatch_api,
            dispatch_inputs(
                pull_request.repository,
                RelaySubject(
                    pull_request.number,
                    pull_request.head_branch,
                    pull_request.repository.full_name,
                ),
            ),
        )
        for pull_request in waiting_sets(gateway)
    ]


# The commands ----------------------------------------------------------------


def required(environment: Mapping[str, str], name: str) -> str:
    value = environment.get(name, "")
    if not value:
        raise MergeSetError(
            f"{name} is not set; this script runs in a GitHub Actions step"
        )
    return value


def run_sync(environment: Mapping[str, str]) -> None:
    event = parse_event(
        repository=environment.get("EVENT_REPOSITORY", ""),
        branch=environment.get("EVENT_BRANCH", ""),
        head_repository=environment.get("EVENT_HEAD_REPOSITORY", ""),
        pull_request=environment.get("EVENT_PULL_REQUEST", ""),
    )
    bot_login = required(environment, "MERGE_SET_BOT_LOGIN")
    # The URL of this run of the Merge set workflow.
    run_url = (
        f"{required(environment, 'GITHUB_SERVER_URL')}/"
        f"{required(environment, 'GITHUB_REPOSITORY')}/actions/runs/"
        f"{required(environment, 'GITHUB_RUN_ID')}"
    )
    token = required(environment, "MERGE_SET_TOKEN")
    gateway = GitHubGateway(GitHubApi(token), bot_login, GitMerges(token))
    sync(gateway, event, run_url)


def run_wait_syncs(environment: Mapping[str, str]) -> None:
    token = required(environment, "MERGE_SET_TOKEN")
    gateway = GitHubGateway(
        GitHubApi(token),
        required(environment, "MERGE_SET_BOT_LOGIN"),
        GitMerges(token),
    )
    dispatch_api = GitHubApi(required(environment, "MERGE_SET_DISPATCH_TOKEN"))
    for run_url in start_wait_syncs(gateway, dispatch_api):
        print(f"Started the sync of a set that waits for a person: {run_url}")


def run_repositories() -> None:
    resolve.append_to_runner_file(
        "GITHUB_OUTPUT", f"repositories={repository_list()}\n"
    )


def parse_arguments(argv: Sequence[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Merge the pull requests of a set together."
    )
    commands = parser.add_subparsers(
        dest="command", metavar="subcommand", required=True
    )
    commands.add_parser(
        "repositories",
        help="Name every repository of a set in the step's `repositories` output.",
    )
    commands.add_parser(
        "sync",
        help="Read the set of the event, act on its ticked boxes, and publish it.",
    )
    commands.add_parser(
        "wait-syncs",
        help="Start the sync of each set that waits for the review of a person.",
    )
    return parser.parse_args(argv)


def main(argv: Sequence[str]) -> int:
    arguments = parse_arguments(argv)
    try:
        match arguments.command:
            case "repositories":
                run_repositories()
            case "sync":
                run_sync(os.environ)
            case "wait-syncs":
                run_wait_syncs(os.environ)
    except (MergeSetError, resolve.ResolveError) as error:
        print(f"::error::{resolve.escape_workflow_command(str(error))}")
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
