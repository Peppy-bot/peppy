#!/usr/bin/env python3
"""The pr-title check: the title of a pull request names the Plane work items
that the pull request implements, and each of them exists in Plane.

The pr-title workflow of each repository of the Peppy-bot organization runs
this script through the pr-title action, on each `pull_request_target` event
of a pull request. The script reads the event that GitHub gives the job and
asks Plane about the items. It never reads the code of the pull request.

A title names its items in brackets. The Plane GitHub App links the pull
request to each item that its title names in brackets, and moves that item
through the states of the pull request. An item named without brackets is a
reference only. The `title-form` input of the action selects one of two forms:

- `plain`: the items, then the subject.
  `[FRAMEWORK-123] Restart an instance under its policy`, and with several
  items `[FRAMEWORK-123] [SIMULATION-45] Restart an instance under its policy`.
- `conventional-commit`: a conventional commit, then the items.
  `feat(router): restart a stopped router [PLATFORM-123]`, and with several
  items `feat(router): restart a stopped router [PLATFORM-123] [INFRAOPS-45]`.

An item exists when Plane shows it to the account of the API key (the
PLANE_API_KEY secret): its project is a project of the workspace that the
account is a member of, and that project has a work item with its number.

A pull request needs no item, and the check passes at once, when a bot opened
it (Dependabot, or a GitHub App such as the release bot of peppy), or when it
goes into a branch other than the default branch of its repository (the
release pull requests of peppy from `dev` into `main`).

The decisions are functions of their inputs, tested in test_pr_title.py: the
HTTP requests to Plane and the wait between two attempts are parameters of
Plane. Standard library only: the check runs on the runner's python3.
"""

from __future__ import annotations

import enum
import json
import os
import re
import sys
import time
import urllib.error
import urllib.parse
import urllib.request
from collections.abc import Callable, Mapping, Sequence
from dataclasses import dataclass
from pathlib import Path

PLANE_API_URL = "https://api.plane.so/api/v1"
PLANE_WORKSPACE = "peppy"
# The most projects Plane gives in one page.
PLANE_PROJECTS_PAGE_SIZE = 100
# A request that Plane answers with its rate limit or a server error, or does
# not answer, goes again, up to this many attempts in total.
PLANE_ATTEMPTS = 3
PLANE_TIMEOUT_SECONDS = 20
# The wait before the next attempt when Plane does not say how long to wait,
# and the longest wait the check accepts from Plane's Retry-After.
DEFAULT_RETRY_WAIT_SECONDS = 5
MAX_RETRY_WAIT_SECONDS = 60

CONVENTIONAL_COMMIT_TYPES = (
    "build",
    "chore",
    "ci",
    "docs",
    "feat",
    "fix",
    "perf",
    "refactor",
    "revert",
    "style",
    "test",
)

# One item in brackets: the identifier of a Plane project, a hyphen, and the
# number of the item in that project, which Plane starts at 1.
ITEM_IN_BRACKETS = re.compile(r"\[([A-Z][A-Z0-9]*)-([1-9][0-9]*)\]")
# What looks like an item outside brackets of its own, as `(PLATFORM-3)` or
# `[feat/PLATFORM-3]` names it.
ITEM_WITHOUT_BRACKETS = re.compile(r"(?<![\w-])[A-Z][A-Z0-9]*-[1-9][0-9]*(?![\w-])")
_ITEMS = rf"{ITEM_IN_BRACKETS.pattern}(?: {ITEM_IN_BRACKETS.pattern})*"
PLAIN_TITLE = re.compile(rf"(?P<items>{_ITEMS}) (?P<subject>\S.*)")
CONVENTIONAL_COMMIT_TITLE = re.compile(
    rf"(?:{'|'.join(CONVENTIONAL_COMMIT_TYPES)})(?:\([^()\s][^()]*\))?!?: "
    rf"(?P<subject>\S.*?) (?P<items>{_ITEMS})"
)


class TitleForm(enum.Enum):
    PLAIN = "plain"
    CONVENTIONAL_COMMIT = "conventional-commit"


@dataclass(frozen=True)
class FormRule:
    pattern: re.Pattern[str]
    # The form as the messages of the check give it.
    text: str
    # Where the items go in a title of the form.
    item_place: str


FORM_RULES = {
    TitleForm.PLAIN: FormRule(
        pattern=PLAIN_TITLE,
        text=(
            "`[<ITEM>] <subject>`, for example `[FRAMEWORK-123] Restart an "
            "instance under its policy`, and with several items `[FRAMEWORK-123] "
            "[SIMULATION-45] <subject>`"
        ),
        item_place="before the subject",
    ),
    TitleForm.CONVENTIONAL_COMMIT: FormRule(
        pattern=CONVENTIONAL_COMMIT_TITLE,
        text=(
            "`<type>(<scope>): <subject> [<ITEM>]`, for example `feat(router): "
            "restart a stopped router [PLATFORM-123]`, and with several items "
            "`<type>(<scope>): <subject> [PLATFORM-123] [INFRAOPS-45]`, where the "
            f"type is one of {', '.join(CONVENTIONAL_COMMIT_TYPES)}"
        ),
        item_place="after the subject",
    ),
}


