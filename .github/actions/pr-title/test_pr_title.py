"""The titles that `pr_title.py` accepts, and what it asks Plane.

The cases give the check the events that GitHub sends and the answers that the
Plane API gives, through a fake Plane, so they never reach the network and
never wait: the wait between two attempts is a recorded parameter.
"""

import contextlib
import io
import json
import re
import tempfile
import unittest
import urllib.parse
from pathlib import Path

import pr_title
from pr_title import (
    EventError,
    HttpResponse,
    Plane,
    PlaneError,
    PlaneItemId,
    PullRequest,
    TitleError,
    TitleForm,
)

ACTION = Path(__file__).resolve().parent
REPOSITORY = ACTION.parents[2]
PEPPY_WORKFLOW = REPOSITORY / ".github" / "workflows" / "pr-title.yml"

PLAIN = TitleForm.PLAIN
CONVENTIONAL = TitleForm.CONVENTIONAL_COMMIT
FRAMEWORK_123 = PlaneItemId("FRAMEWORK", 123)
SIMULATION_45 = PlaneItemId("SIMULATION", 45)
PLATFORM_3 = PlaneItemId("PLATFORM", 3)
INFRAOPS_12 = PlaneItemId("INFRAOPS", 12)
API_KEY = "plane_api_key"


class PlainTitles(unittest.TestCase):
    def test_one_item_before_the_subject(self):
        self.assertEqual(
            pr_title.parse_title("[FRAMEWORK-123] Restart an instance", PLAIN),
            (FRAMEWORK_123,),
        )

    def test_several_items_in_their_order(self):
        self.assertEqual(
            pr_title.parse_title(
                "[SIMULATION-45] [FRAMEWORK-123] Restart an instance", PLAIN
            ),
            (SIMULATION_45, FRAMEWORK_123),
        )

    def test_a_subject_keeps_brackets_that_name_no_item(self):
        self.assertEqual(
            pr_title.parse_title("[FRAMEWORK-123] Read [optional] fields", PLAIN),
            (FRAMEWORK_123,),
        )

    def test_a_project_identifier_can_hold_digits(self):
        self.assertEqual(
            pr_title.parse_title("[R2D2-7] Calibrate", PLAIN),
            (PlaneItemId("R2D2", 7),),
        )

    def test_a_title_not_in_the_plain_form_is_refused(self):
        for case, title in (
            ("no item", "Restart an instance"),
            ("the item after the subject", "Restart an instance [FRAMEWORK-123]"),
            ("the branch name as the item", "[feat/FRAMEWORK-137] Restart"),
            ("parentheses", "(FRAMEWORK-123) Restart an instance"),
            ("a lower case project", "[framework-123] Restart an instance"),
            ("item number 0", "[FRAMEWORK-0] Restart an instance"),
            ("a leading zero", "[FRAMEWORK-0123] Restart an instance"),
            ("no number", "[FRAMEWORK] Restart an instance"),
            (
                "two items in one pair of brackets",
                "[FRAMEWORK-1, SIMULATION-2] Restart",
            ),
            ("items without a space between", "[FRAMEWORK-1][SIMULATION-2] Restart"),
            ("two spaces before the subject", "[FRAMEWORK-123]  Restart"),
            ("no space before the subject", "[FRAMEWORK-123]Restart"),
            ("an item alone", "[FRAMEWORK-123]"),
            ("an item and a space", "[FRAMEWORK-123] "),
            ("a conventional commit", "feat: restart an instance [FRAMEWORK-123]"),
        ):
            with self.subTest(case=case):
                with self.assertRaisesRegex(TitleError, r"\[FRAMEWORK-123\] Restart"):
                    pr_title.parse_title(title, PLAIN)

    def test_items_with_no_subject_are_refused(self):
        with self.assertRaisesRegex(TitleError, "no subject"):
            pr_title.parse_title("[FRAMEWORK-123] [SIMULATION-45]", PLAIN)

    def test_an_item_in_the_subject_is_refused(self):
        with self.assertRaisesRegex(
            TitleError, r"names \[SIMULATION-45\] in its subject.*before the subject"
        ):
            pr_title.parse_title(
                "[FRAMEWORK-123] Restart what [SIMULATION-45] stops", PLAIN
            )

    def test_an_item_named_twice_is_refused(self):
        with self.assertRaisesRegex(TitleError, "FRAMEWORK-123 more than once"):
            pr_title.parse_title("[FRAMEWORK-123] [FRAMEWORK-123] Restart", PLAIN)

    def test_the_refusal_quotes_the_title(self):
        with self.assertRaisesRegex(TitleError, "Got: Restart an instance$"):
            pr_title.parse_title("Restart an instance", PLAIN)


