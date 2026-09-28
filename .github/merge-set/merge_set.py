#!/usr/bin/env python3
"""Merge the pull requests of a set together.

A change that spans several repositories lands as pull requests that share one
head branch name, the set name (see .github/actions/hub-ci-peppy/resolve.py).
The CI of each hub pull request tests it with the other branches of the set,
so the pull requests of a set are green together and merge together.

The `merge-set` commit status is a required check of peppy `dev` and of the
`main` of each hub, and only the merge-set GitHub App reports it:

- A pull request that is alone, because no other repository has a change on
  its branch, gets a green status, and its own merge button merges it.
- A pull request of a set gets a pending status, so its merge button stays
  blocked, and a comment, the dashboard, that lists every repository of the
  set with the state of the CI run of its head, says what blocks the merge,
  and holds two boxes: "Merge" and "Re-run the out-of-date CI".

Ticking "Merge" asks the bot to merge the set. The bot checks every pull
request of the set, sets the status green on each, checks that GitHub merges
each one, and merges them one after the other, peppy first. GitHub merges one
pull request at a time, so a merge that fails part way leaves the set partly
merged: the bot then blocks the pull requests that did not merge and reports
what merged on every pull request of the set.

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
of the set than the head of that branch. Every job that runs the hub-ci-peppy
action uploads the set it ran with, its set record, named after the check run
of the job. A job that does not run the action reads no other repository, so
it has no record and is never out of date. Ticking "Re-run" re-runs every
finished hub run that is out of date.

The workflow merge-set-events.yml of each repository of a set hands every
event that can change the set to the relay (the `relay` subcommand, in the
reusable workflow merge-set-relay.yml), which starts the Merge set workflow of
peppy (merge-set.yml) for the branch of the event. The start and the end of
each CI run, a re-run included, are such events, so the dashboard follows the
state of the CI. When the event is a box that a user ticked, the relay also
answers at once on that pull request, with a link to the run it started. The
Merge set workflow runs `sync`, one run at a time for each set. `sync` holds
no state of its own: it reads the whole set from GitHub on every run (the
branches, the pull requests, their checks, CI runs and reviews, the set
records of the hub runs, the dashboards and the user who last edited each
one), so a run that GitHub drops from its queue loses nothing: the next run of
the set finds the same ticked box.

The decisions are pure functions of their inputs, tested in test_merge_set.py.
The I/O around them is kept thin, in GitHubGateway. Standard library only: it
runs on the runner's python3.
"""

from __future__ import annotations

import argparse
import io
import json
import os
import re
import sys
import time
import urllib.error
import urllib.parse
import urllib.request
import zipfile
from collections.abc import Mapping, Sequence
from dataclasses import dataclass
from enum import Enum
from pathlib import Path

# resolve.py of the hub-ci-peppy action holds the one model of the hubs, of the
# set name and of the set records, and the GitHub request every script of
# this repository makes.
sys.path.insert(
    0, str(Path(__file__).resolve().parents[1] / "actions" / "hub-ci-peppy")
)
import resolve  # noqa: E402


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


@dataclass(frozen=True)
class Repository:
    name: str
    # The branch every pull request of a set targets in this repository.
    integration_branch: str
    # The hub this repository is; None for peppy.
    hub: resolve.Hub | None

    @property
    def full_name(self) -> str:
        return f"{resolve.ORGANISATION}/{self.name}"


PEPPY = Repository("peppy", resolve.PEPPY_DEV_BRANCH, None)