class TitleError(Exception):
    """The title does not name its items in the form of the repository."""


class EventError(Exception):
    """The event that GitHub gave the job is not one the check can read."""


class PlaneError(Exception):
    """Plane gave no answer that the check can use."""


@dataclass(frozen=True, order=True)
class PlaneItemId:
    """The identifier of a Plane work item, as `FRAMEWORK-123`."""

    project: str
    number: int

    def __str__(self) -> str:
        return f"{self.project}-{self.number}"


def parse_title_form(text: str) -> TitleForm:
    try:
        return TitleForm(text)
    except ValueError:
        forms = " or ".join(f"`{form.value}`" for form in TitleForm)
        raise EventError(
            f"The title-form input of the pr-title action is `{text}`: it must be "
            f"{forms}."
        ) from None


def parse_title(title: str, form: TitleForm) -> tuple[PlaneItemId, ...]:
    """The items that a title names, in their order in the title."""
    rule = FORM_RULES[form]
    match = rule.pattern.fullmatch(title)
    if match is None:
        raise TitleError(not_in_form_text(title, rule))
    if not ITEM_IN_BRACKETS.sub("", match["subject"]).strip():
        raise TitleError(
            f"The title names items and no subject: it must be {rule.text}. "
            f"Got: {title}"
        )
    item_in_subject = ITEM_IN_BRACKETS.search(match["subject"])
    if item_in_subject is not None:
        raise TitleError(
            f"The title names {item_in_subject[0]} in its subject: name every item "
            f"{rule.item_place}, as {rule.text}. Got: {title}"
        )
    items = tuple(
        PlaneItemId(project, int(number))
        for project, number in ITEM_IN_BRACKETS.findall(match["items"])
    )
    repeated = sorted({item for item in items if items.count(item) > 1})
    if repeated:
        names = ", ".join(str(item) for item in repeated)
        raise TitleError(f"The title names {names} more than once. Got: {title}")
    return items


def not_in_form_text(title: str, rule: FormRule) -> str:
    return f"The title must be {rule.text}.{unbracketed_item_hint(title)} Got: {title}"


def unbracketed_item_hint(title: str) -> str:
    """A hint for a title that names no item in brackets, but holds what looks
    like an item outside brackets of its own; empty for any other title."""
    if ITEM_IN_BRACKETS.search(title) is not None:
        return ""
    unbracketed = ITEM_WITHOUT_BRACKETS.search(title)
    if unbracketed is None:
        return ""
    return (
        f" If {unbracketed[0]} is a Plane item, name it alone in brackets, as "
        f"[{unbracketed[0]}]: Plane links an item without brackets as a reference "
        "only, and does not move it through the states of the pull request."
    )


@dataclass(frozen=True)
class PullRequest:
    """What the check reads of the pull request of a `pull_request_target`
    event."""

    title: str
    author: str
    opened_by_bot: bool
    base_branch: str
    default_branch: str


def parse_pull_request(event: Mapping) -> PullRequest:
    try:
        pull_request = event["pull_request"]
        return PullRequest(
            title=pull_request["title"],
            author=pull_request["user"]["login"],
            opened_by_bot=pull_request["user"]["type"] == "Bot",
            base_branch=pull_request["base"]["ref"],
            default_branch=event["repository"]["default_branch"],
        )
    except (KeyError, TypeError) as error:
        raise EventError(
            "The event of the job is not the event of a pull request: it lacks "
            f"{error}."
        ) from error


def reason_no_item_is_needed(pull_request: PullRequest) -> str | None:
    """Why the pull request needs no Plane item, or None when it needs one."""
    if pull_request.opened_by_bot:
        return (
            f"{pull_request.author} opened this pull request. A bot has no Plane "
            "item to name, so the title can name none."
        )
    if pull_request.base_branch != pull_request.default_branch:
        return (
            f"This pull request goes into `{pull_request.base_branch}`, not into the "
            f"default branch `{pull_request.default_branch}`, so the title can name "
            "no Plane item."
        )
    return None


# Plane -----------------------------------------------------------------------


@dataclass(frozen=True)
class HttpResponse:
    status: int
    body: bytes
    retry_after: str | None


Send = Callable[[urllib.request.Request], HttpResponse]