class ConventionalCommitTitles(unittest.TestCase):
    def test_one_item_after_the_subject(self):
        for title in (
            "feat(router): restart a stopped router [PLATFORM-3]",
            "feat: restart a stopped router [PLATFORM-3]",
            "feat(router)!: restart a stopped router [PLATFORM-3]",
            "fix(aws/ecr): expire sha builds beyond 50 [PLATFORM-3]",
        ):
            with self.subTest(title=title):
                self.assertEqual(
                    pr_title.parse_title(title, CONVENTIONAL), (PLATFORM_3,)
                )

    def test_every_conventional_commit_type(self):
        for commit_type in pr_title.CONVENTIONAL_COMMIT_TYPES:
            with self.subTest(type=commit_type):
                self.assertEqual(
                    pr_title.parse_title(
                        f"{commit_type}: a change [PLATFORM-3]", CONVENTIONAL
                    ),
                    (PLATFORM_3,),
                )

    def test_several_items_in_their_order(self):
        self.assertEqual(
            pr_title.parse_title(
                "feat(router): add the router edge [INFRAOPS-12] [PLATFORM-3]",
                CONVENTIONAL,
            ),
            (INFRAOPS_12, PLATFORM_3),
        )

    def test_a_title_not_in_the_conventional_commit_form_is_refused(self):
        for case, title in (
            ("no item", "feat(router): restart a stopped router"),
            ("the item first", "[PLATFORM-3] feat(router): restart a stopped router"),
            ("parentheses", "feat(router): restart a stopped router (PLATFORM-3)"),
            ("an unknown type", "feature(router): restart [PLATFORM-3]"),
            ("an upper case type", "Feat(router): restart [PLATFORM-3]"),
            ("an empty scope", "feat(): restart a stopped router [PLATFORM-3]"),
            ("no space after the colon", "feat(router):restart [PLATFORM-3]"),
            ("no subject", "feat(router): [PLATFORM-3]"),
            ("a plain title", "Restart a stopped router [PLATFORM-3]"),
        ):
            with self.subTest(case=case):
                with self.assertRaisesRegex(TitleError, r"<type>\(<scope>\)"):
                    pr_title.parse_title(title, CONVENTIONAL)

    def test_an_item_in_the_subject_is_refused(self):
        with self.assertRaisesRegex(
            TitleError, r"names \[INFRAOPS-12\] in its subject.*after the subject"
        ):
            pr_title.parse_title(
                "feat(router): add [INFRAOPS-12] edges [PLATFORM-3]", CONVENTIONAL
            )

    def test_items_with_no_subject_are_refused(self):
        with self.assertRaisesRegex(TitleError, "no subject"):
            pr_title.parse_title(
                "feat(router): [INFRAOPS-12] [PLATFORM-3]", CONVENTIONAL
            )


class UnbracketedItemHint(unittest.TestCase):
    def test_an_item_in_parentheses_gets_the_hint_of_the_brackets(self):
        with self.assertRaisesRegex(
            TitleError, r"name it alone in brackets, as \[PLATFORM-3\]"
        ):
            pr_title.parse_title("feat(router): restart (PLATFORM-3)", CONVENTIONAL)

    def test_an_item_in_a_branch_name_gets_the_hint_of_the_brackets(self):
        with self.assertRaisesRegex(TitleError, r"as \[FRAMEWORK-137\]"):
            pr_title.parse_title("[feat/FRAMEWORK-137] Restart", PLAIN)

    def test_a_title_with_an_item_in_brackets_gets_no_hint(self):
        with self.assertRaises(TitleError) as raised:
            pr_title.parse_title("Restart [FRAMEWORK-123] and ROS-2 nodes", PLAIN)
        self.assertNotIn("alone in brackets", str(raised.exception))

    def test_a_title_with_nothing_like_an_item_gets_no_hint(self):
        with self.assertRaises(TitleError) as raised:
            pr_title.parse_title("Restart an instance", PLAIN)
        self.assertNotIn("alone in brackets", str(raised.exception))