# Every repository a set can span, in the order the bot merges them: peppy
# first, then the hubs in the order of their repository ids. The order is
# fixed, so every report of a merge reads the same way.
REPOSITORIES = (
    PEPPY,
    *(Repository(hub.name, resolve.HUB_MAIN_BRANCH, hub) for hub in resolve.HUBS),
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


def repository_of_full_name(full_name: str) -> Repository:
    """The repository GITHUB_REPOSITORY names, `<organisation>/<name>`."""
    organisation, _, name = full_name.partition("/")
    if organisation.casefold() != resolve.ORGANISATION.casefold():
        raise MergeSetError(
            f"`{full_name}` is not a repository of {resolve.ORGANISATION}"
        )
    return repository_named(name)


def repository_list() -> str:
    """Every repository of a set, comma-separated: the `repositories` of the
    token that reaches all of them."""
    return ",".join(repository.name for repository in REPOSITORIES)


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
        """The set of the event: its branch, unless the branch is in a fork or
        is an integration branch, which no set is named after."""
        if self.from_fork or self.branch in resolve.BRANCHES_WITHOUT_SET_NAME:
            return None
        return self.branch


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


def relay_subject(event_name: str, payload: Mapping) -> RelaySubject | None:
    """The subject of an event the relay hands on; None for an event that
    cannot change a set (the deletion of a tag)."""
    try:
        match event_name:
            case "pull_request_target":
                pull_request = payload["pull_request"]
                head = pull_request["head"]
                return RelaySubject(
                    pull_request=pull_request["number"],
                    branch=head["ref"],
                    head_repository=(head.get("repo") or {}).get("full_name"),
                )
            case "issue_comment":
                return RelaySubject(payload["issue"]["number"], None, None)
            case "workflow_run":
                run = payload["workflow_run"]
                pull_requests = run.get("pull_requests") or []
                return RelaySubject(
                    pull_request=pull_requests[0]["number"] if pull_requests else None,
                    branch=run["head_branch"],
                    head_repository=(run.get("head_repository") or {}).get("full_name"),
                )
            case "delete":
                if payload["ref_type"] != "branch":
                    return None
                return RelaySubject(
                    None, payload["ref"], payload["repository"]["full_name"]
                )
    except (KeyError, TypeError, IndexError) as error:
        raise MergeSetError(f"the `{event_name}` event lacks {error}") from error
    raise MergeSetError(f"the relay hands on no `{event_name}` event")


# The pull requests and the set ------------------------------------------------


@dataclass(frozen=True)
class PullRequest:
    repository: Repository
    number: int
    url: str
    title: str
    is_open: bool
    merged: bool
    draft: bool
    head_commit: str
    head_branch: str
    base_branch: str
    from_fork: bool

    @property
    def label(self) -> str:
        return f"{self.repository.name}#{self.number}"

    @property
    def reference(self) -> str:
        """The pull request as GitHub links it from any repository."""
        return f"{self.repository.full_name}#{self.number}"


def parse_pull_request(repository: Repository, item: Mapping) -> PullRequest:
    """A pull request as the REST API returns it, in a list or alone."""
    head_repository = (item["head"].get("repo") or {}).get("full_name")
    return PullRequest(
        repository=repository,
        number=item["number"],
        url=item["html_url"],
        title=item["title"],
        is_open=item["state"] == "open",
        merged=item.get("merged_at") is not None,
        draft=bool(item.get("draft")),
        head_commit=item["head"]["sha"],
        head_branch=item["head"]["ref"],
        base_branch=item["base"]["ref"],
        from_fork=head_repository != item["base"]["repo"]["full_name"],
    )


@dataclass(frozen=True)
class Member:
    """A repository of the set: one that has a change on the branch of the
    set."""

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
class SetState:
    name: str
    # Ordered as REPOSITORIES.
    members: tuple[Member, ...]
    # The repositories that still hold the branch of a pull request that
    # merged it: a branch left behind, not a change of the set.
    left_behind: tuple[Repository, ...]

    @property
    def is_alone(self) -> bool:
        return len(self.members) < 2

    def open_pull_requests(self) -> list[PullRequest]:
        return [
            pull_request
            for member in self.members
            for pull_request in member.open_pull_requests
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


def choose_set(
    name: str,
    heads: Mapping[Repository, str | None],
    pull_requests: Mapping[Repository, Sequence[PullRequest]],
) -> SetState:
    """The set named `name`, from the head of its branch in each repository
    (None where there is no such branch) and the pull requests from that
    branch in each repository."""
    members, left_behind = [], []
    for repository in REPOSITORIES:
        head = heads[repository]
        if head is None:
            continue
        own = [
            pull_request
            for pull_request in pull_requests[repository]
            if not pull_request.from_fork and pull_request.head_branch == name
        ]
        open_ones = tuple(pull_request for pull_request in own if pull_request.is_open)
        if not open_ones and is_left_behind(head, own):
            left_behind.append(repository)
            continue
        members.append(Member(repository, head, open_ones))
    return SetState(name, tuple(members), tuple(left_behind))


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

# The name of the one CI workflow of peppy and of each hub, which the
# workflow_run trigger of merge-set-events.yml names too.
CI_WORKFLOW = "Tests"


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


def set_record_of_bundle(bundle: bytes) -> str:
    """The set record in the zip GitHub serves an artifact as."""
    with zipfile.ZipFile(io.BytesIO(bundle)) as archive:
        if resolve.SET_RECORD_FILE not in archive.namelist():
            raise MergeSetError(
                f"the set record artifact holds {', '.join(archive.namelist())}, "
                f"not {resolve.SET_RECORD_FILE}"
            )
        return archive.read(resolve.SET_RECORD_FILE).decode()


@dataclass(frozen=True)
class Mismatch:
    """A member of the set a job ran at another commit than the head of its
    branch. The commit alone decides: a job that ran `main` at the commit a
    branch of the set still points at ran the same content."""

    repository: Repository
    tested: TestedRef
    current: str


@dataclass(frozen=True)
class StaleJob:
    run: HubRun
    job: str
    mismatches: tuple[Mismatch, ...]
    record_expired: bool


def tested_ref_of(tested: TestedSet, repository: Repository) -> TestedRef | None:
    """What a job ran of `repository`; None when it did not read it (a job
    without the deploy key of the private hub, or with no peppy dev build)."""
    if repository.hub is None:
        return tested.peppy
    return tested.hubs.get(repository.name)


def mismatches_of(
    tested: TestedSet, state: SetState, under_test: Repository
) -> tuple[Mismatch, ...]:
    mismatches = []
    for member in state.members:
        if member.repository is under_test:
            continue
        recorded = tested_ref_of(tested, member.repository)
        if recorded is None:
            continue
        if recorded.commit != member.head:
            mismatches.append(Mismatch(member.repository, recorded, member.head))
    return tuple(mismatches)


def stale_jobs(state: SetState, runs: Sequence[HubRun]) -> list[StaleJob]:
    """The jobs of the runs of a hub pull request that did not run with the
    head of every branch of the set."""
    stale = []
    for run in runs:
        for record in run.records:
            if record.tested is None:
                stale.append(StaleJob(run, record.job, (), record_expired=True))
                continue
            mismatches = mismatches_of(record.tested, state, run.repository)
            if mismatches:
                stale.append(
                    StaleJob(run, record.job, mismatches, record_expired=False)
                )
    return stale


def latest_run_id(runs: Sequence[Mapping]) -> int | None:
    return max((run["id"] for run in runs), default=None)


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


@dataclass(frozen=True)
class Blocker:
    repository: Repository
    text: str
    # A missing approval, which an admin of every repository of the set
    # merges without.
    waived_for_admins: bool = False


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
        f"{mismatch.repository.name} at `{mismatch.tested.ref}` "
        f"`{short(mismatch.tested.commit)}`, and its branch of the set is now at "
        f"`{short(mismatch.current)}`"
        for mismatch in stale.mismatches
    )
    return f"{where} ran with {ran}: re-run it."


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


def pull_request_blockers(report: PullRequestReport) -> list[Blocker]:
    pull_request = report.pull_request
    repository = pull_request.repository
    label = pull_request.label
    blockers = []

    def block(text: str, waived_for_admins: bool = False) -> None:
        blockers.append(Blocker(repository, text, waived_for_admins))

    if pull_request.base_branch != repository.integration_branch:
        block(
            f"{label} targets `{pull_request.base_branch}`: retarget it onto "
            f"`{repository.integration_branch}`."
        )
    if pull_request.draft:
        block(f"{label} is a draft: mark it ready for review.")
    if report.mergeable is False:
        block(f"{label} has conflicts with `{pull_request.base_branch}`: resolve them.")
    if report.mergeable is None:
        block(
            f"GitHub has not yet computed whether {label} merges cleanly into "
            f"`{pull_request.base_branch}`."
        )
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
            waived_for_admins=not report.unbypassed_approval_rulesets,
        )
    for check, state in report.checks:
        if state is not CheckState.PASSED:
            block(
                f"The required check `{check.context}` of {label} {CHECK_STATE_TEXTS[state]}."
            )
    for stale in report.stale:
        block(stale_job_text(label, stale))
    return blockers