def send_request(request: urllib.request.Request) -> HttpResponse:
    """Send a request and give the answer, an HTTP error status included. A
    request that gets no answer raises an OSError."""
    try:
        with urllib.request.urlopen(request, timeout=PLANE_TIMEOUT_SECONDS) as response:
            return HttpResponse(
                response.status, response.read(), response.headers.get("Retry-After")
            )
    except urllib.error.HTTPError as error:
        return HttpResponse(error.code, error.read(), error.headers.get("Retry-After"))


def is_transient(status: int) -> bool:
    """Whether Plane can answer the same request in another way soon: its rate
    limit, or an error of its servers."""
    return status == 429 or 500 <= status <= 599


def retry_wait_seconds(retry_after: str | None) -> float:
    """The wait before the next attempt. Plane's Retry-After gives a number of
    seconds; a missing or other value gives the default wait."""
    if retry_after is None or not retry_after.strip().isdecimal():
        return DEFAULT_RETRY_WAIT_SECONDS
    return min(int(retry_after), MAX_RETRY_WAIT_SECONDS)


@dataclass(frozen=True)
class PlaneProject:
    identifier: str
    name: str
    # Whether the account of the API key is a member of the project: Plane
    # shows the items of a project to its members alone.
    is_member: bool


@dataclass(frozen=True)
class WorkItem:
    item: PlaneItemId
    name: str


class Plane:
    """The read-only questions of the check to the Plane API, as the account of
    one API key."""

    def __init__(
        self,
        api_key: str,
        send: Send = send_request,
        sleep: Callable[[float], None] = time.sleep,
    ):
        self.api_key = api_key
        self.send = send
        self.sleep = sleep

    def projects(self) -> dict[str, PlaneProject]:
        """The projects of the workspace, by identifier."""
        projects: dict[str, PlaneProject] = {}
        cursor = None
        while True:
            query = {"per_page": str(PLANE_PROJECTS_PAGE_SIZE)}
            if cursor is not None:
                query["cursor"] = cursor
            page = self.get_json(
                f"workspaces/{PLANE_WORKSPACE}/projects/?{urllib.parse.urlencode(query)}",
                "the list of the projects",
            )
            try:
                for result in page["results"]:
                    project = PlaneProject(
                        identifier=result["identifier"],
                        name=result["name"],
                        is_member=result["is_member"] is True,
                    )
                    projects[project.identifier] = project
                if not page["next_page_results"]:
                    return projects
                next_cursor = page["next_cursor"]
            except (KeyError, TypeError) as error:
                raise PlaneError(
                    f"Plane gave a list of the projects without {error}."
                ) from error
            if next_cursor == cursor:
                raise PlaneError(
                    f"Plane gave the page {cursor} of the list of the projects twice."
                )
            cursor = next_cursor

    def work_item(self, item: PlaneItemId) -> WorkItem | None:
        """The work item, or None when its project has no item with its
        number."""
        response = self.get(f"workspaces/{PLANE_WORKSPACE}/work-items/{item}/")
        if response.status == 404:
            return None
        data = self.json_of(response, f"the lookup of {item}")
        try:
            name = data["name"]
            number = data["sequence_id"]
        except (KeyError, TypeError) as error:
            raise PlaneError(f"Plane gave {item} without {error}.") from error
        if number != item.number:
            raise PlaneError(f"Plane gave the item number {number} for {item}.")
        return WorkItem(item, name)

    def get_json(self, path: str, what: str):
        return self.json_of(self.get(path), what)

    def json_of(self, response: HttpResponse, what: str):
        if response.status in (401, 403):
            raise PlaneError(
                f"Plane refused {what} (HTTP {response.status}): it does not accept "
                "the API key of the PLANE_API_KEY secret. Put a valid key of an "
                "account of the peppy workspace in the secret."
            )
        if response.status != 200:
            raise PlaneError(
                f"Plane answered {what} with HTTP {response.status}: "
                f"{response.body[:200].decode('utf-8', 'replace')}"
            )
        try:
            return json.loads(response.body)
        except ValueError as error:
            raise PlaneError(f"Plane answered {what} with no JSON: {error}") from error

    def get(self, path: str) -> HttpResponse:
        """The answer of Plane to a GET, after up to PLANE_ATTEMPTS attempts
        when Plane is at its rate limit, fails or does not answer."""
        if not self.api_key:
            raise PlaneError(
                "The PLANE_API_KEY secret is empty or not set, so the check cannot "
                "ask Plane about the items. Give the repository the organization "
                "secret PLANE_API_KEY."
            )
        request = urllib.request.Request(
            f"{PLANE_API_URL}/{path}",
            headers={"X-Api-Key": self.api_key, "Accept": "application/json"},
        )
        for attempt in range(1, PLANE_ATTEMPTS + 1):
            try:
                response = self.send(request)
            except OSError as error:
                failure = f"no answer ({error})"
                wait = DEFAULT_RETRY_WAIT_SECONDS
            else:
                if not is_transient(response.status):
                    return response
                failure = f"HTTP {response.status}"
                wait = retry_wait_seconds(response.retry_after)
            if attempt < PLANE_ATTEMPTS:
                self.sleep(wait)
        raise PlaneError(
            f"Plane gave {failure} to GET {path}, {PLANE_ATTEMPTS} times. Re-run the "
            "check when Plane answers again."
        )