class TitleForms(unittest.TestCase):
    def test_the_forms_of_the_action_input(self):
        self.assertIs(pr_title.parse_title_form("plain"), PLAIN)
        self.assertIs(pr_title.parse_title_form("conventional-commit"), CONVENTIONAL)

    def test_another_form_is_refused(self):
        for text in ("", "Plain", "conventional"):
            with self.subTest(text=text):
                with self.assertRaisesRegex(
                    EventError, "`plain` or `conventional-commit`"
                ):
                    pr_title.parse_title_form(text)


def pull_request_event(
    title="[FRAMEWORK-123] Restart an instance",
    user_type="User",
    login="alice",
    base="dev",
    default_branch="dev",
):
    """A `pull_request_target` event of peppy, with what GitHub puts in it
    that the check reads."""
    return {
        "action": "edited",
        "pull_request": {
            "number": 7,
            "title": title,
            "user": {"login": login, "type": user_type},
            "base": {"ref": base},
            "head": {"ref": "feat/restart-policy"},
        },
        "repository": {
            "full_name": "Peppy-bot/peppy",
            "default_branch": default_branch,
        },
    }


def pull_request(**fields) -> PullRequest:
    return pr_title.parse_pull_request(pull_request_event(**fields))


class PullRequests(unittest.TestCase):
    def test_the_pull_request_of_an_event(self):
        self.assertEqual(
            pull_request(),
            PullRequest(
                title="[FRAMEWORK-123] Restart an instance",
                author="alice",
                opened_by_bot=False,
                base_branch="dev",
                default_branch="dev",
            ),
        )

    def test_an_event_without_a_pull_request_is_refused(self):
        for case, event in (
            (
                "a push",
                {"ref": "refs/heads/dev", "repository": {"default_branch": "dev"}},
            ),
            ("no repository", {"pull_request": pull_request_event()["pull_request"]}),
            (
                "a pull request without its user",
                {**pull_request_event(), "pull_request": {"title": "x"}},
            ),
        ):
            with self.subTest(case=case):
                with self.assertRaisesRegex(EventError, "lacks"):
                    pr_title.parse_pull_request(event)

    def test_a_human_pull_request_into_the_default_branch_needs_an_item(self):
        self.assertIsNone(pr_title.reason_no_item_is_needed(pull_request()))

    def test_a_pull_request_of_a_bot_needs_no_item(self):
        for login in ("dependabot[bot]", "peppy-release-bot[bot]"):
            with self.subTest(login=login):
                reason = pr_title.reason_no_item_is_needed(
                    pull_request(login=login, user_type="Bot", title="Bump serde")
                )
                self.assertIn(f"{login} opened this pull request", reason)

    def test_a_pull_request_into_another_branch_needs_no_item(self):
        reason = pr_title.reason_no_item_is_needed(
            pull_request(base="main", title="Release 0.34.0")
        )
        self.assertIn("goes into `main`, not into the default branch `dev`", reason)