def member_blockers(
    set_name: str, member: Member, reports: Mapping[str, PullRequestReport]
) -> list[Blocker]:
    repository = member.repository
    if not member.open_pull_requests:
        return [
            Blocker(
                repository,
                f"{repository.name} has a change on the branch `{set_name}` "
                f"(`{short(member.head)}`) but no open pull request from it: "
                "open one, or delete the branch.",
            )
        ]
    if member.pull_request is None:
        labels = ", ".join(
            pull_request.label for pull_request in member.open_pull_requests
        )
        return [
            Blocker(
                repository,
                f"{repository.name} has more than one open pull request from "
                f"`{set_name}` ({labels}): close all of them but one.",
            )
        ]
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


def pull_request_cell(member: Member) -> str:
    if member.pull_request is not None:
        return f"[#{member.pull_request.number}]({member.pull_request.url})"
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
    state: SetState, reports: Mapping[str, PullRequestReport]
) -> str:
    """The dashboard of a set. Reports are keyed by pull request label."""
    blockers = set_blockers(state, reports)
    stale_run_count = len(stale_runs(reports))
    blocked = {blocker.repository for blocker in blockers_for(blockers, admin=True)}
    unapproved = {blocker.repository for blocker in blockers} - blocked
    lines = [
        DASHBOARD_MARKER,
        f"### Set `{state.name}`",
        "",
        f"These pull requests share the branch name `{state.name}`. The CI of "
        "each hub pull request tests it with the others, so they merge "
        "together: the `merge-set` check keeps the merge button of each one "
        "blocked. Tick the first box below to merge all of them.",
        "",
        "| Repository | Pull request | Head | CI | State |",
        "| --- | --- | --- | --- | --- |",
    ]
    lines.extend(
        f"| {member.repository.name} | {pull_request_cell(member)} "
        f"| `{short(member.head)}` | {ci_cell(member, reports)} "
        f"| {member_state(member.repository, blocked, unapproved)} |"
        for member in state.members
    )
    if state.left_behind:
        lines.append("")
        lines.extend(
            f"{repository.name} still has the branch `{state.name}` of a pull "
            "request that merged it, so it is not part of the set."
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
    return "\n".join(lines) + "\n"


def member_state(
    repository: Repository, blocked: set[Repository], unapproved: set[Repository]
) -> str:
    if repository in blocked:
        return "blocked"
    if repository in unapproved:
        return "not approved"
    return "ready"


ADMIN_WAIVER = (
    "An admin of every repository of the set merges it without the approvals."
)


def readiness_lines(blockers: Sequence[Blocker]) -> list[str]:
    """What the dashboard says of the merge: ready, ready for an admin alone,
    or what blocks it."""
    if not blockers:
        return [
            "**Ready to merge.** Each pull request is approved, its required "
            "checks passed, and each hub run ran with the head of every branch "
            "of the set."
        ]
    hard = blockers_for(blockers, admin=True)
    if not hard:
        return [
            "**Ready to merge for an admin.** Each required check passed and "
            "each hub run ran with the head of every branch of the set, but "
            f"approvals are missing. {ADMIN_WAIVER}",
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


def render_alone_dashboard(set_name: str) -> str:
    return (
        f"{DASHBOARD_MARKER}\n### Set `{set_name}`\n\n"
        f"No other repository has a change on the branch `{set_name}`, so this "
        "pull request is alone: its own merge button merges it.\n"
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


# The merge -------------------------------------------------------------------

MERGE_METHOD = "merge"
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
WRITE_PERMISSIONS = frozenset({"admin", "maintain", "write"})
ADMIN_PERMISSION = "admin"


@dataclass(frozen=True)
class MergedPullRequest:
    pull_request: PullRequest
    commit: str


@dataclass(frozen=True)
class MergeOutcome:
    merged: tuple[MergedPullRequest, ...]
    # The pull request whose merge failed, and GitHub's reason; None when
    # every pull request merged.
    failed: PullRequest | None
    failure: str | None
    not_tried: tuple[PullRequest, ...]

    @property
    def unmerged(self) -> tuple[PullRequest, ...]:
        failed = (self.failed,) if self.failed is not None else ()
        return failed + self.not_tried


def merge_commit_message(
    set_name: str, pull_request: PullRequest, pull_requests: Sequence[PullRequest]
) -> str:
    others = ", ".join(
        other.reference for other in pull_requests if other is not pull_request
    )
    return (
        f"{pull_request.title}\n\nMerged together with the set `{set_name}`: {others}."
    )


def render_merge_report(
    set_name: str,
    requesters: Sequence[str],
    outcome: MergeOutcome,
    unapproved: Sequence[PullRequest],
) -> str:
    """The report of a merge, on every pull request of the set. `unapproved`
    are the pull requests an admin merged without an approval."""
    merged = [
        f"- {merged.pull_request.label} merged as `{short(merged.commit)}`"
        + (" without an approval" if merged.pull_request in unapproved else "")
        + "."
        for merged in outcome.merged
    ]
    if outcome.failed is None:
        return (
            "\n".join(
                [
                    f"The set `{set_name}` merged, as {mention(requesters)} asked:",
                    "",
                    *merged,
                ]
            )
            + "\n"
        )
    lines = [
        f"The merge of the set `{set_name}`, which {mention(requesters)} asked "
        "for, stopped:",
        "",
        *merged,
        f"- {outcome.failed.label} did not merge: {outcome.failure}",
        *(
            f"- {pull_request.label} was not tried."
            for pull_request in outcome.not_tried
        ),
        "",
        "The pull requests that did not merge are blocked again. Remove the "
        "cause, then merge the rest: tick the box of their merge-set comment "
        "again, or, when one pull request is left alone, use its merge button.",
    ]
    return "\n".join(lines) + "\n"


def render_merged_dashboard(set_name: str, outcome: MergeOutcome) -> str:
    labels = ", ".join(merged.pull_request.label for merged in outcome.merged)
    text = f"{DASHBOARD_MARKER}\n### Set `{set_name}`\n\nMerged together: {labels}."
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


def render_access_refusal(
    action: Action, missing: Mapping[str, Sequence[Repository]]
) -> str:
    what = "merge the set" if action is Action.MERGE else "re-run the CI of the set"
    lines = [
        f"@{login}, the bot did not {what} for you: you have no write access to "
        f"{', '.join(repository.name for repository in repositories)}."
        for login, repositories in missing.items()
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
        lines.append(
            "Still running with an old commit, so not re-run (tick the box again "
            "when it ends):"
        )
        lines.extend(f"- {run_link(run)} of {run.repository.name}" for run in running)
    return "\n".join(lines) + "\n"


def render_nothing_to_rerun(requesters: Sequence[str], set_name: str) -> str:
    return (
        f"{mention(requesters)}, no CI of the set `{set_name}` is out of date: "
        "each hub run ran with the head of every branch of the set.\n"
    )


def render_rerun_waits_for_peppy(
    requesters: Sequence[str], peppy_pull_request: PullRequest, head: str
) -> str:
    return (
        f"{mention(requesters)}, the bot did not re-run the CI: the dev build of "
        f"peppy `{short(head)}` ({peppy_pull_request.label}) is not ready, and a "
        "hub run started now would fail on it. Tick the box again when the "
        f"Tests run of {peppy_pull_request.label} has uploaded it.\n"
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


def error_message(error: urllib.error.HTTPError) -> str:
    """The message GitHub puts in the body of a refused call, else the HTTP
    reason."""
    try:
        document = json.loads(error.read() or b"{}")
    except (json.JSONDecodeError, OSError):
        return str(error.reason)
    return document.get("message") or str(error.reason)


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


def pull_request_path(pull_request: PullRequest) -> str:
    return f"/repos/{pull_request.repository.full_name}/pulls/{pull_request.number}"


def comment_path(dashboard: Dashboard) -> str:
    repository = dashboard.pull_request.repository
    return f"/repos/{repository.full_name}/issues/comments/{dashboard.id}"


def issue_comments_path(pull_request: PullRequest) -> str:
    return f"/repos/{pull_request.repository.full_name}/issues/{pull_request.number}/comments"


class GitHubGateway:
    """Everything the sync reads from and writes to GitHub."""

    def __init__(self, api: GitHubApi, bot_login: str):
        self.api = api
        self.bot_login = bot_login
        self.rules: dict[str, BranchRules] = {}
        self.bypasses: dict[tuple[str, int], RulesetBypass] = {}
        self.permissions: dict[tuple[str, str], str] = {}
        self.ci_workflows: dict[str, frozenset[int]] = {}

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
        item = self.api.request("GET", f"/repos/{repository.full_name}/pulls/{number}")
        return parse_pull_request(repository, item)

    def merge_state(self, pull_request: PullRequest) -> tuple[bool | None, str]:
        """Whether the pull request merges without conflicts (None while GitHub
        computes it), and its `mergeable_state`."""
        item = self.api.request("GET", pull_request_path(pull_request))
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

    def is_behind(self, pull_request: PullRequest) -> bool:
        comparison = self.api.request(
            "GET",
            f"/repos/{pull_request.repository.full_name}/compare/"
            f"{quoted(pull_request.base_branch)}...{pull_request.head_commit}",
        )
        return comparison["behind_by"] > 0

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

    def peppy_dev_build_ready(self, commit: str) -> bool:
        """Whether the peppy CI run of `commit` uploaded its dev build, which
        a hub run of a set with a peppy branch installs."""
        runs = self.api.request(
            "GET",
            f"/repos/{resolve.PEPPY_REPOSITORY}/actions/workflows/"
            f"{resolve.PEPPY_CI_WORKFLOW}/runs",
            {"head_sha": commit, "per_page": 100},
        )
        run_id = latest_run_id(runs["workflow_runs"])
        if run_id is None:
            return False
        artifacts = resolve.run_artifacts(
            run_id, resolve.DEV_BUILD_ARTIFACT, resolve.GitHubReader(self.api.token)
        )
        return resolve.newest_usable(artifacts) is not None

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
        for comment in self.api.pages(issue_comments_path(pull_request)):
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
            "POST", issue_comments_path(pull_request), body={"body": body}
        )
        return comment["html_url"]

    def update_comment(self, dashboard: Dashboard, body: str) -> None:
        self.api.request("PATCH", comment_path(dashboard), body={"body": body})

    # The actions.

    def permission(self, repository: Repository, login: str) -> str:
        key = (repository.name, login)
        if key not in self.permissions:
            try:
                response = self.api.request(
                    "GET",
                    f"/repos/{repository.full_name}/collaborators/{login}/permission",
                )
                self.permissions[key] = response["permission"]
            except ApiError as error:
                if error.status != 404:
                    raise
                self.permissions[key] = "none"
        return self.permissions[key]

    def merge(self, pull_request: PullRequest, message: str) -> str:
        """Merge the pull request at its head commit, and return the merge
        commit. A call that fails is checked against the pull request: GitHub
        can merge it and still fail the response."""
        path = pull_request_path(pull_request)
        try:
            response = self.api.request(
                "PUT",
                f"{path}/merge",
                body={
                    "merge_method": MERGE_METHOD,
                    "sha": pull_request.head_commit,
                    "commit_message": message,
                },
            )
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

    @staticmethod
    def sleep(seconds: float) -> None:
        time.sleep(seconds)


# The sync --------------------------------------------------------------------


@dataclass(frozen=True)
class RunContext:
    bot_login: str
    # The URL of this run of the Merge set workflow.
    run_url: str


def read_set(gateway: GitHubGateway, name: str) -> SetState:
    heads = {
        repository: gateway.branch_head(repository, name) for repository in REPOSITORIES
    }
    pull_requests = {
        repository: gateway.pull_requests_from(repository, name)
        if heads[repository]
        else []
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
            tuple(stale_jobs(state, gateway.hub_runs(pull_request)))
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
    dashboards = {}
    for pull_request in state.open_pull_requests():
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


def missing_write_access(
    gateway: GitHubGateway, state: SetState, login: str
) -> list[Repository]:
    return [
        member.repository
        for member in state.members
        if gateway.permission(member.repository, login) not in WRITE_PERMISSIONS
    ]


def is_admin_of_the_set(gateway: GitHubGateway, state: SetState, login: str) -> bool:
    return all(
        gateway.permission(member.repository, login) == ADMIN_PERMISSION
        for member in state.members
    )


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
        message = merge_commit_message(set_name, pull_request, pull_requests)
        try:
            commit = gateway.merge(pull_request, message)
        except MergeRefused as refusal:
            return MergeOutcome(
                merged=tuple(merged),
                failed=pull_request,
                failure=str(refusal),
                not_tried=tuple(pull_requests[index + 1 :]),
            )
        merged.append(MergedPullRequest(pull_request, commit))
    return MergeOutcome(tuple(merged), None, None, ())


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
    context: RunContext,
) -> None:
    asked_on = request.dashboard.pull_request
    admin = is_admin_of_the_set(gateway, state, request.requester)
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
            pull_request, merging_gate(state.name, request.requester, context.run_url)
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
    for pull_request in pull_requests:
        gateway.create_comment(pull_request, report)
    for merged in outcome.merged:
        dashboard = dashboards.get(merged.pull_request.label)
        if dashboard is not None:
            write_dashboard(
                gateway,
                merged.pull_request,
                dashboard,
                render_merged_dashboard(state.name, outcome),
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
    peppy = state.member_of(PEPPY)
    if (
        peppy is not None
        and peppy.pull_request is not None
        and not gateway.peppy_dev_build_ready(peppy.head)
    ):
        gateway.create_comment(
            asked_on,
            render_rerun_waits_for_peppy(requesters, peppy.pull_request, peppy.head),
        )
        return
    rerun = [run for run in runs if run.completed]
    running = [run for run in runs if not run.completed]
    for run in rerun:
        gateway.rerun(run)
    gateway.create_comment(
        asked_on, render_rerun_report(requesters, state.name, rerun, running)
    )


def handle_requests(
    gateway: GitHubGateway,
    state: SetState,
    dashboards: Mapping[str, Dashboard],
    requests: Sequence[Request],
    context: RunContext,
) -> None:
    """Act on each action once, for the first user who asked for it with
    write access to every repository of the set. The re-run goes first: a
    merge asked for at the same time then finds the re-run CI running and
    stops."""
    reports = read_reports(gateway, state)
    for action in (Action.RERUN, Action.MERGE):
        asked = [request for request in requests if request.action is action]
        if not asked:
            continue
        requesters = requesters_of(asked)
        missing = {
            login: missing_write_access(gateway, state, login) for login in requesters
        }
        allowed = next(
            (request for request in asked if not missing[request.requester]), None
        )
        if allowed is None:
            gateway.create_comment(
                asked[0].dashboard.pull_request, render_access_refusal(action, missing)
            )
            continue
        if action is Action.RERUN:
            rerun_out_of_date(gateway, state, reports, allowed, requesters)
        else:
            merge_the_set(
                gateway, state, reports, dashboards, allowed, requesters, context
            )


def publish(
    gateway: GitHubGateway, state: SetState, dashboards: Mapping[str, Dashboard]
) -> None:
    """Write the dashboard and the gate of each open pull request of the set."""
    if state.is_alone:
        for pull_request in state.open_pull_requests():
            set_gate(gateway, pull_request, alone_gate(state.name))
            dashboard = dashboards.get(pull_request.label)
            if dashboard is not None:
                write_dashboard(
                    gateway, pull_request, dashboard, render_alone_dashboard(state.name)
                )
        return
    body = render_set_dashboard(state, read_reports(gateway, state))
    for pull_request in state.open_pull_requests():
        url = write_dashboard(
            gateway, pull_request, dashboards.get(pull_request.label), body
        )
        set_gate(gateway, pull_request, member_gate(state.name, url))


def no_set_gate(pull_request: PullRequest) -> Gate:
    if pull_request.from_fork:
        return Gate("success", "From a fork, so not part of a set", None)
    return Gate(
        "success",
        status_description(f"From {pull_request.head_branch}, so not part of a set"),
        None,
    )


def sync(gateway: GitHubGateway, event: Event, context: RunContext) -> None:
    set_name = event.set_name
    if set_name is None:
        if event.pull_request is None:
            return
        pull_request = gateway.pull_request(event.repository, event.pull_request)
        if pull_request.is_open:
            set_gate(gateway, pull_request, no_set_gate(pull_request))
        return
    state = read_set(gateway, set_name)
    # The bodies as this run first read them: a dashboard someone edits
    # during the run is left for the sync that edit starts.
    dashboards = read_dashboards(gateway, state)
    requests = read_requests(gateway, dashboards) if not state.is_alone else []
    if requests:
        handle_requests(gateway, state, dashboards, requests, context)
        state = read_set(gateway, set_name)
    publish(gateway, state, dashboards)


# The relay -------------------------------------------------------------------

# The workflow of peppy the relay starts, and the branch it runs from, whose
# `merge-set` environment alone holds the key of the App.
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


def subject_of_pull_request(item: Mapping) -> RelaySubject:
    head = item["head"]
    return RelaySubject(
        pull_request=item["number"],
        branch=head["ref"],
        head_repository=(head.get("repo") or {}).get("full_name"),
    )


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


def render_request_received(ticked: TickedBoxes, set_name: str, run_url: str) -> str:
    requests = " and to ".join(
        request_text(action, set_name) for action in ticked.actions
    )
    return (
        f"@{ticked.requester}, the merge-set bot got your request to {requests}. "
        f"[This run]({run_url}) acts on it, and the bot reports the result here.\n"
    )


def dispatched_run_url(response: object) -> str:
    """The URL of the run that a dispatch with `return_run_details` started."""
    url = response.get("html_url") if isinstance(response, Mapping) else None
    if not isinstance(url, str):
        raise MergeSetError(
            f"the dispatch of {SYNC_WORKFLOW} names no run: {json.dumps(response)}"
        )
    return url


def start_sync(api: GitHubApi, inputs: Mapping[str, str]) -> str:
    """Start the Merge set workflow of peppy, and return the URL of its run."""
    response = api.request(
        "POST",
        f"/repos/{resolve.PEPPY_REPOSITORY}/actions/workflows/{SYNC_WORKFLOW}/dispatches",
        body={"ref": SYNC_WORKFLOW_REF, "inputs": inputs, "return_run_details": True},
    )
    return dispatched_run_url(response)


def run_relay(environment: Mapping[str, str]) -> None:
    repository = repository_of_full_name(required(environment, "GITHUB_REPOSITORY"))
    event_name = required(environment, "GITHUB_EVENT_NAME")
    payload = resolve.read_event_payload(required(environment, "GITHUB_EVENT_PATH"))
    subject = relay_subject(event_name, payload)
    if subject is None:
        print(f"The `{event_name}` event cannot change a set.")
        return
    if subject.branch is None:
        reader = GitHubApi(required(environment, "PULL_REQUEST_READ_TOKEN"))
        subject = subject_of_pull_request(
            reader.request(
                "GET", f"/repos/{repository.full_name}/pulls/{subject.pull_request}"
            )
        )
    inputs = dispatch_inputs(repository, subject)
    run_url = start_sync(GitHubApi(required(environment, "MERGE_SET_TOKEN")), inputs)
    print(f"Started {SYNC_WORKFLOW} of peppy for {json.dumps(inputs)}: {run_url}")
    if event_name != "issue_comment":
        return
    ticked = ticked_boxes_of_edit(payload)
    if not ticked.actions:
        return
    GitHubApi(required(environment, "COMMENT_TOKEN")).request(
        "POST",
        f"/repos/{repository.full_name}/issues/{subject.pull_request}/comments",
        body={"body": render_request_received(ticked, subject.branch, run_url)},
    )
    print(f"Answered the request of @{ticked.requester} on #{subject.pull_request}")


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
    context = RunContext(
        bot_login=required(environment, "MERGE_SET_BOT_LOGIN"),
        run_url=(
            f"{required(environment, 'GITHUB_SERVER_URL')}/"
            f"{required(environment, 'GITHUB_REPOSITORY')}/actions/runs/"
            f"{required(environment, 'GITHUB_RUN_ID')}"
        ),
    )
    gateway = GitHubGateway(
        GitHubApi(required(environment, "MERGE_SET_TOKEN")), context.bot_login
    )
    sync(gateway, event, context)


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
        "relay", help="Start the Merge set workflow of peppy for the event of this run."
    )
    commands.add_parser(
        "sync",
        help="Read the set of the event, act on its ticked boxes, and publish it.",
    )
    return parser.parse_args(argv)


def main(argv: Sequence[str]) -> int:
    arguments = parse_arguments(argv)
    try:
        match arguments.command:
            case "repositories":
                run_repositories()
            case "relay":
                run_relay(os.environ)
            case "sync":
                run_sync(os.environ)
    except (MergeSetError, resolve.ResolveError) as error:
        print(f"::error::{resolve.escape_workflow_command(str(error))}")
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