# The check -------------------------------------------------------------------


@dataclass(frozen=True)
class Outcome:
    """What the check found: it passes when it found no problem."""

    problems: tuple[str, ...]
    notes: tuple[str, ...]

    @property
    def passed(self) -> bool:
        return not self.problems


def find_work_items(
    items: Sequence[PlaneItemId], plane: Plane
) -> tuple[list[WorkItem], list[str]]:
    """The items that Plane shows, and a problem for each other one."""
    projects = plane.projects()
    found: list[WorkItem] = []
    problems: list[str] = []
    for item in items:
        project = projects.get(item.project)
        if project is None:
            problems.append(
                f"{item}: the peppy workspace of Plane has no project {item.project}. "
                f"{member_projects_text(projects)}"
            )
            continue
        if not project.is_member:
            problems.append(
                f"{item}: the account of the PLANE_API_KEY secret is not a member of "
                f"the Plane project {project.identifier} ({project.name}), so Plane "
                "does not show it the items of that project. Add the account to the "
                "project."
            )
            continue
        work_item = plane.work_item(item)
        if work_item is None:
            problems.append(
                f"{item}: the Plane project {project.identifier} has no item "
                f"{item.number}."
            )
            continue
        found.append(work_item)
    return found, problems


def member_projects_text(projects: Mapping[str, PlaneProject]) -> str:
    """The projects whose items a title can name."""
    identifiers = sorted(
        identifier for identifier, project in projects.items() if project.is_member
    )
    if not identifiers:
        return "The account of the PLANE_API_KEY secret is a member of no project."
    return f"A title can name the items of {', '.join(identifiers)}."


def check(pull_request: PullRequest, form: TitleForm, plane: Plane) -> Outcome:
    reason = reason_no_item_is_needed(pull_request)
    if reason is not None:
        return Outcome(problems=(), notes=(reason,))
    try:
        items = parse_title(pull_request.title, form)
        found, problems = find_work_items(items, plane)
    except (TitleError, PlaneError) as error:
        return Outcome(problems=(str(error),), notes=())
    notes = tuple(f"{work_item.item}: {work_item.name}" for work_item in found)
    return Outcome(problems=tuple(problems), notes=notes)


# The output ------------------------------------------------------------------


def workflow_command_text(text: str) -> str:
    """Text that a workflow command carries as data: GitHub reads `%`, a
    carriage return and a line feed in it as part of the command."""
    return text.replace("%", "%25").replace("\r", "%0D").replace("\n", "%0A")


def log_lines(outcome: Outcome) -> list[str]:
    """The lines of the job log. Each problem is an error annotation, which the
    checks of the pull request show."""
    return [
        *(
            f"::error title=pr-title::{workflow_command_text(problem)}"
            for problem in outcome.problems
        ),
        *outcome.notes,
    ]


def summary_text(outcome: Outcome) -> str:
    """The job summary: the items the title names, or what is wrong."""
    if outcome.passed:
        heading = "### pr-title passed"
        lines = outcome.notes
    else:
        heading = "### pr-title failed"
        lines = outcome.problems
    return "\n".join([heading, "", *(f"- {line}" for line in lines), ""])


def read_event(environment: Mapping[str, str]) -> Mapping:
    """The event that GitHub gave the job, from the file GITHUB_EVENT_PATH
    names."""
    path = environment.get("GITHUB_EVENT_PATH")
    if not path:
        raise EventError("GITHUB_EVENT_PATH is not set.")
    try:
        return json.loads(Path(path).read_text(encoding="utf-8"))
    except (OSError, ValueError) as error:
        raise EventError(f"The event file {path} cannot be read: {error}") from error


def main(environment: Mapping[str, str]) -> int:
    try:
        form = parse_title_form(environment.get("TITLE_FORM", ""))
        pull_request = parse_pull_request(read_event(environment))
    except EventError as error:
        outcome = Outcome(problems=(str(error),), notes=())
    else:
        plane = Plane(environment.get("PLANE_API_KEY", ""))
        outcome = check(pull_request, form, plane)
    print("\n".join(log_lines(outcome)))
    summary_path = environment.get("GITHUB_STEP_SUMMARY")
    if summary_path:
        with open(summary_path, "a", encoding="utf-8") as summary:
            summary.write(summary_text(outcome))
    return 0 if outcome.passed else 1


if __name__ == "__main__":
    sys.exit(main(os.environ))