class FakePlane:
    """The Plane API of the peppy workspace, as the account of API_KEY sees it:
    each project by identifier, with the account's membership and the numbers
    and names of its items. Each request it gets is recorded, and `failures`
    gives the answers of the first requests before the real ones (an
    HttpResponse, or an OSError to raise)."""

    def __init__(self, projects, page_size=100, failures=()):
        self.projects = projects
        self.page_size = page_size
        self.failures = list(failures)
        self.requests = []
        self.waits = []

    def plane(self, api_key=API_KEY):
        return Plane(api_key, send=self.send, sleep=self.waits.append)

    def send(self, request):
        self.requests.append(request)
        if self.failures:
            failure = self.failures.pop(0)
            if isinstance(failure, OSError):
                raise failure
            return failure
        if request.get_header("X-api-key") != API_KEY:
            # Plane answers a key it does not accept with 403, not 401.
            return json_response({"detail": "Given API token is not valid"}, status=403)
        url = urllib.parse.urlsplit(request.full_url)
        workspace = f"/api/v1/workspaces/{pr_title.PLANE_WORKSPACE}/"
        self.assert_prefix(url.path, workspace)
        path = url.path.removeprefix(workspace)
        if path == "projects/":
            return self.projects_page(urllib.parse.parse_qs(url.query))
        item = re.fullmatch(r"work-items/([A-Z0-9]+)-([0-9]+)/", path)
        if item is None:
            raise AssertionError(f"the check asked Plane for {request.full_url}")
        return self.work_item(item[1], int(item[2]))

    @staticmethod
    def assert_prefix(path, prefix):
        if not path.startswith(prefix):
            raise AssertionError(f"the check asked Plane for {path}")

    def projects_page(self, query):
        identifiers = sorted(self.projects)
        start = int(query.get("cursor", ["0"])[0])
        end = start + self.page_size
        return json_response(
            {
                "results": [
                    {
                        "identifier": identifier,
                        "name": f"{identifier.title()} project",
                        "is_member": self.projects[identifier][0],
                    }
                    for identifier in identifiers[start:end]
                ],
                "next_cursor": str(end),
                "next_page_results": end < len(identifiers),
            }
        )

    def work_item(self, project, number):
        is_member, items = self.projects.get(project, (False, {}))
        if not is_member:
            return json_response({"error": "You do not have permission"}, status=403)
        if number not in items:
            return json_response({"error": "Work item not found"}, status=404)
        return json_response({"name": items[number], "sequence_id": number})


def json_response(body, status=200, retry_after=None):
    return HttpResponse(status, json.dumps(body).encode(), retry_after)


WORKSPACE = {
    "FRAMEWORK": (True, {123: "Restart an instance under its policy"}),
    "SIMULATION": (True, {45: "Robots join over MCP"}),
    "PLATFORM": (False, {3: "Fix CI"}),
}


class PlaneQuestions(unittest.TestCase):
    def test_the_projects_of_the_workspace(self):
        projects = FakePlane(WORKSPACE).plane().projects()
        self.assertEqual(sorted(projects), ["FRAMEWORK", "PLATFORM", "SIMULATION"])
        self.assertTrue(projects["FRAMEWORK"].is_member)
        self.assertFalse(projects["PLATFORM"].is_member)

    def test_the_projects_of_every_page(self):
        fake = FakePlane(WORKSPACE, page_size=2)
        self.assertEqual(
            sorted(fake.plane().projects()), ["FRAMEWORK", "PLATFORM", "SIMULATION"]
        )
        self.assertEqual(len(fake.requests), 2)

    def test_a_page_given_twice_is_refused(self):
        fake = FakePlane(WORKSPACE)
        same_page = json_response(
            {"results": [], "next_cursor": "a", "next_page_results": True}
        )
        fake.send = lambda request: same_page
        with self.assertRaisesRegex(PlaneError, "page a .* twice"):
            fake.plane().projects()

    def test_a_work_item(self):
        self.assertEqual(
            FakePlane(WORKSPACE).plane().work_item(FRAMEWORK_123),
            pr_title.WorkItem(FRAMEWORK_123, "Restart an instance under its policy"),
        )

    def test_a_missing_work_item_is_none(self):
        self.assertIsNone(
            FakePlane(WORKSPACE).plane().work_item(PlaneItemId("FRAMEWORK", 9999))
        )

    def test_a_work_item_with_another_number_is_refused(self):
        fake = FakePlane(WORKSPACE)
        fake.send = lambda request: json_response({"name": "x", "sequence_id": 12})
        with self.assertRaisesRegex(PlaneError, "number 12 for FRAMEWORK-123"):
            fake.plane().work_item(FRAMEWORK_123)

    def test_a_work_item_without_its_name_is_refused(self):
        fake = FakePlane(WORKSPACE)
        fake.send = lambda request: json_response({"sequence_id": 123})
        with self.assertRaisesRegex(PlaneError, "FRAMEWORK-123 without 'name'"):
            fake.plane().work_item(FRAMEWORK_123)

    def test_each_request_carries_the_api_key_and_asks_for_json(self):
        fake = FakePlane(WORKSPACE)
        fake.plane().work_item(FRAMEWORK_123)
        (request,) = fake.requests
        self.assertEqual(
            request.full_url,
            "https://api.plane.so/api/v1/workspaces/peppy/work-items/FRAMEWORK-123/",
        )
        self.assertEqual(request.get_method(), "GET")
        self.assertEqual(request.get_header("X-api-key"), API_KEY)
        self.assertEqual(request.get_header("Accept"), "application/json")

    def test_a_refused_key_is_a_problem_of_the_secret(self):
        with self.assertRaisesRegex(PlaneError, r"HTTP 403.*PLANE_API_KEY"):
            FakePlane(WORKSPACE).plane(api_key="revoked").projects()

    def test_an_empty_key_asks_nothing(self):
        fake = FakePlane(WORKSPACE)
        with self.assertRaisesRegex(PlaneError, "PLANE_API_KEY secret is empty"):
            fake.plane(api_key="").projects()
        self.assertEqual(fake.requests, [])

    def test_another_error_quotes_the_answer(self):
        fake = FakePlane(WORKSPACE, failures=[HttpResponse(400, b"bad cursor", None)])
        with self.assertRaisesRegex(PlaneError, "HTTP 400: bad cursor"):
            fake.plane().projects()

    def test_an_answer_that_is_not_json_is_refused(self):
        fake = FakePlane(WORKSPACE, failures=[HttpResponse(200, b"<html>", None)])
        with self.assertRaisesRegex(PlaneError, "no JSON"):
            fake.plane().projects()


class PlaneRetries(unittest.TestCase):
    def test_the_rate_limit_waits_as_plane_asks(self):
        fake = FakePlane(WORKSPACE, failures=[HttpResponse(429, b"", "7")])
        self.assertIsNotNone(fake.plane().work_item(FRAMEWORK_123))
        self.assertEqual(fake.waits, [7])
        self.assertEqual(len(fake.requests), 2)

    def test_a_long_or_unreadable_retry_after_gives_a_bounded_wait(self):
        for retry_after, wait in (
            ("3600", pr_title.MAX_RETRY_WAIT_SECONDS),
            ("Wed, 21 Oct 2026 07:28:00 GMT", pr_title.DEFAULT_RETRY_WAIT_SECONDS),
            (None, pr_title.DEFAULT_RETRY_WAIT_SECONDS),
        ):
            with self.subTest(retry_after=retry_after):
                self.assertEqual(pr_title.retry_wait_seconds(retry_after), wait)

    def test_a_server_error_or_no_answer_is_tried_again(self):
        fake = FakePlane(
            WORKSPACE,
            failures=[HttpResponse(502, b"", None), ConnectionResetError("reset")],
        )
        self.assertIsNotNone(fake.plane().work_item(FRAMEWORK_123))
        self.assertEqual(len(fake.requests), 3)
        self.assertEqual(fake.waits, [pr_title.DEFAULT_RETRY_WAIT_SECONDS] * 2)

    def test_the_last_attempt_gives_up_without_a_wait(self):
        fake = FakePlane(
            WORKSPACE, failures=[HttpResponse(503, b"", None)] * pr_title.PLANE_ATTEMPTS
        )
        with self.assertRaisesRegex(PlaneError, r"HTTP 503 .* 3 times\. Re-run"):
            fake.plane().work_item(FRAMEWORK_123)
        self.assertEqual(len(fake.requests), pr_title.PLANE_ATTEMPTS)
        self.assertEqual(len(fake.waits), pr_title.PLANE_ATTEMPTS - 1)

    def test_a_missing_item_is_not_tried_again(self):
        fake = FakePlane(WORKSPACE)
        fake.plane().work_item(PlaneItemId("FRAMEWORK", 9999))
        self.assertEqual(len(fake.requests), 1)
        self.assertEqual(fake.waits, [])


class Checks(unittest.TestCase):
    def check(self, workspace=WORKSPACE, **fields):
        fake = FakePlane(workspace)
        outcome = pr_title.check(pull_request(**fields), PLAIN, fake.plane())
        return outcome, fake

    def test_items_that_exist_pass_with_their_names(self):
        outcome, fake = self.check(title="[FRAMEWORK-123] [SIMULATION-45] Restart")
        self.assertTrue(outcome.passed)
        self.assertEqual(
            outcome.notes,
            (
                "FRAMEWORK-123: Restart an instance under its policy",
                "SIMULATION-45: Robots join over MCP",
            ),
        )
        # One list of the projects, then one lookup for each item.
        self.assertEqual(len(fake.requests), 3)

    def test_an_unknown_project_names_the_projects_a_title_can_name(self):
        outcome, fake = self.check(title="[FRAMEWROK-123] Restart")
        self.assertEqual(
            outcome.problems,
            (
                "FRAMEWROK-123: the peppy workspace of Plane has no project FRAMEWROK. "
                "A title can name the items of FRAMEWORK, SIMULATION.",
            ),
        )
        self.assertEqual(len(fake.requests), 1)

    def test_an_account_in_no_project_says_so(self):
        outcome, _ = self.check(
            workspace={"PLATFORM": (False, {})}, title="[FRAMEWORK-123] Restart"
        )
        self.assertIn("is a member of no project", outcome.problems[0])

    def test_a_project_of_which_the_account_is_not_a_member(self):
        outcome, fake = self.check(title="[PLATFORM-3] Fix CI")
        (problem,) = outcome.problems
        self.assertIn("is not a member of the Plane project PLATFORM", problem)
        self.assertEqual(len(fake.requests), 1)

    def test_a_missing_item(self):
        outcome, _ = self.check(title="[FRAMEWORK-9999] Restart")
        self.assertEqual(
            outcome.problems,
            ("FRAMEWORK-9999: the Plane project FRAMEWORK has no item 9999.",),
        )

    def test_every_item_is_checked_and_each_problem_is_reported(self):
        outcome, _ = self.check(
            title="[FRAMEWORK-9999] [SIMULATION-45] [PLATFORM-3] Restart"
        )
        self.assertFalse(outcome.passed)
        self.assertEqual(len(outcome.problems), 2)
        self.assertEqual(outcome.notes, ("SIMULATION-45: Robots join over MCP",))

    def test_a_title_not_in_the_form_asks_plane_nothing(self):
        outcome, fake = self.check(title="Restart an instance")
        self.assertFalse(outcome.passed)
        self.assertEqual(fake.requests, [])

    def test_a_pull_request_that_needs_no_item_asks_plane_nothing(self):
        outcome, fake = self.check(base="main", title="Release 0.34.0")
        self.assertTrue(outcome.passed)
        self.assertEqual(fake.requests, [])

    def test_a_plane_failure_fails_the_check(self):
        fake = FakePlane(WORKSPACE, failures=[HttpResponse(401, b"", None)])
        outcome = pr_title.check(pull_request(), PLAIN, fake.plane())
        self.assertIn("PLANE_API_KEY", outcome.problems[0])


class Output(unittest.TestCase):
    def test_the_text_of_a_workflow_command_escapes_what_github_reads(self):
        self.assertEqual(
            pr_title.workflow_command_text("100% done\r\n::warning::x"),
            "100%25 done%0D%0A::warning::x",
        )

    def test_each_problem_is_an_error_annotation(self):
        outcome = pr_title.Outcome(problems=("a\nb", "c"), notes=("d",))
        self.assertEqual(
            pr_title.log_lines(outcome),
            ["::error title=pr-title::a%0Ab", "::error title=pr-title::c", "d"],
        )

    def test_the_summary_lists_the_items_or_the_problems(self):
        passed = pr_title.Outcome(problems=(), notes=("FRAMEWORK-123: Restart",))
        failed = pr_title.Outcome(problems=("No item",), notes=("ignored",))
        self.assertEqual(
            pr_title.summary_text(passed),
            "### pr-title passed\n\n- FRAMEWORK-123: Restart\n",
        )
        self.assertEqual(
            pr_title.summary_text(failed), "### pr-title failed\n\n- No item\n"
        )


class Main(unittest.TestCase):
    """The script as the action runs it, on events that ask Plane nothing."""

    def run_main(self, event, **environment):
        with tempfile.TemporaryDirectory() as directory:
            event_path = Path(directory) / "event.json"
            event_path.write_text(json.dumps(event))
            summary_path = Path(directory) / "summary.md"
            output = io.StringIO()
            with contextlib.redirect_stdout(output):
                status = pr_title.main(
                    {
                        "GITHUB_EVENT_PATH": str(event_path),
                        "GITHUB_STEP_SUMMARY": str(summary_path),
                        "TITLE_FORM": "plain",
                        **environment,
                    }
                )
            return status, output.getvalue(), summary_path.read_text()

    def test_a_title_not_in_the_form_fails_with_an_annotation(self):
        status, output, summary = self.run_main(pull_request_event(title="Restart"))
        self.assertEqual(status, 1)
        self.assertTrue(output.startswith("::error title=pr-title::The title must be"))
        self.assertTrue(summary.startswith("### pr-title failed"))

    def test_a_pull_request_of_a_bot_passes_without_a_key(self):
        status, output, _ = self.run_main(
            pull_request_event(login="dependabot[bot]", user_type="Bot", title="Bump")
        )
        self.assertEqual(status, 0)
        self.assertIn("dependabot[bot] opened this pull request", output)

    def test_a_title_in_the_form_without_a_key_fails(self):
        status, output, _ = self.run_main(pull_request_event(), PLANE_API_KEY="")
        self.assertEqual(status, 1)
        self.assertIn("PLANE_API_KEY secret is empty", output)

    def test_an_unknown_form_fails(self):
        status, output, _ = self.run_main(pull_request_event(), TITLE_FORM="loose")
        self.assertEqual(status, 1)
        self.assertIn("title-form input of the pr-title action is `loose`", output)

    def test_an_unreadable_event_fails(self):
        output = io.StringIO()
        with contextlib.redirect_stdout(output):
            status = pr_title.main(
                {"GITHUB_EVENT_PATH": "/nonexistent/event.json", "TITLE_FORM": "plain"}
            )
        self.assertEqual(status, 1)
        self.assertIn("cannot be read", output.getvalue())


def workflow_lines(path):
    return [line.strip() for line in path.read_text().splitlines()]


class RepositoryFacts(unittest.TestCase):
    def test_the_action_gives_the_script_every_variable_it_reads(self):
        lines = workflow_lines(ACTION / "action.yml")
        for line in (
            "PLANE_API_KEY: ${{ inputs.plane-api-key }}",
            "TITLE_FORM: ${{ inputs.title-form }}",
            'run: python3 "$GITHUB_ACTION_PATH/pr_title.py"',
            f"default: {TitleForm.PLAIN.value}",
        ):
            with self.subTest(line=line):
                self.assertIn(line, lines)

    def test_peppy_runs_the_check_on_the_events_that_change_a_title(self):
        lines = workflow_lines(PEPPY_WORKFLOW)
        for line in (
            "name: pr-title",
            "pull_request_target:",
            "types: [opened, edited, synchronize, reopened]",
            "permissions: {}",
            "pr-title:",
            "- uses: Peppy-bot/peppy/.github/actions/pr-title@dev",
            "plane-api-key: ${{ secrets.PLANE_API_KEY }}",
        ):
            with self.subTest(line=line):
                self.assertIn(line, lines)

    def test_the_check_never_checks_out_the_pull_request(self):
        # `pull_request_target` gives the job the secret, so the job must run
        # nothing of the pull request.
        text = PEPPY_WORKFLOW.read_text()
        self.assertNotIn("actions/checkout", text)
        self.assertNotIn("${{ github.event.pull_request", text)

    def test_the_check_runs_on_no_condition(self):
        # A job that its `if` skips concludes `skipped`, which passes the
        # required check whatever the title: the script decides alone.
        self.assertFalse(
            any(line.startswith("if:") for line in workflow_lines(PEPPY_WORKFLOW))
        )

    def test_the_changes_job_runs_these_cases(self):
        text = (REPOSITORY / ".github" / "workflows" / "tests.yml").read_text()
        self.assertIn("--start-directory .github/actions/pr-title", text)


if __name__ == "__main__":
    unittest.main()
