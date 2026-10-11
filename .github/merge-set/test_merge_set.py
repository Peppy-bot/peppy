#!/usr/bin/env python3
"""Tests for the merge-set bot.

The decisions run on fixed input data. The sync runs against FakeGitHub, an
in-memory GitHub with the methods of GitHubGateway, which records what the bot
writes; its waits are recorded, never slept. GitHubGateway runs against
FakeApi, which answers each call from a table, and FakeGit. GitMerges runs the
git of the host against GitFixture, a repository in a temporary directory
that it fetches from as it fetches from GitHub. Nothing touches the network.
The last cases hold the workflows of this repository to the names the script
uses, so the changes job of tests.yml runs these cases on every change.
"""

import base64
import io
import json
import os
import subprocess
import tempfile
import unittest
import zipfile
from dataclasses import replace
from pathlib import Path
from unittest.mock import patch

import merge_set
import resolve
from merge_set import (
    PEPPY,
    AccessRefusal,
    Action,
    ApiError,
    BranchRules,
    CheckRun,
    CheckState,
    CommitStatus,
    Gate,
    HubRun,
    JobRecord,
    MergeRefused,
    MergeSetError,
    PullRequest,
    RequiredCheck,
    ReviewDecision,
    TestedRef,
    TestedSet,
)

REPOSITORY_ROOT = Path(__file__).resolve().parents[2]
WORKFLOWS = REPOSITORY_ROOT / ".github" / "workflows"

NODES_HUB = merge_set.REPOSITORIES_BY_NAME["nodes-hub"]
CONTRACTS_HUB = merge_set.REPOSITORIES_BY_NAME["contracts-hub"]
LAUNCHERS_HUB = merge_set.REPOSITORIES_BY_NAME["launchers-hub"]
PRIVATE_NODES_HUB = merge_set.REPOSITORIES_BY_NAME["private-nodes-hub"]

SET_NAME = "feat/gripper-force"
BOT = "peppy-merge-set[bot]"
RUN_URL = "https://github.com/Peppy-bot/peppy/actions/runs/99"
CONTEXT = merge_set.RunContext(bot_login=BOT, run_url=RUN_URL)
TEST_CHECK = RequiredCheck("test", 15368)
# The ruleset of the integration branches that requires an approval.
APPROVAL_RULESET = 20864833
RULES = BranchRules((TEST_CHECK,), strict=False, approval_rulesets=(APPROVAL_RULESET,))


def commit(repository, label="head"):
    """A distinct, fixed commit for each repository and label."""
    return f"{repository.name}:{label}".encode().hex()[:40].ljust(40, "0")


def pull_request(repository, number=1, **changes):
    fields = dict(
        repository=repository,
        number=number,
        url=f"https://github.com/Peppy-bot/{repository.name}/pull/{number}",
        title=f"Change {repository.name}",
        # The user who ticks the boxes unless a test names another one.
        author="alice",
        is_open=True,
        merged=False,
        draft=False,
        head_commit=commit(repository),
        head_branch=SET_NAME,
        base_branch=repository.integration_branch,
        from_fork=False,
    )
    fields.update(changes)
    return PullRequest(**fields)


def member(repository, *pull_requests, head=None):
    return merge_set.Member(
        repository, head or commit(repository), tuple(pull_requests)
    )


def set_state(*members, left_behind=()):
    return merge_set.SetState(SET_NAME, tuple(members), tuple(left_behind))


def tested_set(hubs=None, peppy=None):
    return TestedSet(hubs=hubs or {}, peppy=peppy)


def no_clean_base_merge(mismatch):
    """The answer of a branch that changed on its own since the job ran."""
    return False


def run_url(repository, run_id):
    return f"https://github.com/Peppy-bot/{repository.name}/actions/runs/{run_id}"


def hub_run(repository, run_id=10, completed=True, records=()):
    return HubRun(
        repository=repository,
        id=run_id,
        name=f"Tests #{run_id}",
        url=run_url(repository, run_id),
        completed=completed,
        records=tuple(records),
    )


def ci_run_of(repository, state=CheckState.PASSED, run_id=10):
    return merge_set.CiRun(run_url(repository, run_id), state)


def report(pr, **changes):
    fields = dict(
        pull_request=pr,
        mergeable=True,
        review=ReviewDecision.APPROVED,
        checks=((TEST_CHECK, CheckState.PASSED),),
        ci=ci_run_of(pr.repository),
        behind=False,
        stale=(),
        unbypassed_approval_rulesets=(),
    )
    fields.update(changes)
    return merge_set.PullRequestReport(**fields)


def reports_by_label(*reports):
    return {report.pull_request.label: report for report in reports}


def blocker_texts(state, reports):
    return [blocker.text for blocker in merge_set.set_blockers(state, reports)]


def record_bundle(text, name=resolve.SET_RECORD_FILE):
    buffer = io.BytesIO()
    with zipfile.ZipFile(buffer, "w") as archive:
        archive.writestr(name, text)
    return buffer.getvalue()


def set_record_text(hubs, peppy=None):
    return json.dumps(
        {
            "name": SET_NAME,
            "peppy": peppy
            or {
                "build": "latest-release",
                "version": "peppy v0.31.3",
                "source": "https://peppy.bot/latest/x.tgz",
                "ref": None,
                "commit": None,
            },
            "hubs": hubs,
        }
    )


class Repositories(unittest.TestCase):
    def test_peppy_merges_first_then_the_hubs_by_repository_id(self):
        self.assertEqual(
            [repository.name for repository in merge_set.REPOSITORIES],
            ["peppy", *(hub.name for hub in resolve.HUBS)],
        )

    def test_each_repository_merges_into_its_integration_branch(self):
        self.assertEqual(PEPPY.integration_branch, "dev")
        for repository in merge_set.REPOSITORIES[1:]:
            with self.subTest(repository=repository.name):
                self.assertEqual(repository.integration_branch, "main")
                self.assertIs(repository.hub, resolve.HUBS_BY_NAME[repository.name])

    def test_peppy_is_the_repository_resolve_names(self):
        self.assertEqual(PEPPY.full_name, resolve.PEPPY_REPOSITORY)

    def test_the_token_reaches_every_repository_of_a_set(self):
        self.assertEqual(
            merge_set.repository_list(),
            "peppy,nodes-hub,launchers-hub,contracts-hub,mcp-hub,pairings-hub,"
            "private-nodes-hub",
        )

    def test_a_repository_outside_every_set_is_refused(self):
        with self.assertRaises(MergeSetError):
            merge_set.repository_named("landing-page")

    def test_a_full_name_names_a_repository_of_a_set_or_none(self):
        self.assertIs(merge_set.repository_of_full_name("Peppy-bot/peppy"), PEPPY)
        self.assertIs(merge_set.repository_of_full_name("peppy-bot/peppy"), PEPPY)
        self.assertIsNone(merge_set.repository_of_full_name("someone/peppy"))
        self.assertIsNone(merge_set.repository_of_full_name("Peppy-bot/landing-page"))


class Events(unittest.TestCase):
    def test_the_branch_of_an_event_names_its_set(self):
        event = merge_set.parse_event(
            "nodes-hub", SET_NAME, "Peppy-bot/nodes-hub", "12"
        )
        self.assertEqual(event.set_name, SET_NAME)
        self.assertEqual(event.pull_request, 12)
        self.assertFalse(event.from_fork)

    def test_an_event_from_a_fork_or_an_integration_branch_has_no_set(self):
        cases = [
            ("nodes-hub", SET_NAME, "someone/nodes-hub"),
            ("nodes-hub", SET_NAME, ""),
            ("nodes-hub", "main", "Peppy-bot/nodes-hub"),
            ("peppy", "dev", "Peppy-bot/peppy"),
        ]
        for repository, branch, head_repository in cases:
            with self.subTest(branch=branch, head_repository=head_repository):
                event = merge_set.parse_event(repository, branch, head_repository, "")
                self.assertIsNone(event.set_name)

    def test_a_malformed_event_is_refused(self):
        for arguments in (
            ("nodes-hub", "", "Peppy-bot/nodes-hub", ""),
            ("nodes-hub", SET_NAME, "Peppy-bot/nodes-hub", "twelve"),
            ("landing-page", SET_NAME, "Peppy-bot/landing-page", ""),
        ):
            with self.subTest(arguments=arguments):
                with self.assertRaises(MergeSetError):
                    merge_set.parse_event(*arguments)


class Relay(unittest.TestCase):
    # The events of pull request 7 of nodes-hub, from branch SET_NAME into
    # main, as the webhook of the App gets them.

    def assert_relayed(self, event_name, payload, subject):
        self.assertEqual(merge_set.relay_subject(event_name, payload, BOT), subject)

    def assert_dropped(self, event_name, payload):
        self.assertIsNone(merge_set.relay_subject(event_name, payload, BOT))

    def test_a_pull_request_that_changes_names_its_branch_and_head_repository(self):
        for action in sorted(merge_set.PULL_REQUEST_ACTIONS - {"edited"}):
            with self.subTest(action=action):
                self.assert_relayed(
                    "pull_request", pull_request_event(action), PULL_REQUEST_SUBJECT
                )

    def test_a_pull_request_moved_onto_the_default_branch_is_relayed(self):
        moved = pull_request_event("edited", changes={"base": {"ref": {"from": "x"}}})
        self.assert_relayed("pull_request", moved, PULL_REQUEST_SUBJECT)
        self.assert_dropped(
            "pull_request",
            pull_request_event("edited", changes={"title": {"from": "x"}}),
        )

    def test_a_pull_request_into_another_branch_or_that_does_not_change_is_dropped(
        self,
    ):
        self.assert_dropped("pull_request", pull_request_event("opened", base="other"))
        for action in ("labeled", "assigned", "review_requested"):
            with self.subTest(action=action):
                self.assert_dropped("pull_request", pull_request_event(action))

    def test_a_pull_request_from_a_deleted_fork_has_no_head_repository(self):
        payload = pull_request_event("opened", head_repository=None)
        subject = merge_set.relay_subject("pull_request", payload, BOT)
        self.assertIsNone(subject.head_repository)
        inputs = merge_set.dispatch_inputs(NODES_HUB, subject)
        self.assertEqual(inputs["head-repository"], "")
        event = merge_set.parse_event(
            inputs["repository"],
            inputs["branch"],
            inputs["head-repository"],
            inputs["pull-request"],
        )
        self.assertTrue(event.from_fork)

    def test_a_review_that_changes_the_approvals_is_relayed(self):
        for action in sorted(merge_set.REVIEW_ACTIONS):
            with self.subTest(action=action):
                self.assert_relayed(
                    "pull_request_review",
                    pull_request_event(action),
                    PULL_REQUEST_SUBJECT,
                )
        self.assert_dropped("pull_request_review", pull_request_event("edited"))
        self.assert_dropped(
            "pull_request_review", pull_request_event("submitted", base="other")
        )

    def test_a_user_edit_of_a_dashboard_names_its_pull_request_alone(self):
        subject = merge_set.relay_subject(
            "issue_comment", comment_edit(dashboard_text(), dashboard_text()), BOT
        )
        self.assertEqual(subject, merge_set.RelaySubject(7, None, None))
        with self.assertRaises(MergeSetError):
            merge_set.dispatch_inputs(NODES_HUB, subject)

    def test_every_other_comment_event_is_dropped(self):
        def edit(**changes):
            payload = comment_edit(dashboard_text(), dashboard_text(Action.MERGE))
            for path, value in changes.items():
                *parents, key = path.split("__")
                node = payload
                for parent in parents:
                    node = node[parent]
                node[key] = value
            return payload

        for case, payload in (
            ("a new comment", edit(action="created")),
            ("a comment of an issue", edit(issue__pull_request=None)),
            ("a comment of a user", edit(comment__user__login="alice")),
            ("a comment of another bot", edit(comment__user__login="other[bot]")),
            ("an edit of a bot", edit(sender__type="Bot")),
            ("another comment of the bot", edit(comment__body="Merged.")),
        ):
            with self.subTest(case=case):
                self.assert_dropped("issue_comment", payload)

    def test_the_start_and_the_end_of_a_ci_run_name_its_branch_and_pull_request(
        self,
    ):
        for action in ("requested", "completed"):
            with self.subTest(action=action):
                self.assert_relayed(
                    "workflow_run", ci_run_event(action), PULL_REQUEST_SUBJECT
                )

    def test_the_start_of_a_re_run_alone_is_relayed_as_in_progress(self):
        # GitHub sends no `requested` for a re-run.
        self.assert_dropped("workflow_run", ci_run_event("in_progress", attempt=1))
        self.assert_relayed(
            "workflow_run",
            ci_run_event("in_progress", attempt=2),
            PULL_REQUEST_SUBJECT,
        )

    def test_a_run_of_another_workflow_or_event_is_dropped(self):
        self.assert_dropped("workflow_run", ci_run_event("completed", workflow="Lint"))
        self.assert_dropped("workflow_run", ci_run_event("completed", event="push"))

    def test_a_ci_run_of_a_fork_names_no_pull_request(self):
        payload = ci_run_event("completed", head_repository="someone/nodes-hub")
        payload["workflow_run"]["pull_requests"] = []
        subject = merge_set.relay_subject("workflow_run", payload, BOT)
        self.assertIsNone(subject.pull_request)
        self.assertEqual(
            merge_set.dispatch_inputs(NODES_HUB, subject)["pull-request"], ""
        )

    def test_a_deleted_branch_is_relayed_and_a_deleted_tag_is_not(self):
        branch = webhook_event(ref=SET_NAME, ref_type="branch")
        self.assert_relayed(
            "delete",
            branch,
            merge_set.RelaySubject(None, SET_NAME, "Peppy-bot/nodes-hub"),
        )
        self.assert_dropped("delete", {**branch, "ref_type": "tag"})

    def test_an_event_the_relay_does_not_serve_is_dropped(self):
        for event_name in ("push", "installation", "check_run"):
            with self.subTest(event_name=event_name):
                self.assert_dropped(event_name, webhook_event(action="created"))

    def test_an_event_without_what_the_relay_reads_is_refused(self):
        with self.assertRaises(MergeSetError):
            merge_set.relay_subject("pull_request", {"pull_request": {}}, BOT)

    def relay(self, event_name, payload):
        """What the relay says, and its API calls, each (token, method, path,
        body). The token of each call names its scope."""
        calls = []

        def request(api, method, path, query=None, body=None):
            calls.append((api.token, method, path, body))
            if method == "GET":
                return pull_request_item()
            if path.endswith("/dispatches"):
                return {
                    "workflow_run_id": 99,
                    "run_url": "https://api.github.com/repos/Peppy-bot/peppy/actions/runs/99",
                    "html_url": RELAY_RUN_URL,
                }
            return {"id": 1}

        with patch.object(merge_set.GitHubApi, "request", request):
            text = merge_set.relay(
                event_name,
                payload,
                BOT,
                lambda scope: merge_set.GitHubApi(token_of_scope(scope)),
            )
        return text, calls

    DISPATCH = (
        "peppy:actions=write",
        "POST",
        "/repos/Peppy-bot/peppy/actions/workflows/merge-set.yml/dispatches",
        {
            "ref": "dev",
            "inputs": {
                "repository": "nodes-hub",
                "branch": SET_NAME,
                "head-repository": "Peppy-bot/nodes-hub",
                "pull-request": "7",
            },
            "return_run_details": True,
        },
    )

    def test_the_relay_starts_the_sync_for_an_event_that_can_change_a_set(self):
        text, calls = self.relay("pull_request", pull_request_event("synchronize"))
        self.assertEqual(calls, [self.DISPATCH])
        self.assertIn(RELAY_RUN_URL, text)

    def test_the_relay_reads_the_branch_of_a_dashboard_edit(self):
        edit = comment_edit(before=dashboard_text(), after=dashboard_text() + "x")
        text, calls = self.relay("issue_comment", edit)
        self.assertEqual(
            calls,
            [
                (
                    "nodes-hub:pull_requests=write",
                    "GET",
                    "/repos/Peppy-bot/nodes-hub/pulls/7",
                    None,
                ),
                self.DISPATCH,
            ],
        )
        self.assertNotIn("Answered", text)

    def test_a_ticked_box_is_answered_on_its_pull_request_with_the_run(self):
        edit = comment_edit(
            before=dashboard_text(), after=dashboard_text(Action.MERGE), login="bob"
        )
        text, calls = self.relay("issue_comment", edit)
        self.assertEqual(
            calls[1:],
            [
                self.DISPATCH,
                (
                    "nodes-hub:pull_requests=write",
                    "POST",
                    "/repos/Peppy-bot/nodes-hub/issues/7/comments",
                    {
                        "body": f"@bob, the merge-set bot got your request to merge the "
                        f"set `{SET_NAME}`. [This run]({RELAY_RUN_URL}) acts on it, and "
                        "the bot reports the result here.\n"
                    },
                ),
            ],
        )
        self.assertTrue(text.endswith("Answered the request of @bob on #7."))

    def test_the_relay_calls_nothing_for_an_event_that_cannot_change_a_set(self):
        text, calls = self.relay("pull_request", pull_request_event("labeled"))
        self.assertEqual(calls, [])
        self.assertEqual(text, "The `pull_request` event cannot change a set.")

    def test_the_relay_calls_nothing_for_a_repository_no_set_spans(self):
        payload = pull_request_event("opened")
        payload["repository"]["full_name"] = "Peppy-bot/landing-page"
        text, calls = self.relay("pull_request", payload)
        self.assertEqual(calls, [])
        self.assertEqual(text, "No set spans the repository `Peppy-bot/landing-page`.")

    def test_a_dispatch_that_names_no_run_is_refused(self):
        self.assertEqual(
            merge_set.dispatched_run_url({"html_url": RELAY_RUN_URL}), RELAY_RUN_URL
        )
        for response in (None, {}, {"html_url": None}):
            with self.subTest(response=response):
                with self.assertRaises(MergeSetError) as refused:
                    merge_set.dispatched_run_url(response)
                self.assertIn("merge-set.yml names no run", str(refused.exception))


RELAY_RUN_URL = "https://github.com/Peppy-bot/peppy/actions/runs/99"
PULL_REQUEST_SUBJECT = merge_set.RelaySubject(7, SET_NAME, "Peppy-bot/nodes-hub")


def token_of_scope(scope):
    """A stand-in token that names its scope."""
    permissions = ",".join(f"{name}={level}" for name, level in scope.permissions)
    return f"{','.join(scope.repositories)}:{permissions}"


def webhook_event(**fields):
    """A webhook payload of an event of nodes-hub."""
    return {
        "repository": {"full_name": "Peppy-bot/nodes-hub", "default_branch": "main"},
        "installation": {"id": 5},
        "sender": {"login": "alice", "type": "User"},
        **fields,
    }


def pull_request_item(base="main", head_repository="Peppy-bot/nodes-hub"):
    """Pull request 7 of nodes-hub, from branch SET_NAME."""
    return {
        "number": 7,
        "head": {
            "ref": SET_NAME,
            "repo": None if head_repository is None else {"full_name": head_repository},
        },
        "base": {"ref": base},
    }


def pull_request_event(
    action, base="main", head_repository="Peppy-bot/nodes-hub", **fields
):
    """A `pull_request` or `pull_request_review` event of pull request 7."""
    return webhook_event(
        action=action,
        pull_request=pull_request_item(base, head_repository),
        **fields,
    )


def ci_run_event(
    action,
    attempt=1,
    workflow=merge_set.CI_WORKFLOW,
    event="pull_request",
    head_repository="Peppy-bot/nodes-hub",
):
    """A `workflow_run` event of a CI run of pull request 7."""
    return webhook_event(
        action=action,
        workflow={"name": workflow},
        workflow_run={
            "event": event,
            "run_attempt": attempt,
            "head_branch": SET_NAME,
            "head_repository": {"full_name": head_repository},
            "pull_requests": [{"number": 7}],
        },
    )


def dashboard_text(*ticked):
    """A merge-set comment whose boxes of `ticked` are ticked."""
    boxes = [merge_set.box_line(action, action.value) for action in Action]
    return (
        f"{merge_set.DASHBOARD_MARKER}\n### Set `{SET_NAME}`\n\n"
        + "\n".join(
            box.replace("- [ ]", "- [x]") if action in ticked else box
            for action, box in zip(Action, boxes)
        )
        + "\n"
    )


def comment_edit(before, after, login="alice"):
    """A user's edit of the merge-set comment of pull request 7."""
    return webhook_event(
        action="edited",
        issue={"number": 7, "pull_request": {"number": 7}},
        comment={"body": after, "user": {"login": BOT}},
        changes={"body": {"from": before}},
        sender={"login": login, "type": "User"},
    )


class TickedBoxesOfAnEdit(unittest.TestCase):
    def test_a_box_ticked_by_the_edit_is_a_request_of_its_editor(self):
        edit = comment_edit(dashboard_text(), dashboard_text(Action.RERUN), "bob")
        self.assertEqual(
            merge_set.ticked_boxes_of_edit(edit),
            merge_set.TickedBoxes("bob", (Action.RERUN,)),
        )

    def test_both_boxes_ticked_by_one_edit_are_both_requests(self):
        edit = comment_edit(
            dashboard_text(), dashboard_text(Action.MERGE, Action.RERUN)
        )
        self.assertEqual(
            merge_set.ticked_boxes_of_edit(edit).actions, (Action.MERGE, Action.RERUN)
        )

    def test_an_edit_that_ticks_no_box_is_no_request(self):
        for before, after in (
            # A box ticked before the edit and still ticked.
            (dashboard_text(Action.MERGE), dashboard_text(Action.MERGE) + "x"),
            # A box unticked.
            (dashboard_text(Action.MERGE), dashboard_text()),
            # Other text changed.
            (dashboard_text(), dashboard_text() + "x"),
        ):
            with self.subTest(before=before, after=after):
                edit = comment_edit(before, after)
                self.assertEqual(merge_set.ticked_boxes_of_edit(edit).actions, ())

    def test_an_edit_without_the_text_from_before_changed_no_text(self):
        edit = comment_edit(dashboard_text(), dashboard_text(Action.MERGE))
        del edit["changes"]
        self.assertEqual(merge_set.ticked_boxes_of_edit(edit).actions, ())

    def test_an_edit_without_its_comment_or_editor_is_refused(self):
        for missing in ("comment", "sender"):
            edit = comment_edit(dashboard_text(), dashboard_text(Action.MERGE))
            del edit[missing]
            with self.subTest(missing=missing), self.assertRaises(MergeSetError):
                merge_set.ticked_boxes_of_edit(edit)

    def test_the_answer_names_each_request_and_the_run(self):
        ticked = merge_set.TickedBoxes("bob", (Action.MERGE, Action.RERUN))
        self.assertEqual(
            merge_set.render_request_received(ticked, SET_NAME, RELAY_RUN_URL),
            f"@bob, the merge-set bot got your request to merge the set `{SET_NAME}` "
            f"and to re-run the out-of-date CI of the set `{SET_NAME}`. "
            f"[This run]({RELAY_RUN_URL}) acts on it, and the bot reports the result "
            "here.\n",
        )


class PullRequests(unittest.TestCase):
    def item(self, **changes):
        item = {
            "number": 12,
            "html_url": "https://github.com/Peppy-bot/nodes-hub/pull/12",
            "title": "Add the gripper force",
            "user": {"login": "alice"},
            "state": "open",
            "merged_at": None,
            "draft": False,
            "head": {
                "sha": commit(NODES_HUB),
                "ref": SET_NAME,
                "repo": {"full_name": "Peppy-bot/nodes-hub"},
            },
            "base": {"ref": "main", "repo": {"full_name": "Peppy-bot/nodes-hub"}},
        }
        item.update(changes)
        return item

    def test_an_open_pull_request_of_the_repository(self):
        parsed = merge_set.parse_pull_request(NODES_HUB, self.item())
        self.assertEqual(
            parsed, pull_request(NODES_HUB, 12, url=parsed.url, title=parsed.title)
        )
        self.assertEqual(parsed.label, "nodes-hub#12")
        self.assertEqual(parsed.reference, "Peppy-bot/nodes-hub#12")

    def test_a_merged_pull_request(self):
        parsed = merge_set.parse_pull_request(
            NODES_HUB, self.item(state="closed", merged_at="2026-09-28T10:00:00Z")
        )
        self.assertFalse(parsed.is_open)
        self.assertTrue(parsed.merged)

    def test_the_author_is_the_user_who_opened_it(self):
        self.assertEqual(
            merge_set.parse_pull_request(NODES_HUB, self.item()).author, "alice"
        )

    def test_a_pull_request_whose_author_github_does_not_name(self):
        unnamed = self.item()
        del unnamed["user"]
        for item in (self.item(user=None), unnamed):
            with self.subTest(item=item):
                self.assertIsNone(merge_set.parse_pull_request(NODES_HUB, item).author)

    def test_a_pull_request_from_a_fork_or_a_deleted_fork(self):
        for repo in ({"full_name": "someone/nodes-hub"}, None):
            with self.subTest(repo=repo):
                item = self.item()
                item["head"] = {**item["head"], "repo": repo}
                self.assertTrue(merge_set.parse_pull_request(NODES_HUB, item).from_fork)


class ChooseSet(unittest.TestCase):
    def heads(self, *repositories):
        return {
            repository: commit(repository) if repository in repositories else None
            for repository in merge_set.REPOSITORIES
        }

    def pulls(self, **by_name):
        pulls = {repository: [] for repository in merge_set.REPOSITORIES}
        for name, pull_requests in by_name.items():
            pulls[merge_set.REPOSITORIES_BY_NAME[name.replace("_", "-")]] = (
                pull_requests
            )
        return pulls

    def test_the_members_are_the_repositories_with_the_branch_in_merge_order(self):
        state = merge_set.choose_set(
            SET_NAME,
            self.heads(CONTRACTS_HUB, PEPPY, NODES_HUB),
            self.pulls(
                peppy=[pull_request(PEPPY)],
                nodes_hub=[pull_request(NODES_HUB)],
                contracts_hub=[pull_request(CONTRACTS_HUB)],
            ),
        )
        self.assertEqual(
            [m.repository for m in state.members], [PEPPY, NODES_HUB, CONTRACTS_HUB]
        )
        self.assertFalse(state.is_alone)
        self.assertEqual(state.left_behind, ())

    def test_one_repository_with_the_branch_is_alone(self):
        state = merge_set.choose_set(
            SET_NAME,
            self.heads(NODES_HUB),
            self.pulls(nodes_hub=[pull_request(NODES_HUB)]),
        )
        self.assertTrue(state.is_alone)
        self.assertEqual(state.open_pull_requests(), [pull_request(NODES_HUB)])

    def test_a_branch_without_an_open_pull_request_is_a_member(self):
        closed = pull_request(CONTRACTS_HUB, 3, is_open=False)
        state = merge_set.choose_set(
            SET_NAME,
            self.heads(NODES_HUB, CONTRACTS_HUB),
            self.pulls(nodes_hub=[pull_request(NODES_HUB)], contracts_hub=[closed]),
        )
        contracts = state.member_of(CONTRACTS_HUB)
        self.assertEqual(contracts.open_pull_requests, ())
        self.assertIsNone(contracts.pull_request)

    def test_the_branch_of_a_merged_pull_request_is_left_behind(self):
        merged = pull_request(CONTRACTS_HUB, 3, is_open=False, merged=True)
        state = merge_set.choose_set(
            SET_NAME,
            self.heads(NODES_HUB, CONTRACTS_HUB),
            self.pulls(nodes_hub=[pull_request(NODES_HUB)], contracts_hub=[merged]),
        )
        self.assertEqual([m.repository for m in state.members], [NODES_HUB])
        self.assertEqual(state.left_behind, (CONTRACTS_HUB,))
        self.assertTrue(state.is_alone)

    def test_a_branch_with_commits_after_its_merged_pull_request_is_a_member(self):
        merged = pull_request(
            CONTRACTS_HUB,
            3,
            is_open=False,
            merged=True,
            head_commit=commit(CONTRACTS_HUB, "old"),
        )
        state = merge_set.choose_set(
            SET_NAME,
            self.heads(NODES_HUB, CONTRACTS_HUB),
            self.pulls(nodes_hub=[pull_request(NODES_HUB)], contracts_hub=[merged]),
        )
        self.assertEqual(
            [m.repository for m in state.members], [NODES_HUB, CONTRACTS_HUB]
        )

    def test_only_the_newest_closed_pull_request_decides(self):
        # The branch name was used before: the newest pull request of it
        # closed unmerged, so the branch still holds a change.
        older = pull_request(CONTRACTS_HUB, 3, is_open=False, merged=True)
        newer = pull_request(CONTRACTS_HUB, 5, is_open=False)
        state = merge_set.choose_set(
            SET_NAME,
            self.heads(NODES_HUB, CONTRACTS_HUB),
            self.pulls(
                nodes_hub=[pull_request(NODES_HUB)], contracts_hub=[older, newer]
            ),
        )
        self.assertEqual(len(state.members), 2)

    def test_pull_requests_from_forks_are_not_part_of_the_set(self):
        fork = pull_request(CONTRACTS_HUB, 3, from_fork=True)
        state = merge_set.choose_set(
            SET_NAME,
            self.heads(NODES_HUB, CONTRACTS_HUB),
            self.pulls(nodes_hub=[pull_request(NODES_HUB)], contracts_hub=[fork]),
        )
        self.assertEqual(state.member_of(CONTRACTS_HUB).open_pull_requests, ())

    def test_several_open_pull_requests_from_the_branch_are_kept(self):
        first = pull_request(NODES_HUB, 1)
        second = pull_request(NODES_HUB, 2, base_branch="release")
        state = merge_set.choose_set(
            SET_NAME, self.heads(NODES_HUB), self.pulls(nodes_hub=[first, second])
        )
        self.assertEqual(state.member_of(NODES_HUB).open_pull_requests, (first, second))
        self.assertIsNone(state.member_of(NODES_HUB).pull_request)


class Rules(unittest.TestCase):
    def rule(self, *checks, strict=False):
        return {
            "type": "required_status_checks",
            "parameters": {
                "strict_required_status_checks_policy": strict,
                "required_status_checks": [
                    {"context": context, **({"integration_id": app} if app else {})}
                    for context, app in checks
                ],
            },
        }

    def test_the_required_checks_of_every_ruleset_leave_out_the_gate(self):
        rules = merge_set.parse_branch_rules(
            [
                {"type": "deletion"},
                self.rule(("test", 15368), ("merge-set", 4242)),
                self.rule(("test", 15368), ("check-index", None)),
            ]
        )
        self.assertEqual(
            rules,
            BranchRules(
                (RequiredCheck("test", 15368), RequiredCheck("check-index", None)),
                strict=False,
                approval_rulesets=(),
            ),
        )

    def test_one_strict_ruleset_makes_the_branch_strict(self):
        rules = merge_set.parse_branch_rules(
            [self.rule(("test", 15368)), self.rule(("lint", None), strict=True)]
        )
        self.assertTrue(rules.strict)

    def test_the_rulesets_that_require_an_approval_are_kept_by_id(self):
        def pull_request_rule(ruleset_id, **parameters):
            return {
                "type": "pull_request",
                "ruleset_id": ruleset_id,
                "parameters": {
                    "required_approving_review_count": 0,
                    "require_code_owner_review": False,
                    "required_reviewers": [],
                    **parameters,
                },
            }

        rules = merge_set.parse_branch_rules(
            [
                {"type": "deletion", "ruleset_id": 1},
                pull_request_rule(2, required_approving_review_count=1),
                pull_request_rule(3),
                pull_request_rule(4, require_code_owner_review=True),
                pull_request_rule(
                    5, required_reviewers=[{"reviewer": {"id": 7, "type": "Team"}}]
                ),
                pull_request_rule(2, required_approving_review_count=2),
            ]
        )
        self.assertEqual(rules.approval_rulesets, (2, 4, 5))


class RequiredChecks(unittest.TestCase):
    def run_of(
        self,
        run_id,
        name="test",
        app_id=15368,
        status="completed",
        conclusion="success",
    ):
        return CheckRun(run_id, name, app_id, status == "completed", conclusion)

    def test_the_newest_check_run_of_the_app_decides(self):
        runs = [self.run_of(1, conclusion="failure"), self.run_of(2)]
        self.assertIs(
            merge_set.required_check_state(TEST_CHECK, runs, []), CheckState.PASSED
        )
        runs = [self.run_of(2, conclusion="failure"), self.run_of(1)]
        self.assertIs(
            merge_set.required_check_state(TEST_CHECK, runs, []), CheckState.FAILED
        )

    def test_a_skipped_or_neutral_check_run_passes(self):
        for conclusion in ("skipped", "neutral"):
            with self.subTest(conclusion=conclusion):
                runs = [self.run_of(1, conclusion=conclusion)]
                self.assertIs(
                    merge_set.required_check_state(TEST_CHECK, runs, []),
                    CheckState.PASSED,
                )

    def test_an_unfinished_check_run_is_running(self):
        runs = [self.run_of(1, status="in_progress", conclusion=None)]
        self.assertIs(
            merge_set.required_check_state(TEST_CHECK, runs, []), CheckState.RUNNING
        )

    def test_a_check_run_of_another_app_does_not_count(self):
        runs = [self.run_of(1, app_id=999)]
        self.assertIs(
            merge_set.required_check_state(TEST_CHECK, runs, []), CheckState.MISSING
        )

    def test_a_commit_status_answers_a_check_no_check_run_reports(self):
        check = RequiredCheck("legacy", None)
        for state, expected in (
            ("success", CheckState.PASSED),
            ("pending", CheckState.RUNNING),
            ("error", CheckState.FAILED),
        ):
            with self.subTest(state=state):
                statuses = [CommitStatus("legacy", state, "", None, "ci")]
                self.assertIs(
                    merge_set.required_check_state(check, [], statuses), expected
                )

    def test_the_gate_is_the_newest_merge_set_status_of_the_bot(self):
        statuses = [
            CommitStatus("merge-set", "success", "forged", None, "someone"),
            CommitStatus("merge-set", "pending", "part of a set", "u", BOT),
            CommitStatus("merge-set", "success", "alone", None, BOT),
        ]
        self.assertEqual(
            merge_set.current_gate(statuses, BOT), Gate("pending", "part of a set", "u")
        )
        self.assertIsNone(merge_set.current_gate([], BOT))

    def test_the_review_decision(self):
        self.assertIs(
            merge_set.parse_review_decision(None), ReviewDecision.NOT_REQUIRED
        )
        self.assertIs(
            merge_set.parse_review_decision("APPROVED"), ReviewDecision.APPROVED
        )
        with self.assertRaises(MergeSetError):
            merge_set.parse_review_decision("MAYBE")


# The id of the CI workflow in the cases, and of another workflow.
CI_WORKFLOW_ID = 4
OTHER_WORKFLOW_ID = 3


def run_item(
    run_id, status="completed", conclusion="success", workflow_id=CI_WORKFLOW_ID
):
    """A workflow run as the REST API lists it."""
    return {
        "id": run_id,
        "workflow_id": workflow_id,
        "html_url": f"u{run_id}",
        "status": status,
        "conclusion": conclusion,
    }


class CiRuns(unittest.TestCase):
    def latest(self, *runs):
        return merge_set.latest_ci_run(runs, frozenset({CI_WORKFLOW_ID}))

    def test_the_newest_run_of_the_head_decides(self):
        latest = self.latest(
            run_item(7, status="in_progress", conclusion=None),
            run_item(5, conclusion="failure"),
        )
        self.assertEqual(latest, merge_set.CiRun("u7", CheckState.RUNNING))

    def test_the_runs_of_other_workflows_do_not_count(self):
        latest = self.latest(
            run_item(5, conclusion="failure"),
            run_item(9, workflow_id=OTHER_WORKFLOW_ID),
        )
        self.assertEqual(latest, merge_set.CiRun("u5", CheckState.FAILED))

    def test_a_head_the_ci_has_not_started_on_has_no_ci_run(self):
        self.assertIsNone(self.latest())
        self.assertIsNone(self.latest(run_item(9, workflow_id=OTHER_WORKFLOW_ID)))

    def test_the_state_of_a_ci_run(self):
        cases = [
            *(
                (status, None, CheckState.RUNNING)
                for status in (
                    "requested",
                    "queued",
                    "pending",
                    "waiting",
                    "in_progress",
                )
            ),
            *(
                ("completed", conclusion, CheckState.PASSED)
                for conclusion in ("success", "neutral", "skipped")
            ),
            *(
                ("completed", conclusion, CheckState.FAILED)
                for conclusion in (
                    "failure",
                    "cancelled",
                    "timed_out",
                    "startup_failure",
                    "action_required",
                    "stale",
                )
            ),
        ]
        for status, conclusion, expected in cases:
            with self.subTest(status=status, conclusion=conclusion):
                run = run_item(1, status=status, conclusion=conclusion)
                self.assertIs(self.latest(run).state, expected)

    def test_the_ci_workflows_are_found_by_their_own_name(self):
        # A branch can add a second workflow of the name, which the
        # workflow_run trigger names too.
        workflows = [
            {"id": OTHER_WORKFLOW_ID, "name": "Merge set events"},
            {"id": CI_WORKFLOW_ID, "name": merge_set.CI_WORKFLOW},
            {"id": 5, "name": merge_set.CI_WORKFLOW},
        ]
        self.assertEqual(
            merge_set.ci_workflow_ids(NODES_HUB, workflows),
            frozenset({CI_WORKFLOW_ID, 5}),
        )

    def test_a_repository_without_a_ci_workflow_is_refused(self):
        workflows = [{"id": OTHER_WORKFLOW_ID, "name": "Merge set events"}]
        with self.assertRaises(MergeSetError) as refusal:
            merge_set.ci_workflow_ids(NODES_HUB, workflows)
        self.assertEqual(
            str(refusal.exception),
            "nodes-hub has no workflow named Tests: the merge-set bot reads the CI "
            "of a pull request from it",
        )


class SetRecords(unittest.TestCase):
    def test_a_record_of_a_dev_build_names_the_peppy_branch(self):
        text = set_record_text(
            {"nodes-hub": {"ref": "checkout", "commit": commit(NODES_HUB)}},
            peppy={
                "build": "dev-build",
                "version": "peppy dev-aaaaaaaaaaaa",
                "source": "https://github.com/Peppy-bot/peppy/actions/runs/7",
                "ref": SET_NAME,
                "commit": commit(PEPPY),
            },
        )
        tested = merge_set.parse_tested_set(text)
        self.assertEqual(tested.peppy, TestedRef(SET_NAME, commit(PEPPY)))
        self.assertEqual(
            tested.hubs["nodes-hub"], TestedRef("checkout", commit(NODES_HUB))
        )

    def test_a_record_of_any_other_peppy_names_none(self):
        tested = merge_set.parse_tested_set(set_record_text({}))
        self.assertIsNone(tested.peppy)

    def test_a_malformed_record_is_refused(self):
        for text in (
            "not json",
            json.dumps({"hubs": {}}),
            set_record_text({"nodes-hub": {"ref": "main", "commit": "short"}}),
            set_record_text({"nodes-hub": "main"}),
        ):
            with self.subTest(text=text):
                with self.assertRaises(MergeSetError):
                    merge_set.parse_tested_set(text)

    def test_the_record_is_read_from_the_artifact_zip(self):
        self.assertEqual(merge_set.set_record_of_bundle(record_bundle("{}")), "{}")
        with self.assertRaises(MergeSetError):
            merge_set.set_record_of_bundle(record_bundle("{}", name="other.json"))

    def test_the_latest_run_of_each_workflow_counts(self):
        runs = [
            {"id": 5, "workflow_id": 1},
            {"id": 9, "workflow_id": 2},
            {"id": 7, "workflow_id": 1},
        ]
        self.assertEqual(
            [run["id"] for run in merge_set.latest_run_of_each_workflow(runs)], [7, 9]
        )

    def test_each_latest_job_finds_the_record_named_after_its_check_run(self):
        artifacts = [
            merge_set.Artifact("hub-ci-set-100", False, "u100"),
            merge_set.Artifact("hub-ci-set-200", True, "u200"),
            merge_set.Artifact("peppy-dev-x86_64-unknown-linux-gnu", False, "u"),
        ]
        # Job 300 re-ran job 100 in a later attempt: only the latest job ids
        # are listed, so the record of the first attempt no longer counts.
        jobs = [{"id": 200, "name": "check-index"}, {"id": 300, "name": "discover"}]
        self.assertEqual(
            merge_set.recorded_jobs(jobs, artifacts), [("check-index", artifacts[1])]
        )


class Freshness(unittest.TestCase):
    def state(self):
        return set_state(
            member(PEPPY, pull_request(PEPPY)),
            member(NODES_HUB, pull_request(NODES_HUB)),
            member(CONTRACTS_HUB, pull_request(CONTRACTS_HUB)),
        )

    def fresh_record(self):
        return tested_set(
            hubs={
                "nodes-hub": TestedRef("checkout", commit(NODES_HUB)),
                "contracts-hub": TestedRef(SET_NAME, commit(CONTRACTS_HUB)),
                "launchers-hub": TestedRef("main", commit(LAUNCHERS_HUB, "main")),
            },
            peppy=TestedRef(SET_NAME, commit(PEPPY)),
        )

    def stale_of(self, tested):
        run = hub_run(NODES_HUB, records=[JobRecord("check-index", tested)])
        return merge_set.stale_jobs(self.state(), [run], no_clean_base_merge)

    def test_a_job_that_ran_with_every_head_of_the_set_is_fresh(self):
        self.assertEqual(self.stale_of(self.fresh_record()), [])

    def test_a_job_that_ran_with_an_older_commit_of_a_branch_is_stale(self):
        tested = self.fresh_record()
        old = TestedRef(SET_NAME, commit(CONTRACTS_HUB, "old"))
        stale = self.stale_of(
            replace(tested, hubs={**tested.hubs, "contracts-hub": old})
        )
        self.assertEqual(len(stale), 1)
        self.assertEqual(
            stale[0].mismatches,
            (merge_set.Mismatch(CONTRACTS_HUB, old, commit(CONTRACTS_HUB)),),
        )

    def test_a_job_that_ran_a_commit_its_head_merges_its_base_into_is_fresh(self):
        tested = self.fresh_record()
        old = TestedRef(SET_NAME, commit(CONTRACTS_HUB, "old"))
        run = hub_run(
            NODES_HUB,
            records=[
                JobRecord(
                    "check-index",
                    replace(tested, hubs={**tested.hubs, "contracts-hub": old}),
                )
            ],
        )
        asked = []

        def is_clean_base_merge(mismatch):
            asked.append(mismatch)
            return True

        self.assertEqual(
            merge_set.stale_jobs(self.state(), [run], is_clean_base_merge), []
        )
        # Only the branch that moved since the job ran is checked.
        self.assertEqual(
            asked, [merge_set.Mismatch(CONTRACTS_HUB, old, commit(CONTRACTS_HUB))]
        )

    def test_a_job_that_ran_before_the_branch_existed_is_stale(self):
        tested = self.fresh_record()
        at_main = TestedRef("main", commit(CONTRACTS_HUB, "main"))
        stale = self.stale_of(
            replace(tested, hubs={**tested.hubs, "contracts-hub": at_main})
        )
        self.assertEqual(stale[0].mismatches[0].tested, at_main)

    def test_a_job_that_ran_main_at_the_commit_of_the_branch_is_fresh(self):
        # A branch made from main that has no commit of its own yet.
        tested = self.fresh_record()
        at_main = TestedRef("main", commit(CONTRACTS_HUB))
        self.assertEqual(
            self.stale_of(
                replace(tested, hubs={**tested.hubs, "contracts-hub": at_main})
            ),
            [],
        )

    def test_a_job_that_ran_an_older_peppy_dev_build_is_stale(self):
        tested = replace(
            self.fresh_record(), peppy=TestedRef("dev", commit(PEPPY, "dev"))
        )
        stale = self.stale_of(tested)
        self.assertEqual(stale[0].mismatches[0].repository, PEPPY)

    def test_a_repository_the_job_did_not_read_does_not_make_it_stale(self):
        state = set_state(
            member(NODES_HUB, pull_request(NODES_HUB)),
            member(PRIVATE_NODES_HUB, pull_request(PRIVATE_NODES_HUB)),
        )
        tested = tested_set(
            hubs={"nodes-hub": TestedRef("checkout", commit(NODES_HUB))}
        )
        run = hub_run(NODES_HUB, records=[JobRecord("check-index", tested)])
        self.assertEqual(merge_set.stale_jobs(state, [run], no_clean_base_merge), [])

    def test_an_expired_record_is_stale(self):
        stale = self.stale_of(None)
        self.assertTrue(stale[0].record_expired)
        self.assertIn(
            "expired set record", merge_set.stale_job_text("nodes-hub#1", stale[0])
        )

    def test_each_stale_run_is_named_once(self):
        tested = replace(
            self.fresh_record(), peppy=TestedRef("dev", commit(PEPPY, "dev"))
        )
        run = hub_run(
            NODES_HUB,
            records=[JobRecord("check-index", tested), JobRecord("discover", tested)],
        )
        nodes = pull_request(NODES_HUB)
        reports = {
            nodes.label: report(
                nodes,
                stale=tuple(
                    merge_set.stale_jobs(self.state(), [run], no_clean_base_merge)
                ),
            )
        }
        self.assertEqual(merge_set.stale_runs(reports), [run])


class Blockers(unittest.TestCase):
    def two_member_state(self, nodes=None):
        return set_state(
            member(PEPPY, pull_request(PEPPY)),
            member(NODES_HUB, nodes or pull_request(NODES_HUB)),
        )

    def reports(self, nodes_report):
        peppy = pull_request(PEPPY)
        return {
            peppy.label: report(peppy),
            nodes_report.pull_request.label: nodes_report,
        }

    def test_a_set_ready_to_merge_has_no_blocker(self):
        state = self.two_member_state()
        self.assertEqual(
            blocker_texts(state, self.reports(report(pull_request(NODES_HUB)))), []
        )

    def test_each_reason_a_pull_request_cannot_merge(self):
        stale = merge_set.StaleJob(
            hub_run(NODES_HUB),
            "check-index",
            (
                merge_set.Mismatch(
                    PEPPY, TestedRef(SET_NAME, commit(PEPPY, "old")), commit(PEPPY)
                ),
            ),
            record_expired=False,
        )
        cases = [
            (
                {"pull_request": pull_request(NODES_HUB, base_branch="dev")},
                "targets `dev`: retarget it onto `main`",
            ),
            ({"pull_request": pull_request(NODES_HUB, draft=True)}, "is a draft"),
            ({"mergeable": False}, "has conflicts with `main`"),
            ({"mergeable": None}, "has not yet computed"),
            ({"behind": True}, "is behind `main`"),
            ({"review": ReviewDecision.REVIEW_REQUIRED}, "needs an approving review"),
            ({"review": ReviewDecision.CHANGES_REQUESTED}, "requested changes"),
            (
                {"checks": ((TEST_CHECK, CheckState.FAILED),)},
                "`test` of nodes-hub#1 failed",
            ),
            (
                {"checks": ((TEST_CHECK, CheckState.RUNNING),)},
                "`test` of nodes-hub#1 is running",
            ),
            ({"checks": ((TEST_CHECK, CheckState.MISSING),)}, "has not reported yet"),
            ({"stale": (stale,)}, "ran with peppy at `feat/gripper-force`"),
        ]
        for changes, expected in cases:
            with self.subTest(expected=expected):
                nodes_report = report(
                    changes.get("pull_request", pull_request(NODES_HUB)),
                    **{
                        key: value
                        for key, value in changes.items()
                        if key != "pull_request"
                    },
                )
                texts = blocker_texts(
                    self.two_member_state(nodes_report.pull_request),
                    self.reports(nodes_report),
                )
                self.assertEqual(len(texts), 1, texts)
                self.assertIn(expected, texts[0])

    def test_only_a_missing_approval_is_waived_for_an_admin(self):
        unapproved = report(
            pull_request(NODES_HUB), review=ReviewDecision.REVIEW_REQUIRED
        )
        (missing,) = merge_set.set_blockers(
            self.two_member_state(), self.reports(unapproved)
        )
        self.assertTrue(missing.waived_for_admins)
        self.assertEqual(merge_set.blockers_for([missing], admin=True), [])
        self.assertEqual(merge_set.blockers_for([missing], admin=False), [missing])
        refused = report(
            pull_request(NODES_HUB), review=ReviewDecision.CHANGES_REQUESTED
        )
        (changes,) = merge_set.set_blockers(
            self.two_member_state(), self.reports(refused)
        )
        self.assertFalse(changes.waived_for_admins)
        self.assertEqual(merge_set.blockers_for([changes], admin=True), [changes])

    def test_an_approval_the_app_cannot_bypass_is_not_waived_for_an_admin(self):
        for rulesets, expected in (
            (("Protect main and dev",), "of the ruleset `Protect main and dev`."),
            (("A", "B"), "of the rulesets `A`, `B`."),
        ):
            with self.subTest(rulesets=rulesets):
                unapproved = report(
                    pull_request(NODES_HUB),
                    review=ReviewDecision.REVIEW_REQUIRED,
                    unbypassed_approval_rulesets=rulesets,
                )
                (missing,) = merge_set.set_blockers(
                    self.two_member_state(), self.reports(unapproved)
                )
                self.assertFalse(missing.waived_for_admins)
                self.assertEqual(
                    missing.text,
                    "nodes-hub#1 needs an approving review. No admin merges it "
                    "without one: the merge-set App is not a bypass actor " + expected,
                )

    def test_a_review_no_rule_asks_for_does_not_block(self):
        nodes_report = report(
            pull_request(NODES_HUB), review=ReviewDecision.NOT_REQUIRED
        )
        self.assertEqual(
            blocker_texts(self.two_member_state(), self.reports(nodes_report)), []
        )

    def test_a_branch_without_a_pull_request_blocks_the_set(self):
        state = set_state(member(PEPPY, pull_request(PEPPY)), member(CONTRACTS_HUB))
        peppy = pull_request(PEPPY)
        texts = blocker_texts(state, {peppy.label: report(peppy)})
        self.assertEqual(len(texts), 1)
        self.assertIn("contracts-hub has a change on the branch", texts[0])
        self.assertIn("no open pull request", texts[0])

    def test_several_open_pull_requests_from_the_branch_block_the_set(self):
        state = set_state(
            member(PEPPY, pull_request(PEPPY)),
            member(NODES_HUB, pull_request(NODES_HUB, 1), pull_request(NODES_HUB, 2)),
        )
        peppy = pull_request(PEPPY)
        texts = blocker_texts(state, {peppy.label: report(peppy)})
        self.assertIn("more than one open pull request", texts[0])
        self.assertIn("nodes-hub#1, nodes-hub#2", texts[0])


class Access(unittest.TestCase):
    def setUp(self):
        self.peppy = pull_request(PEPPY, 512)
        self.nodes = pull_request(NODES_HUB, 88)
        self.state = set_state(member(PEPPY, self.peppy), member(NODES_HUB, self.nodes))

    @staticmethod
    def permissions(peppy="write", nodes="write"):
        return {PEPPY: peppy, NODES_HUB: nodes}

    def test_the_author_of_every_pull_request_with_write_access_is_not_refused(self):
        for permission in sorted(merge_set.WRITE_PERMISSIONS):
            with self.subTest(permission=permission):
                self.assertIsNone(
                    merge_set.access_refusal(
                        self.state, "alice", self.permissions(permission, permission)
                    )
                )

    def test_an_admin_of_every_repository_who_opened_no_pull_request_is_not_refused(
        self,
    ):
        self.assertIsNone(
            merge_set.access_refusal(
                self.state, "bob", self.permissions("admin", "admin")
            )
        )

    def test_a_user_with_write_access_who_opened_no_pull_request_is_refused(self):
        self.assertEqual(
            merge_set.access_refusal(self.state, "bob", self.permissions()),
            AccessRefusal(not_opened=(self.peppy, self.nodes), no_write_access=()),
        )

    def test_the_author_of_some_pull_requests_of_the_set_alone_is_refused(self):
        nodes = replace(self.nodes, author="bob")
        state = set_state(member(PEPPY, self.peppy), member(NODES_HUB, nodes))
        self.assertEqual(
            merge_set.access_refusal(state, "alice", self.permissions()),
            AccessRefusal(not_opened=(nodes,), no_write_access=()),
        )
        self.assertEqual(
            merge_set.access_refusal(state, "bob", self.permissions()),
            AccessRefusal(not_opened=(self.peppy,), no_write_access=()),
        )

    def test_an_admin_of_some_repositories_alone_is_refused_what_they_did_not_open(
        self,
    ):
        self.assertEqual(
            merge_set.access_refusal(
                self.state, "bob", self.permissions("admin", "write")
            ),
            AccessRefusal(not_opened=(self.peppy, self.nodes), no_write_access=()),
        )
        self.assertIsNone(
            merge_set.access_refusal(
                self.state, "alice", self.permissions("admin", "write")
            )
        )

    def test_the_author_without_write_access_to_a_repository_is_refused(self):
        for permission in ("triage", "read", "none"):
            with self.subTest(permission=permission):
                self.assertEqual(
                    merge_set.access_refusal(
                        self.state, "alice", self.permissions(nodes=permission)
                    ),
                    AccessRefusal(not_opened=(), no_write_access=(NODES_HUB,)),
                )

    def test_a_pull_request_without_a_named_author_leaves_the_set_to_an_admin(self):
        nodes = replace(self.nodes, author=None)
        state = set_state(member(PEPPY, self.peppy), member(NODES_HUB, nodes))
        self.assertEqual(
            merge_set.access_refusal(state, "alice", self.permissions()),
            AccessRefusal(not_opened=(nodes,), no_write_access=()),
        )
        self.assertIsNone(
            merge_set.access_refusal(state, "bob", self.permissions("admin", "admin"))
        )

    def test_every_open_pull_request_of_a_member_has_to_be_the_users(self):
        other = pull_request(NODES_HUB, 89, author="bob")
        state = set_state(
            member(PEPPY, self.peppy), member(NODES_HUB, self.nodes, other)
        )
        self.assertEqual(
            merge_set.access_refusal(state, "alice", self.permissions()),
            AccessRefusal(not_opened=(other,), no_write_access=()),
        )

    def test_a_refusal_names_each_reason_and_who_the_bot_acts_for(self):
        refusals = {
            "bob": AccessRefusal((self.peppy, self.nodes), (NODES_HUB,)),
            "carol": AccessRefusal((), (PEPPY,)),
        }
        self.assertEqual(
            merge_set.render_access_refusal(Action.RERUN, refusals),
            "@bob, the bot did not re-run the CI of the set for you: you did not "
            "open peppy#512, nodes-hub#88, and you have no write access to "
            f"nodes-hub. It does that only for {merge_set.WHO_MAY_ASK}.\n\n"
            "@carol, the bot did not re-run the CI of the set for you: you have "
            f"no write access to peppy. It does that only for {merge_set.WHO_MAY_ASK}.\n",
        )


class Dashboards(unittest.TestCase):
    def test_a_ready_set_lists_every_member_and_offers_the_merge(self):
        peppy, nodes = pull_request(PEPPY, 512), pull_request(NODES_HUB, 88)
        state = set_state(member(PEPPY, peppy), member(NODES_HUB, nodes))
        body = merge_set.render_set_dashboard(
            state, reports_by_label(report(peppy), report(nodes))
        )
        self.assertTrue(body.startswith(merge_set.DASHBOARD_MARKER))
        self.assertIn("| Repository | Pull request | Head | CI | State |", body)
        self.assertIn(
            f"| peppy | [#512](https://github.com/Peppy-bot/peppy/pull/512) "
            f"| `{commit(PEPPY)[:7]}` | [passed]({run_url(PEPPY, 10)}) | ready |",
            body,
        )
        self.assertIn("**Ready to merge.**", body)
        self.assertIn(
            "- [ ] <!-- merge-set:merge --> Merge the 2 pull requests of this set together",
            body,
        )
        self.assertIn(
            f"The bot acts on a ticked box only for {merge_set.WHO_MAY_ASK}.", body
        )
        self.assertNotIn("merge-set:rerun", body)
        self.assertEqual(merge_set.ticked_actions(body), [])

    def test_each_member_shows_the_ci_of_its_head(self):
        peppy, nodes, contracts = (
            pull_request(PEPPY, 512),
            pull_request(NODES_HUB, 88),
            pull_request(CONTRACTS_HUB, 9),
        )
        state = set_state(
            member(PEPPY, peppy),
            member(NODES_HUB, nodes),
            member(CONTRACTS_HUB, contracts),
            member(LAUNCHERS_HUB),
        )
        running = ci_run_of(NODES_HUB, CheckState.RUNNING, 11)
        failed = ci_run_of(CONTRACTS_HUB, CheckState.FAILED, 12)
        body = merge_set.render_set_dashboard(
            state,
            reports_by_label(
                report(peppy, ci=None),
                report(nodes, ci=running),
                report(contracts, ci=failed),
            ),
        )
        self.assertIn(f"| `{commit(PEPPY)[:7]}` | not started |", body)
        self.assertIn(f"| `{commit(NODES_HUB)[:7]}` | [running]({running.url}) |", body)
        self.assertIn(
            f"| `{commit(CONTRACTS_HUB)[:7]}` | [failed]({failed.url}) |", body
        )
        # The Pull request cell of a member without one says why it has no CI.
        self.assertIn(
            f"| launchers-hub | no open pull request | `{commit(LAUNCHERS_HUB)[:7]}` "
            "|  | blocked |",
            body,
        )

    def test_a_blocked_set_names_what_blocks_it_and_offers_the_re_run(self):
        peppy, nodes = pull_request(PEPPY), pull_request(NODES_HUB)
        state = set_state(
            member(PEPPY, peppy),
            member(NODES_HUB, nodes),
            member(CONTRACTS_HUB),
            left_behind=(LAUNCHERS_HUB,),
        )
        stale = merge_set.StaleJob(
            hub_run(NODES_HUB), "check-index", (), record_expired=True
        )
        body = merge_set.render_set_dashboard(
            state, reports_by_label(report(peppy), report(nodes, stale=(stale,)))
        )
        self.assertIn(
            f"| `{commit(NODES_HUB)[:7]}` | [passed]({run_url(NODES_HUB, 10)}) "
            "| CI out of date |",
            body,
        )
        self.assertIn("| contracts-hub | no open pull request |", body)
        self.assertIn(
            "**What blocks the merge**\n\n"
            f"- {merge_set.stale_job_text(nodes.label, stale)}\n"
            "- contracts-hub has a change on the branch",
            body,
        )
        self.assertIn("launchers-hub still has the branch", body)
        self.assertIn(
            "<!-- merge-set:rerun --> Re-run the out-of-date CI of this set (1 run)",
            body,
        )

    def test_a_set_that_lacks_approvals_alone_is_ready_for_an_admin(self):
        peppy, nodes = pull_request(PEPPY, 512), pull_request(NODES_HUB, 88)
        state = set_state(member(PEPPY, peppy), member(NODES_HUB, nodes))
        body = merge_set.render_set_dashboard(
            state,
            reports_by_label(
                report(peppy),
                report(nodes, review=ReviewDecision.REVIEW_REQUIRED),
            ),
        )
        self.assertIn("**Ready to merge for an admin.**", body)
        self.assertIn(merge_set.ADMIN_WAIVER, body)
        self.assertIn("- nodes-hub#88 needs an approving review.", body)
        self.assertIn(f"| [passed]({run_url(NODES_HUB, 10)}) | not approved |", body)
        self.assertIn(f"| [passed]({run_url(PEPPY, 10)}) | ready |", body)
        self.assertNotIn("What blocks the merge", body)

    def test_a_blocked_set_names_its_missing_approvals_apart(self):
        peppy = pull_request(PEPPY, 512, draft=True)
        nodes = pull_request(NODES_HUB, 88)
        state = set_state(member(PEPPY, peppy), member(NODES_HUB, nodes))
        body = merge_set.render_set_dashboard(
            state,
            reports_by_label(
                report(peppy),
                report(nodes, review=ReviewDecision.REVIEW_REQUIRED),
            ),
        )
        self.assertIn(
            "**What blocks the merge**\n\n- peppy#512 is a draft: mark it ready for review.\n\n"
            f"Approvals are missing too. {merge_set.ADMIN_WAIVER}\n\n"
            "- nodes-hub#88 needs an approving review.",
            body,
        )
        self.assertIn("| blocked |", body)
        self.assertIn("| not approved |", body)

    def test_the_state_of_a_member_names_a_re_run_when_it_alone_clears_it(self):
        stale = merge_set.StaleJob(
            hub_run(NODES_HUB), "check-index", (), record_expired=True
        )
        nodes = pull_request(NODES_HUB)
        cases = [
            ({}, "ready"),
            ({"review": ReviewDecision.REVIEW_REQUIRED}, "not approved"),
            ({"stale": (stale,)}, "CI out of date"),
            (
                {"stale": (stale,), "review": ReviewDecision.REVIEW_REQUIRED},
                "CI out of date",
            ),
            (
                {"stale": (stale,), "checks": ((TEST_CHECK, CheckState.FAILED),)},
                "blocked",
            ),
            ({"mergeable": False}, "blocked"),
        ]
        for changes, expected in cases:
            with self.subTest(changes=changes):
                blockers = merge_set.pull_request_blockers(report(nodes, **changes))
                self.assertEqual(merge_set.member_state(NODES_HUB, blockers), expected)
        # The blockers of another member leave this one ready.
        peppy_blockers = merge_set.pull_request_blockers(
            report(pull_request(PEPPY), mergeable=False)
        )
        self.assertEqual(merge_set.member_state(NODES_HUB, peppy_blockers), "ready")

    def test_the_ticked_boxes_are_read_by_their_marker(self):
        body = (
            "- [x] <!-- merge-set:merge --> Merge the 2 pull requests\n"
            "- [X] <!-- merge-set:rerun --> any text\n"
            "- [x] <!-- merge-set:unknown --> ignored\n"
            "- [ ] <!-- merge-set:merge --> unticked\n"
            "- [x] merge-set:merge without a marker\n"
        )
        self.assertEqual(merge_set.ticked_actions(body), [Action.MERGE, Action.RERUN])

    def test_only_a_user_counts_as_the_one_who_ticked(self):
        self.assertEqual(
            merge_set.parse_editor(
                {"editor": {"__typename": "User", "login": "alice"}}
            ),
            "alice",
        )
        self.assertIsNone(
            merge_set.parse_editor(
                {"editor": {"__typename": "Bot", "login": "peppy-merge-set"}}
            )
        )
        self.assertIsNone(merge_set.parse_editor({"editor": None}))
        self.assertIsNone(merge_set.parse_editor(None))

    def test_a_status_description_fits_what_github_keeps(self):
        gate = merge_set.member_gate("feat/" + "x" * 200, "https://example.com")
        self.assertEqual(len(gate.description), merge_set.STATUS_DESCRIPTION_LIMIT)
        self.assertTrue(gate.description.endswith("…"))
        self.assertEqual(
            merge_set.alone_gate(SET_NAME).description,
            f"Alone: no other repository has a change on the branch {SET_NAME}",
        )


class MergeMessages(unittest.TestCase):
    def test_a_merge_commit_names_the_other_pull_requests_of_the_set(self):
        peppy, nodes = pull_request(PEPPY, 512), pull_request(NODES_HUB, 88)
        self.assertEqual(
            merge_set.merge_commit_message(SET_NAME, nodes, [peppy, nodes]),
            f"Change nodes-hub\n\nMerged together with the set `{SET_NAME}`: "
            "Peppy-bot/peppy#512.",
        )

    def test_the_report_of_a_merge_that_stopped(self):
        peppy, nodes, contracts = (
            pull_request(PEPPY, 512),
            pull_request(NODES_HUB, 88),
            pull_request(CONTRACTS_HUB, 9),
        )
        outcome = merge_set.MergeOutcome(
            merged=(merge_set.MergedPullRequest(peppy, "a" * 40),),
            failed=nodes,
            failure="Base branch was modified.",
            not_tried=(contracts,),
        )
        text = merge_set.render_merge_report(SET_NAME, ["alice"], outcome, [])
        self.assertIn("which @alice asked for, stopped", text)
        self.assertIn("- peppy#512 merged as `aaaaaaa`.", text)
        self.assertIn("- nodes-hub#88 did not merge: Base branch was modified.", text)
        self.assertIn("- contracts-hub#9 was not tried.", text)
        self.assertEqual(outcome.unmerged, (nodes, contracts))


class FakeComment:
    def __init__(self, comment_id, pull_request, author, body):
        self.id = comment_id
        self.pull_request = pull_request
        self.author = author
        self.body = body
        self.editor = None

    @property
    def url(self):
        return f"{self.pull_request.url}#issuecomment-{self.id}"

    def dashboard(self):
        return merge_set.Dashboard(
            self.pull_request, self.id, f"node-{self.id}", self.url, self.body
        )


class FakeGitHub:
    """GitHub for the sync, with the methods of GitHubGateway. A test sets up
    branches, pull requests, checks, reviews, runs and comments, and reads
    back what the bot wrote."""

    def __init__(self, test):
        self.test = test
        self.bot_login = BOT
        self.heads = {}
        self.pulls = {repository.name: [] for repository in merge_set.REPOSITORIES}
        self.rules = {repository.name: RULES for repository in merge_set.REPOSITORIES}
        # The rulesets the App does not bypass, by repository name and id.
        self.unbypassed = set()
        self.check_runs_of = {}
        # The CI run of each pull request, by label; None before it starts.
        self.ci_runs = {}
        self.statuses_of = {}
        self.reviews = {}
        # The answers of merge_state for a pull request, in order; the last
        # one repeats.
        self.merge_states = {}
        self.behind = set()
        # The (repository name, tested commit) pairs whose branch head differs
        # from that commit by a clean merge of its base alone.
        self.clean_base_merges = set()
        self.runs = {}
        self.comments = []
        self.permissions = {}
        self.merge_refusals = {}
        # Edits made while the sync runs, by comment id: someone ticks a box
        # after the sync read the comment.
        self.edits_during_the_run = {}
        # Every read of a branch fails once a merge happened: GitHub fails
        # right after the merge of a set stopped.
        self.fail_reads_after_a_merge = False
        self.merged = []
        self.reruns = []
        self.sleeps = []

    # Setting up.

    def open_pull_request(self, repository, number=1, **changes):
        pr = pull_request(repository, number, **changes)
        self.heads[(repository.name, pr.head_branch)] = pr.head_commit
        self.pulls[repository.name].append(pr)
        self.check_runs_of[(repository.name, pr.head_commit)] = [
            CheckRun(1, "test", 15368, True, "success")
        ]
        self.ci_runs[pr.label] = ci_run_of(repository)
        return pr

    def make_admin(self, login, repositories=merge_set.REPOSITORIES):
        for repository in repositories:
            self.permissions[(repository.name, login)] = "admin"

    def set_author(self, pr, login):
        """The pull request as `login` opened it."""
        pulls = self.pulls[pr.repository.name]
        opened = replace(pr, author=login)
        pulls[pulls.index(pr)] = opened
        return opened

    def dashboard_comment(self, pr, body):
        comment = FakeComment(len(self.comments) + 1, pr, BOT, body)
        self.comments.append(comment)
        return comment

    def tick(self, comment, action, login="alice"):
        marker = f"- [ ] <!-- merge-set:{action.value} -->"
        self.test.assertIn(marker, comment.body)
        comment.body = comment.body.replace(marker, marker.replace("[ ]", "[x]"))
        comment.editor = login

    # Reading back.

    def gates(self, pr):
        return [
            Gate(status.state, status.description, status.target_url)
            for status in reversed(
                self.statuses_of.get((pr.repository.name, pr.head_commit), [])
            )
            if status.context == merge_set.STATUS_CONTEXT
        ]

    def comments_on(self, pr):
        return [
            comment
            for comment in self.comments
            if comment.pull_request.label == pr.label
        ]

    def reports_on(self, pr):
        return [
            comment.body
            for comment in self.comments_on(pr)
            if not comment.body.startswith(merge_set.DASHBOARD_MARKER)
        ]

    def dashboard_body(self, pr):
        dashboards = [
            comment
            for comment in self.comments_on(pr)
            if comment.body.startswith(merge_set.DASHBOARD_MARKER)
        ]
        self.test.assertEqual(len(dashboards), 1, dashboards)
        return dashboards[0].body

    # GitHubGateway.

    def branch_head(self, repository, branch):
        if self.fail_reads_after_a_merge and self.merged:
            raise ApiError("GET branch", 502, "Bad Gateway")
        return self.heads.get((repository.name, branch))

    def pull_requests_from(self, repository, branch):
        return [pr for pr in self.pulls[repository.name] if pr.head_branch == branch]

    def pull_request(self, repository, number):
        return next(pr for pr in self.pulls[repository.name] if pr.number == number)

    def merge_state(self, pr):
        answers = self.merge_states.get(pr.label, [(True, "clean")])
        if len(answers) > 1:
            return answers.pop(0)
        return answers[0]

    def branch_rules(self, repository):
        return self.rules[repository.name]

    def ruleset_bypass(self, repository, ruleset_id):
        return merge_set.RulesetBypass(
            f"Ruleset {ruleset_id}",
            (repository.name, ruleset_id) not in self.unbypassed,
        )

    def check_runs(self, repository, sha):
        return self.check_runs_of.get((repository.name, sha), [])

    def statuses(self, repository, sha):
        return list(self.statuses_of.get((repository.name, sha), []))

    def ci_run(self, pr):
        return self.ci_runs.get(pr.label)

    def review_decision(self, pr):
        return self.reviews.get(pr.label, ReviewDecision.APPROVED)

    def is_behind(self, pr):
        return pr.label in self.behind

    def is_clean_base_merge(self, mismatch):
        return (
            mismatch.repository.name,
            mismatch.tested.commit,
        ) in self.clean_base_merges

    def hub_runs(self, pr):
        return self.runs.get(pr.label, [])

    def gate(self, pr):
        return merge_set.current_gate(self.statuses(pr.repository, pr.head_commit), BOT)

    def post_gate(self, pr, gate):
        key = (pr.repository.name, pr.head_commit)
        status = CommitStatus(
            merge_set.STATUS_CONTEXT, gate.state, gate.description, gate.target_url, BOT
        )
        self.statuses_of[key] = [status, *self.statuses_of.get(key, [])]

    def dashboard(self, pr):
        for comment in self.comments_on(pr):
            if comment.author == BOT and comment.body.startswith(
                merge_set.DASHBOARD_MARKER
            ):
                return comment.dashboard()
        return None

    def comment_by_id(self, comment_id):
        return next(comment for comment in self.comments if comment.id == comment_id)

    def comment_body(self, dashboard):
        comment = self.comment_by_id(dashboard.id)
        if comment.id in self.edits_during_the_run:
            comment.body = self.edits_during_the_run.pop(comment.id)
        return comment.body

    def comment_editor(self, dashboard):
        return self.comment_by_id(dashboard.id).editor

    def create_comment(self, pr, body):
        comment = FakeComment(len(self.comments) + 1, pr, BOT, body)
        self.comments.append(comment)
        return comment.url

    def update_comment(self, dashboard, body):
        comment = self.comment_by_id(dashboard.id)
        comment.body = body
        comment.editor = None

    def permission(self, repository, login):
        return self.permissions.get((repository.name, login), "write")

    def merge(self, pr, message):
        self.test.assertEqual(
            self.gate(pr).state, "success", f"{pr.label} merged while blocked"
        )
        if pr.label in self.merge_refusals:
            raise MergeRefused(self.merge_refusals[pr.label])
        self.merged.append((pr.label, message))
        pulls = self.pulls[pr.repository.name]
        pulls[pulls.index(pr)] = replace(pr, is_open=False, merged=True)
        # GitHub deletes the branch of a merged pull request.
        del self.heads[(pr.repository.name, pr.head_branch)]
        return f"{pr.repository.name}-merge".encode().hex()[:40].ljust(40, "0")

    def rerun(self, run):
        self.reruns.append((run.repository.name, run.id))

    def sleep(self, seconds):
        self.sleeps.append(seconds)


def sync(
    github, repository=NODES_HUB, branch=SET_NAME, head_repository=None, pull_request=""
):
    event = merge_set.parse_event(
        repository.name,
        branch,
        head_repository or repository.full_name,
        str(pull_request),
    )
    merge_set.sync(github, event, CONTEXT)


class SyncAlone(unittest.TestCase):
    def test_a_pull_request_alone_gets_a_green_gate_and_no_comment(self):
        github = FakeGitHub(self)
        nodes = github.open_pull_request(NODES_HUB)
        sync(github)
        self.assertEqual(github.gates(nodes), [merge_set.alone_gate(SET_NAME)])
        self.assertEqual(github.comments, [])

    def test_a_second_sync_writes_nothing_new(self):
        github = FakeGitHub(self)
        nodes = github.open_pull_request(NODES_HUB)
        sync(github)
        sync(github)
        self.assertEqual(len(github.gates(nodes)), 1)

    def test_a_pull_request_left_alone_by_its_set_says_so(self):
        github = FakeGitHub(self)
        nodes = github.open_pull_request(NODES_HUB)
        old = github.dashboard_comment(nodes, f"{merge_set.DASHBOARD_MARKER}\nold set")
        sync(github)
        self.assertEqual(old.body, merge_set.render_alone_dashboard(SET_NAME))
        self.assertEqual(github.gates(nodes)[-1], merge_set.alone_gate(SET_NAME))

    def test_a_box_ticked_on_a_pull_request_now_alone_merges_nothing(self):
        github = FakeGitHub(self)
        nodes = github.open_pull_request(NODES_HUB)
        old = github.dashboard_comment(
            nodes,
            f"{merge_set.DASHBOARD_MARKER}\n"
            + merge_set.box_line(Action.MERGE, "Merge"),
        )
        github.tick(old, Action.MERGE)
        sync(github)
        self.assertEqual(github.merged, [])
        self.assertEqual(old.body, merge_set.render_alone_dashboard(SET_NAME))

    def test_a_pull_request_from_a_fork_is_not_part_of_a_set(self):
        github = FakeGitHub(self)
        fork = github.open_pull_request(NODES_HUB, 5, from_fork=True)
        sync(github, head_repository="someone/nodes-hub", pull_request=5)
        self.assertEqual(
            github.gates(fork),
            [Gate("success", "From a fork, so not part of a set", None)],
        )

    def test_a_pull_request_from_an_integration_branch_is_not_part_of_a_set(self):
        github = FakeGitHub(self)
        release = github.open_pull_request(
            PEPPY, 5, head_branch="dev", base_branch="main"
        )
        sync(github, repository=PEPPY, branch="dev", pull_request=5)
        self.assertEqual(
            github.gates(release),
            [Gate("success", "From dev, so not part of a set", None)],
        )

    def test_an_event_of_no_set_and_no_pull_request_does_nothing(self):
        github = FakeGitHub(self)
        sync(github, branch="main")
        self.assertEqual(github.statuses_of, {})


class SyncSet(unittest.TestCase):
    def setUp(self):
        self.github = FakeGitHub(self)
        self.peppy = self.github.open_pull_request(PEPPY, 512)
        self.nodes = self.github.open_pull_request(NODES_HUB, 88)

    def test_each_pull_request_of_a_set_gets_a_dashboard_and_a_pending_gate(self):
        sync(self.github)
        for pr in (self.peppy, self.nodes):
            with self.subTest(pr=pr.label):
                body = self.github.dashboard_body(pr)
                self.assertIn("**Ready to merge.**", body)
                (comment,) = self.github.comments_on(pr)
                self.assertEqual(
                    self.github.gates(pr),
                    [merge_set.member_gate(SET_NAME, comment.url)],
                )

    def test_a_second_sync_of_an_unchanged_set_writes_nothing(self):
        sync(self.github)
        bodies = [comment.body for comment in self.github.comments]
        sync(self.github)
        self.assertEqual([comment.body for comment in self.github.comments], bodies)
        self.assertEqual(len(self.github.gates(self.nodes)), 1)

    def test_a_pull_request_that_joins_a_set_loses_its_green_gate(self):
        self.github.pulls["peppy"].clear()
        del self.github.heads[("peppy", SET_NAME)]
        sync(self.github)
        self.assertEqual(self.github.gates(self.nodes)[-1].state, "success")
        self.github.open_pull_request(PEPPY, 512)
        sync(self.github, repository=PEPPY)
        self.assertEqual(self.github.gates(self.nodes)[-1].state, "pending")

    def test_the_dashboard_follows_what_blocks_the_set(self):
        self.github.reviews[self.nodes.label] = ReviewDecision.REVIEW_REQUIRED
        sync(self.github)
        self.assertIn(
            "nodes-hub#88 needs an approving review.",
            self.github.dashboard_body(self.peppy),
        )
        del self.github.reviews[self.nodes.label]
        sync(self.github)
        self.assertIn("**Ready to merge.**", self.github.dashboard_body(self.peppy))

    def test_the_dashboard_follows_the_ci_of_each_pull_request(self):
        self.github.ci_runs[self.peppy.label] = None
        running = ci_run_of(NODES_HUB, CheckState.RUNNING)
        self.github.ci_runs[self.nodes.label] = running
        sync(self.github)
        body = self.github.dashboard_body(self.nodes)
        self.assertIn(f"| `{self.peppy.head_commit[:7]}` | not started |", body)
        self.assertIn(
            f"| `{self.nodes.head_commit[:7]}` | [running]({running.url}) |", body
        )
        # The start of the peppy run and the end of the nodes-hub run each
        # start a sync, which writes every dashboard of the set again.
        self.github.ci_runs[self.peppy.label] = ci_run_of(PEPPY, CheckState.RUNNING)
        self.github.ci_runs[self.nodes.label] = replace(
            running, state=CheckState.FAILED
        )
        sync(self.github, repository=PEPPY)
        for pr in (self.peppy, self.nodes):
            with self.subTest(pr=pr.label):
                body = self.github.dashboard_body(pr)
                self.assertIn(
                    f"| `{self.peppy.head_commit[:7]}` "
                    f"| [running]({run_url(PEPPY, 10)}) |",
                    body,
                )
                self.assertIn(
                    f"| `{self.nodes.head_commit[:7]}` | [failed]({running.url}) |",
                    body,
                )

    def test_a_pull_request_behind_its_base_blocks_only_where_the_ruleset_is_strict(
        self,
    ):
        self.github.behind.add(self.nodes.label)
        sync(self.github)
        self.assertIn("**Ready to merge.**", self.github.dashboard_body(self.peppy))
        self.github.rules["nodes-hub"] = replace(RULES, strict=True)
        sync(self.github)
        self.assertIn(
            "nodes-hub#88 is behind `main`, and the ruleset of that branch requires it "
            "up to date: update the branch.",
            self.github.dashboard_body(self.peppy),
        )

    def test_the_bot_waits_while_github_computes_whether_a_pull_request_merges(self):
        self.github.merge_states[self.nodes.label] = [
            (None, "unknown"),
            (True, "blocked"),
        ]
        sync(self.github)
        self.assertEqual(self.github.sleeps, [merge_set.MERGE_STATE_INTERVAL_SECONDS])
        self.assertIn("**Ready to merge.**", self.github.dashboard_body(self.nodes))

    def test_a_dashboard_edited_during_the_run_is_left_for_the_next_sync(self):
        sync(self.github)
        self.github.reviews[self.nodes.label] = ReviewDecision.REVIEW_REQUIRED
        (comment,) = self.github.comments_on(self.peppy)
        ticked = comment.body.replace("- [ ]", "- [x]")
        self.github.edits_during_the_run[comment.id] = ticked
        sync(self.github)
        self.assertEqual(comment.body, ticked)


class SyncMerge(unittest.TestCase):
    def setUp(self):
        self.github = FakeGitHub(self)
        self.peppy = self.github.open_pull_request(PEPPY, 512)
        self.nodes = self.github.open_pull_request(NODES_HUB, 88)
        self.contracts = self.github.open_pull_request(CONTRACTS_HUB, 9)
        sync(self.github)

    def tick_merge(self, pr=None, login="alice"):
        (comment,) = self.github.comments_on(pr or self.nodes)
        self.github.tick(comment, Action.MERGE, login)
        sync(self.github)

    def test_a_ready_set_merges_in_order_with_a_report_on_each_pull_request(self):
        self.tick_merge()
        self.assertEqual(
            [label for label, _ in self.github.merged],
            ["peppy#512", "nodes-hub#88", "contracts-hub#9"],
        )
        self.assertIn(
            "Merged together with the set `feat/gripper-force`: Peppy-bot/nodes-hub#88, "
            "Peppy-bot/contracts-hub#9.",
            self.github.merged[0][1],
        )
        for pr in (self.peppy, self.nodes, self.contracts):
            with self.subTest(pr=pr.label):
                (report_text,) = self.github.reports_on(pr)
                self.assertTrue(
                    report_text.startswith(
                        "The set `feat/gripper-force` merged, as @alice asked:"
                    )
                )
                self.assertIn(
                    "Merged together: peppy#512, nodes-hub#88, contracts-hub#9.",
                    self.github.dashboard_body(pr),
                )
                self.assertEqual(
                    self.github.gates(pr)[-1],
                    merge_set.merging_gate(SET_NAME, "alice", RUN_URL),
                )

    def test_a_blocked_set_does_not_merge_and_the_box_is_cleared(self):
        self.github.check_runs_of[("contracts-hub", self.contracts.head_commit)] = [
            CheckRun(2, "test", 15368, False, None)
        ]
        self.tick_merge()
        self.assertEqual(self.github.merged, [])
        (refusal,) = self.github.reports_on(self.nodes)
        self.assertIn(
            "@alice, the set `feat/gripper-force` did not merge, because:", refusal
        )
        self.assertIn(
            "The required check `test` of contracts-hub#9 is running.", refusal
        )
        self.assertEqual(
            merge_set.ticked_actions(self.github.dashboard_body(self.nodes)), []
        )
        for pr in (self.peppy, self.nodes, self.contracts):
            self.assertEqual(self.github.gates(pr)[-1].state, "pending")

    def assert_refused(self, refusal_text):
        """The bot merged nothing, answered `refusal_text` on the pull request
        whose box was ticked, cleared the box and kept every gate blocked."""
        self.assertEqual(self.github.merged, [])
        (refusal,) = self.github.reports_on(self.nodes)
        self.assertEqual(refusal, refusal_text)
        self.assertEqual(
            merge_set.ticked_actions(self.github.dashboard_body(self.nodes)), []
        )
        for pr in (self.peppy, self.nodes, self.contracts):
            self.assertEqual(self.github.gates(pr)[-1].state, "pending")

    def test_an_author_without_write_access_to_a_repository_of_the_set_is_refused(
        self,
    ):
        self.github.permissions[("contracts-hub", "alice")] = "read"
        self.tick_merge(login="alice")
        self.assert_refused(
            "@alice, the bot did not merge the set for you: you have no write "
            f"access to contracts-hub. It does that only for {merge_set.WHO_MAY_ASK}.\n"
        )

    def test_a_user_with_write_access_who_opened_no_pull_request_is_refused(self):
        self.tick_merge(login="mallory")
        self.assert_refused(
            "@mallory, the bot did not merge the set for you: you did not open "
            "peppy#512, nodes-hub#88, contracts-hub#9. It does that only for "
            f"{merge_set.WHO_MAY_ASK}.\n"
        )

    def test_a_user_who_opened_no_pull_request_and_lacks_write_access_is_refused(
        self,
    ):
        self.github.permissions[("contracts-hub", "mallory")] = "read"
        self.tick_merge(login="mallory")
        self.assert_refused(
            "@mallory, the bot did not merge the set for you: you did not open "
            "peppy#512, nodes-hub#88, contracts-hub#9, and you have no write "
            f"access to contracts-hub. It does that only for {merge_set.WHO_MAY_ASK}.\n"
        )

    def test_the_author_of_some_pull_requests_of_the_set_alone_is_refused(self):
        self.contracts = self.github.set_author(self.contracts, "bob")
        self.tick_merge(login="alice")
        self.assert_refused(
            "@alice, the bot did not merge the set for you: you did not open "
            f"contracts-hub#9. It does that only for {merge_set.WHO_MAY_ASK}.\n"
        )

    def test_a_set_opened_by_two_users_merges_for_an_admin_alone(self):
        self.contracts = self.github.set_author(self.contracts, "bob")
        (on_peppy,) = self.github.comments_on(self.peppy)
        self.github.tick(on_peppy, Action.MERGE, "bob")
        self.tick_merge(login="alice")
        self.assertEqual(self.github.merged, [])
        (refusal,) = self.github.reports_on(self.peppy)
        self.assertIn("@bob, the bot did not merge the set for you", refusal)
        self.assertIn("@alice, the bot did not merge the set for you", refusal)
        self.github.make_admin("carol")
        self.tick_merge(login="carol")
        self.assertEqual(
            [label for label, _ in self.github.merged],
            ["peppy#512", "nodes-hub#88", "contracts-hub#9"],
        )

    def test_an_admin_merges_a_set_another_user_opened(self):
        self.github.make_admin("bob")
        self.tick_merge(login="bob")
        self.assertEqual(len(self.github.merged), 3)
        (report_text,) = self.github.reports_on(self.contracts)
        self.assertIn("merged, as @bob asked", report_text)

    def test_an_admin_of_some_repositories_who_opened_no_pull_request_is_refused(
        self,
    ):
        self.github.make_admin("bob", (PEPPY, NODES_HUB))
        self.tick_merge(login="bob")
        self.assert_refused(
            "@bob, the bot did not merge the set for you: you did not open "
            "peppy#512, nodes-hub#88, contracts-hub#9. It does that only for "
            f"{merge_set.WHO_MAY_ASK}.\n"
        )

    def test_a_refused_user_who_ticks_with_the_author_is_refused_and_not_named(
        self,
    ):
        (on_peppy,) = self.github.comments_on(self.peppy)
        self.github.tick(on_peppy, Action.MERGE, "mallory")
        self.tick_merge(login="alice")
        self.assertEqual(
            [label for label, _ in self.github.merged],
            ["peppy#512", "nodes-hub#88", "contracts-hub#9"],
        )
        refusal, report_text = self.github.reports_on(self.peppy)
        self.assertEqual(
            refusal,
            "@mallory, the bot did not merge the set for you: you did not open "
            "peppy#512, nodes-hub#88, contracts-hub#9. It does that only for "
            f"{merge_set.WHO_MAY_ASK}.\n",
        )
        self.assertTrue(
            report_text.startswith(
                "The set `feat/gripper-force` merged, as @alice asked:\n"
            )
        )
        for pr in (self.peppy, self.nodes, self.contracts):
            with self.subTest(pr=pr.label):
                self.assertEqual(
                    self.github.gates(pr)[-1],
                    merge_set.merging_gate(SET_NAME, "alice", RUN_URL),
                )

    def test_a_refused_user_who_ticks_with_the_author_is_not_named_in_a_refusal(
        self,
    ):
        self.github.check_runs_of[("contracts-hub", self.contracts.head_commit)] = [
            CheckRun(2, "test", 15368, False, None)
        ]
        (on_peppy,) = self.github.comments_on(self.peppy)
        self.github.tick(on_peppy, Action.MERGE, "mallory")
        self.tick_merge(login="alice")
        self.assertEqual(self.github.merged, [])
        (access_refusal,) = self.github.reports_on(self.peppy)
        self.assertTrue(access_refusal.startswith("@mallory, the bot did not merge"))
        (refusal,) = self.github.reports_on(self.nodes)
        self.assertTrue(
            refusal.startswith(
                "@alice, the set `feat/gripper-force` did not merge, because:"
            )
        )

    def test_a_box_no_user_ticked_is_cleared_without_a_merge(self):
        (comment,) = self.github.comments_on(self.nodes)
        self.github.tick(comment, Action.MERGE)
        comment.editor = None
        sync(self.github)
        self.assertEqual(self.github.merged, [])
        self.assertEqual(self.github.reports_on(self.nodes), [])
        self.assertEqual(merge_set.ticked_actions(comment.body), [])

    def test_a_merge_that_github_refuses_part_way_stops_and_reports(self):
        self.github.merge_refusals[self.nodes.label] = "Base branch was modified."
        self.tick_merge()
        self.assertEqual([label for label, _ in self.github.merged], ["peppy#512"])
        (report_text,) = self.github.reports_on(self.contracts)
        self.assertIn(
            "- nodes-hub#88 did not merge: Base branch was modified.", report_text
        )
        self.assertIn("- contracts-hub#9 was not tried.", report_text)
        # The two that did not merge are a set of their own, blocked again.
        for pr in (self.nodes, self.contracts):
            with self.subTest(pr=pr.label):
                self.assertEqual(self.github.gates(pr)[-1].state, "pending")
                self.assertIn("| nodes-hub |", self.github.dashboard_body(pr))
                self.assertNotIn("| peppy |", self.github.dashboard_body(pr))

    def test_a_merge_that_stops_blocks_the_rest_before_it_reads_the_set_again(self):
        self.github.merge_refusals[self.nodes.label] = "Base branch was modified."
        self.github.fail_reads_after_a_merge = True
        with self.assertRaises(ApiError):
            self.tick_merge()
        for pr in (self.nodes, self.contracts):
            with self.subTest(pr=pr.label):
                self.assertEqual(self.github.gates(pr)[-1].state, "pending")
                self.assertEqual(len(self.github.reports_on(pr)), 1)

    def test_the_pull_request_left_alone_by_a_partial_merge_merges_on_its_own(self):
        self.github.merge_refusals[self.contracts.label] = "Base branch was modified."
        self.tick_merge()
        self.assertEqual(
            self.github.gates(self.contracts)[-1], merge_set.alone_gate(SET_NAME)
        )
        self.assertEqual(
            self.github.dashboard_body(self.contracts),
            merge_set.render_alone_dashboard(SET_NAME),
        )

    def test_a_pull_request_github_does_not_merge_stops_the_set_before_any_merge(self):
        # A rule the bot does not read (a required deployment, say) blocks it.
        self.github.merge_states[self.contracts.label] = [(True, "blocked")]
        self.tick_merge()
        self.assertEqual(self.github.merged, [])
        self.assertEqual(
            self.github.sleeps,
            [merge_set.MERGE_STATE_INTERVAL_SECONDS]
            * (merge_set.MERGE_STATE_ATTEMPTS - 1),
        )
        (refusal,) = self.github.reports_on(self.nodes)
        self.assertIn(
            "GitHub does not merge contracts-hub#9 yet: its merge state is `blocked`.",
            refusal,
        )
        for pr in (self.peppy, self.nodes, self.contracts):
            self.assertEqual(self.github.gates(pr)[-1].state, "pending")

    def test_github_answering_from_before_the_green_gate_is_asked_again(self):
        self.github.merge_states[self.contracts.label] = [
            (True, "clean"),
            (True, "blocked"),
            (True, "clean"),
        ]
        self.tick_merge()
        self.assertEqual(len(self.github.merged), 3)

    def leave_unapproved(self, *pull_requests):
        # GitHub reports a pull request that lacks an approval as blocked.
        for pr in pull_requests:
            self.github.reviews[pr.label] = ReviewDecision.REVIEW_REQUIRED
            self.github.merge_states[pr.label] = [(True, "blocked")]

    def test_an_admin_merges_a_set_another_user_opened_that_lacks_approvals(self):
        self.leave_unapproved(self.nodes, self.contracts)
        self.github.make_admin("bob")
        self.tick_merge(login="bob")
        self.assertEqual(
            [label for label, _ in self.github.merged],
            ["peppy#512", "nodes-hub#88", "contracts-hub#9"],
        )
        (report_text,) = self.github.reports_on(self.peppy)
        self.assertIn("merged, as @bob asked", report_text)
        self.assertRegex(
            report_text, r"- nodes-hub#88 merged as `[0-9a-f]{7}` without an approval\."
        )

    def test_an_admin_merges_a_set_that_lacks_approvals(self):
        self.leave_unapproved(self.nodes, self.contracts)
        self.github.make_admin("alice")
        self.tick_merge()
        self.assertEqual(
            [label for label, _ in self.github.merged],
            ["peppy#512", "nodes-hub#88", "contracts-hub#9"],
        )
        # A pull request blocked for its approval alone is not asked again.
        self.assertEqual(self.github.sleeps, [])
        (report_text,) = self.github.reports_on(self.peppy)
        self.assertRegex(report_text, r"- peppy#512 merged as `[0-9a-f]{7}`\.\n")
        self.assertRegex(
            report_text, r"- nodes-hub#88 merged as `[0-9a-f]{7}` without an approval\."
        )
        self.assertRegex(
            report_text,
            r"- contracts-hub#9 merged as `[0-9a-f]{7}` without an approval\.",
        )

    def test_a_user_who_is_no_admin_does_not_merge_a_set_that_lacks_approvals(self):
        self.leave_unapproved(self.nodes)
        self.tick_merge()
        self.assertEqual(self.github.merged, [])
        (refusal,) = self.github.reports_on(self.nodes)
        self.assertIn("- nodes-hub#88 needs an approving review.", refusal)

    def test_an_admin_of_some_repositories_of_the_set_alone_is_no_admin_of_it(self):
        self.leave_unapproved(self.nodes)
        self.github.make_admin("alice", (PEPPY, NODES_HUB))
        self.tick_merge()
        self.assertEqual(self.github.merged, [])

    def test_an_admin_merges_nothing_of_a_set_whose_approval_the_app_cannot_bypass(
        self,
    ):
        # GitHub would merge peppy and refuse contracts-hub: the bot merges none.
        self.leave_unapproved(self.nodes, self.contracts)
        self.github.unbypassed.add(("contracts-hub", APPROVAL_RULESET))
        self.github.make_admin("alice")
        self.tick_merge()
        self.assertEqual(self.github.merged, [])
        (refusal,) = self.github.reports_on(self.nodes)
        self.assertIn(
            "- contracts-hub#9 needs an approving review. No admin merges it without "
            f"one: the merge-set App is not a bypass actor of the ruleset "
            f"`Ruleset {APPROVAL_RULESET}`.",
            refusal,
        )
        self.assertNotIn("nodes-hub#88", refusal)
        self.assertIn(
            "**What blocks the merge**", self.github.dashboard_body(self.nodes)
        )
        for pr in (self.peppy, self.nodes, self.contracts):
            self.assertEqual(self.github.gates(pr)[-1].state, "pending")

    def test_a_review_that_requests_changes_blocks_an_admin(self):
        self.github.reviews[self.nodes.label] = ReviewDecision.CHANGES_REQUESTED
        self.github.make_admin("alice")
        self.tick_merge()
        self.assertEqual(self.github.merged, [])
        (refusal,) = self.github.reports_on(self.nodes)
        self.assertIn("A reviewer requested changes on nodes-hub#88.", refusal)

    def test_an_admin_does_not_merge_an_approved_pull_request_github_blocks(self):
        # Another rule than the approval blocks it, which the admin does not waive.
        self.leave_unapproved(self.nodes)
        self.github.merge_states[self.contracts.label] = [(True, "blocked")]
        self.github.make_admin("alice")
        self.tick_merge()
        self.assertEqual(self.github.merged, [])
        (refusal,) = self.github.reports_on(self.nodes)
        self.assertIn("GitHub does not merge contracts-hub#9 yet", refusal)
        self.assertNotIn("nodes-hub#88", refusal)

    def test_two_users_who_tick_at_once_get_one_merge(self):
        self.github.make_admin("bob")
        (on_peppy,) = self.github.comments_on(self.peppy)
        self.github.tick(on_peppy, Action.MERGE, "bob")
        self.tick_merge(login="alice")
        self.assertEqual(len(self.github.merged), 3)
        (report_text,) = self.github.reports_on(self.contracts)
        self.assertIn("as @bob, @alice asked", report_text)


class SyncRerun(unittest.TestCase):
    def setUp(self):
        self.github = FakeGitHub(self)
        self.peppy = self.github.open_pull_request(PEPPY, 512)
        self.nodes = self.github.open_pull_request(NODES_HUB, 88)
        old_peppy = TestedSet(
            hubs={"nodes-hub": TestedRef("checkout", self.nodes.head_commit)},
            peppy=TestedRef(SET_NAME, commit(PEPPY, "old")),
        )
        self.stale_run = hub_run(
            NODES_HUB, 10, records=[JobRecord("check-index", old_peppy)]
        )
        self.running_run = hub_run(
            NODES_HUB, 11, completed=False, records=[JobRecord("discover", old_peppy)]
        )
        self.github.runs[self.nodes.label] = [self.stale_run, self.running_run]
        sync(self.github)

    def tick_rerun(self, login="alice"):
        (comment,) = self.github.comments_on(self.peppy)
        self.github.tick(comment, Action.RERUN, login)
        sync(self.github)

    def test_a_user_who_opened_no_pull_request_of_the_set_gets_no_re_run(self):
        self.tick_rerun(login="mallory")
        self.assertEqual(self.github.reruns, [])
        (refusal,) = self.github.reports_on(self.peppy)
        self.assertEqual(
            refusal,
            "@mallory, the bot did not re-run the CI of the set for you: you did "
            "not open peppy#512, nodes-hub#88. It does that only for "
            f"{merge_set.WHO_MAY_ASK}.\n",
        )
        self.assertIn(
            "- [ ] <!-- merge-set:rerun -->", self.github.dashboard_body(self.peppy)
        )

    def test_an_admin_re_runs_the_ci_of_a_set_another_user_opened(self):
        self.github.make_admin("bob", (PEPPY, NODES_HUB))
        self.tick_rerun(login="bob")
        self.assertEqual(self.github.reruns, [("nodes-hub", 10)])

    def test_a_refused_user_who_ticks_with_the_author_is_refused_and_not_named(
        self,
    ):
        (on_nodes,) = self.github.comments_on(self.nodes)
        self.github.tick(on_nodes, Action.RERUN, "mallory")
        self.tick_rerun(login="alice")
        self.assertEqual(self.github.reruns, [("nodes-hub", 10)])
        (report_text,) = self.github.reports_on(self.peppy)
        self.assertTrue(
            report_text.startswith(
                "@alice, the out-of-date CI of the set `feat/gripper-force`:"
            )
        )
        (refusal,) = self.github.reports_on(self.nodes)
        self.assertTrue(
            refusal.startswith(
                "@mallory, the bot did not re-run the CI of the set for you"
            )
        )

    def test_the_dashboard_names_the_stale_runs_and_offers_the_re_run(self):
        body = self.github.dashboard_body(self.peppy)
        self.assertIn(
            f"The job `check-index` of [Tests #10]({self.stale_run.url}) of nodes-hub#88 ran "
            f"with peppy at `{SET_NAME}` `{commit(PEPPY, 'old')[:7]}`",
            body,
        )
        self.assertIn(
            f"| `{self.nodes.head_commit[:7]}` | [passed]({run_url(NODES_HUB, 10)}) "
            "| CI out of date |",
            body,
        )
        self.assertIn("Re-run the out-of-date CI of this set (2 runs)", body)

    def test_a_branch_that_merged_its_base_alone_since_the_runs_needs_no_re_run(
        self,
    ):
        self.github.clean_base_merges.add(("peppy", commit(PEPPY, "old")))
        sync(self.github)
        body = self.github.dashboard_body(self.peppy)
        self.assertIn("**Ready to merge.**", body)
        self.assertIn(merge_set.CURRENT_HUB_RUNS, body)
        self.assertNotIn("merge-set:rerun", body)

    def test_the_finished_stale_runs_are_re_run(self):
        self.tick_rerun()
        self.assertEqual(self.github.reruns, [("nodes-hub", 10)])
        (report_text,) = self.github.reports_on(self.peppy)
        self.assertIn(
            f"Re-run now:\n- [Tests #10]({self.stale_run.url}) of nodes-hub",
            report_text,
        )
        self.assertIn("Still running with an old commit", report_text)
        self.assertIn(
            f"- [Tests #11]({self.running_run.url}) of nodes-hub", report_text
        )

    def test_nothing_is_re_run_when_nothing_is_out_of_date(self):
        (comment,) = self.github.comments_on(self.peppy)
        self.github.tick(comment, Action.RERUN)
        self.github.runs[self.nodes.label] = []
        sync(self.github)
        self.assertEqual(self.github.reruns, [])
        (report_text,) = self.github.reports_on(self.peppy)
        self.assertIn(
            "no CI of the set `feat/gripper-force` is out of date", report_text
        )
        self.assertNotIn("merge-set:rerun", self.github.dashboard_body(self.peppy))


class FakeApi:
    """GitHubApi with the answer of each call from a table: (method, path) to
    a response, or to an ApiError to raise."""

    def __init__(self, answers, downloads=None):
        self.token = "token"
        self.answers = answers
        self.downloads = downloads or {}
        self.calls = []

    def request(self, method, path, query=None, body=None):
        self.calls.append((method, path, query, body))
        answer = self.answers[(method, path)]
        if isinstance(answer, Exception):
            raise answer
        return answer

    def pages(self, path, query=None, key=None):
        response = self.request("GET", path, query)
        return response if key is None else response[key]

    def graphql(self, query, variables):
        self.calls.append(("GRAPHQL", query, variables))
        return self.answers[("GRAPHQL", json.dumps(variables, sort_keys=True))]

    def download(self, url):
        return self.downloads[url]


class FakeGit:
    """GitMerges with the answer of each merge from a table: (merge base,
    ours, theirs, head) to whether it is clean, or to a GitError to raise."""

    def __init__(self, answers):
        self.answers = answers
        self.calls = []

    def is_clean_merge(self, repository, merge_base, ours, theirs, head):
        self.calls.append((repository, merge_base, ours, theirs, head))
        answer = self.answers[(merge_base, ours, theirs, head)]
        if isinstance(answer, Exception):
            raise answer
        return answer


def gateway_of(api, git=None):
    """A GitHubGateway on `api`, with a git that answers no merge unless the
    case gives one."""
    return merge_set.GitHubGateway(api, BOT, git or FakeGit({}))


def comparison_item(merge_base, behind_by=0):
    return {"behind_by": behind_by, "merge_base_commit": {"sha": merge_base}}


def not_found(path):
    return ApiError(f"GET {path}", 404, "Not Found")


class Gateway(unittest.TestCase):
    def test_a_branch_head_is_read_by_its_exact_name(self):
        path = f"/repos/Peppy-bot/nodes-hub/git/ref/heads/{SET_NAME}"
        exact = {"ref": f"refs/heads/{SET_NAME}", "object": {"sha": commit(NODES_HUB)}}
        for answer, expected in (
            (exact, commit(NODES_HUB)),
            (not_found(path), None),
            ([{"ref": f"refs/heads/{SET_NAME}-2", "object": {"sha": "x"}}], None),
        ):
            with self.subTest(answer=answer):
                gateway = gateway_of(FakeApi({("GET", path): answer}))
                self.assertEqual(gateway.branch_head(NODES_HUB, SET_NAME), expected)

    def test_a_merge_github_refuses_is_checked_against_the_pull_request(self):
        path = "/repos/Peppy-bot/nodes-hub/pulls/88"
        refused = ApiError(
            f"PUT {path}/merge", 405, "Required status check is expected."
        )
        for pull, expected in (
            ({"merged": True, "merge_commit_sha": "m" * 40}, "m" * 40),
            ({"merged": False}, None),
        ):
            with self.subTest(pull=pull):
                api = FakeApi({("PUT", f"{path}/merge"): refused, ("GET", path): pull})
                gateway = gateway_of(api)
                if expected is None:
                    with self.assertRaises(MergeRefused) as refusal:
                        gateway.merge(pull_request(NODES_HUB, 88), "message")
                    self.assertEqual(
                        str(refusal.exception), "Required status check is expected."
                    )
                else:
                    self.assertEqual(
                        gateway.merge(pull_request(NODES_HUB, 88), "message"), expected
                    )

    def test_a_merge_asks_for_the_head_commit_it_checked(self):
        path = "/repos/Peppy-bot/nodes-hub/pulls/88/merge"
        api = FakeApi({("PUT", path): {"sha": "m" * 40}})
        gateway_of(api).merge(pull_request(NODES_HUB, 88), "message")
        self.assertEqual(
            api.calls[0][3],
            {
                "merge_method": "merge",
                "sha": commit(NODES_HUB),
                "commit_message": "message",
            },
        )

    def test_the_runs_of_a_hub_pull_request_carry_the_records_of_their_latest_jobs(
        self,
    ):
        pr = pull_request(NODES_HUB, 88)
        runs_path = "/repos/Peppy-bot/nodes-hub/actions/runs"
        tested = set_record_text(
            {"nodes-hub": {"ref": "checkout", "commit": pr.head_commit}}
        )
        api = FakeApi(
            {
                ("GET", runs_path): {
                    "workflow_runs": [
                        {
                            "id": 5,
                            "workflow_id": 1,
                            "name": "Tests",
                            "run_number": 3,
                            "html_url": "u5",
                            "status": "completed",
                        },
                        {
                            "id": 7,
                            "workflow_id": 1,
                            "name": "Tests",
                            "run_number": 4,
                            "html_url": "u7",
                            "status": "in_progress",
                        },
                    ]
                },
                ("GET", f"{runs_path}/7/jobs"): {
                    "jobs": [
                        {"id": 70, "name": "check-index"},
                        {"id": 71, "name": "gate"},
                        {"id": 72, "name": "discover"},
                    ]
                },
                ("GET", f"{runs_path}/7/artifacts"): {
                    "artifacts": [
                        {
                            "name": "hub-ci-set-70",
                            "expired": False,
                            "archive_download_url": "a70",
                        },
                        {
                            "name": "hub-ci-set-72",
                            "expired": True,
                            "archive_download_url": "a72",
                        },
                    ]
                },
            },
            downloads={"a70": record_bundle(tested)},
        )
        (run,) = gateway_of(api).hub_runs(pr)
        self.assertEqual((run.id, run.name, run.completed), (7, "Tests #4", False))
        self.assertEqual(
            run.records,
            (
                JobRecord("check-index", merge_set.parse_tested_set(tested)),
                JobRecord("discover", None),
            ),
        )
        self.assertEqual(
            api.calls[0][2], {"head_sha": pr.head_commit, "event": "pull_request"}
        )

    def test_the_ci_of_a_pull_request_is_the_newest_run_of_the_ci_workflow(self):
        workflows_path = "/repos/Peppy-bot/nodes-hub/actions/workflows"
        runs_path = "/repos/Peppy-bot/nodes-hub/actions/runs"
        api = FakeApi(
            {
                ("GET", workflows_path): {
                    "workflows": [
                        {"id": OTHER_WORKFLOW_ID, "name": "Merge set events"},
                        {"id": CI_WORKFLOW_ID, "name": "Tests"},
                    ]
                },
                ("GET", runs_path): {
                    "workflow_runs": [
                        run_item(5, conclusion="failure"),
                        run_item(7, status="in_progress", conclusion=None),
                        run_item(9, workflow_id=OTHER_WORKFLOW_ID),
                    ]
                },
            }
        )
        gateway = gateway_of(api)
        pr = pull_request(NODES_HUB, 88)
        self.assertEqual(gateway.ci_run(pr), merge_set.CiRun("u7", CheckState.RUNNING))
        self.assertIn(
            ("GET", runs_path, {"head_sha": pr.head_commit, "event": "pull_request"}),
            [call[:3] for call in api.calls],
        )
        # The workflows of a repository are read once for the whole sync.
        gateway.ci_run(pull_request(NODES_HUB, 89))
        self.assertEqual(
            sorted(call[1] for call in api.calls),
            [runs_path, runs_path, workflows_path],
        )

    def test_a_pull_request_is_behind_when_its_base_has_commits_it_lacks(self):
        pr = pull_request(NODES_HUB, 88)
        path = f"/repos/Peppy-bot/nodes-hub/compare/main...{pr.head_commit}"
        for behind_by, expected in ((0, False), (3, True)):
            with self.subTest(behind_by=behind_by):
                api = FakeApi({("GET", path): comparison_item("m" * 40, behind_by)})
                self.assertEqual(gateway_of(api).is_behind(pr), expected)
                self.assertEqual(api.calls[0][2], {"per_page": 1})

    def clean_base_merge_case(self, tested_answer=None, git_answer=True):
        """A branch of nodes-hub that a job ran at `tested`, forked from `main`
        at `fork`, and whose head holds `main` at `base`. GitHub answers the
        comparison of `base` with `tested` with `tested_answer`, else with
        `fork`, and git answers the merge with `git_answer`."""
        self.tested, self.head = commit(NODES_HUB, "tested"), commit(NODES_HUB)
        self.base, self.fork = commit(NODES_HUB, "main"), commit(NODES_HUB, "fork")
        compare = "/repos/Peppy-bot/nodes-hub/compare"
        self.api = FakeApi(
            {
                ("GET", f"{compare}/main...{self.head}"): comparison_item(self.base),
                ("GET", f"{compare}/{self.base}...{self.tested}"): (
                    tested_answer or comparison_item(self.fork)
                ),
            }
        )
        self.git = FakeGit({(self.fork, self.tested, self.base, self.head): git_answer})
        self.mismatch = merge_set.Mismatch(
            NODES_HUB, TestedRef(SET_NAME, self.tested), self.head
        )
        return gateway_of(self.api, self.git)

    def test_git_merges_the_tested_commit_with_the_base_the_head_holds(self):
        for clean in (True, False):
            with self.subTest(clean=clean):
                gateway = self.clean_base_merge_case(git_answer=clean)
                self.assertEqual(gateway.is_clean_base_merge(self.mismatch), clean)
                self.assertEqual(
                    self.git.calls,
                    [(NODES_HUB, self.fork, self.tested, self.base, self.head)],
                )
                # The jobs that ran the same commit are answered at once.
                self.assertEqual(gateway.is_clean_base_merge(self.mismatch), clean)
                self.assertEqual(len(self.git.calls), 1)
                self.assertEqual(len(self.api.calls), 2)

    def test_a_tested_commit_github_does_not_know_is_no_clean_base_merge(self):
        path = f"/repos/Peppy-bot/nodes-hub/compare/{commit(NODES_HUB, 'main')}..."
        gateway = self.clean_base_merge_case(not_found(path))
        self.assertFalse(gateway.is_clean_base_merge(self.mismatch))
        self.assertEqual(self.git.calls, [])

    def test_another_failure_of_github_stops_the_sync(self):
        failure = ApiError("GET compare", 502, "Bad Gateway")
        gateway = self.clean_base_merge_case(failure)
        with self.assertRaises(ApiError):
            gateway.is_clean_base_merge(self.mismatch)

    def test_a_merge_git_cannot_check_is_no_clean_base_merge_and_warns(self):
        gateway = self.clean_base_merge_case(
            git_answer=merge_set.GitError("`git fetch` failed: not our ref")
        )
        with patch("sys.stdout", io.StringIO()) as log:
            self.assertFalse(gateway.is_clean_base_merge(self.mismatch))
        self.assertTrue(log.getvalue().startswith("::warning::"))
        self.assertIn(f"ran nodes-hub at {self.tested[:7]}", log.getvalue())
        self.assertIn("not our ref", log.getvalue())

    def test_a_user_who_is_no_collaborator_has_no_permission(self):
        path = "/repos/Peppy-bot/nodes-hub/collaborators/mallory/permission"
        api = FakeApi({("GET", path): not_found(path)})
        gateway = gateway_of(api)
        self.assertEqual(gateway.permission(NODES_HUB, "mallory"), "none")
        self.assertEqual(gateway.permission(NODES_HUB, "mallory"), "none")
        self.assertEqual(len(api.calls), 1)

    def test_the_app_bypasses_a_ruleset_that_names_it_as_a_bypass_actor(self):
        path = f"/repos/Peppy-bot/nodes-hub/rulesets/{APPROVAL_RULESET}"
        for ruleset, bypassed in (
            ({"current_user_can_bypass": "always"}, True),
            ({"current_user_can_bypass": "pull_requests_only"}, True),
            ({"current_user_can_bypass": "exempt"}, True),
            ({"current_user_can_bypass": "never"}, False),
            ({}, False),
        ):
            with self.subTest(ruleset=ruleset):
                api = FakeApi(
                    {("GET", path): {"name": "Protect main and dev", **ruleset}}
                )
                gateway = gateway_of(api)
                expected = merge_set.RulesetBypass("Protect main and dev", bypassed)
                self.assertEqual(
                    gateway.ruleset_bypass(NODES_HUB, APPROVAL_RULESET), expected
                )
                self.assertEqual(
                    gateway.ruleset_bypass(NODES_HUB, APPROVAL_RULESET), expected
                )
                self.assertEqual(len(api.calls), 1)

    def test_the_dashboard_is_the_comment_of_the_bot_with_the_marker(self):
        pr = pull_request(NODES_HUB, 88)
        path = "/repos/Peppy-bot/nodes-hub/issues/88/comments"
        comment = {
            "user": {"login": BOT},
            "id": 3,
            "node_id": "n3",
            "html_url": "c3",
            "body": f"{merge_set.DASHBOARD_MARKER}\nset",
        }
        api = FakeApi(
            {
                ("GET", path): [
                    {**comment, "id": 1, "user": {"login": "mallory"}},
                    {**comment, "id": 2, "body": "a report"},
                    comment,
                ]
            }
        )
        dashboard = gateway_of(api).dashboard(pr)
        self.assertEqual(
            (dashboard.id, dashboard.node_id, dashboard.url), (3, "n3", "c3")
        )

    def test_the_gate_carries_its_link_when_it_has_one(self):
        pr = pull_request(NODES_HUB, 88)
        path = f"/repos/Peppy-bot/nodes-hub/statuses/{pr.head_commit}"
        api = FakeApi({("POST", path): {}})
        gateway = gateway_of(api)
        gateway.post_gate(pr, merge_set.alone_gate(SET_NAME))
        gateway.post_gate(pr, merge_set.member_gate(SET_NAME, "https://c"))
        self.assertNotIn("target_url", api.calls[0][3])
        self.assertEqual(api.calls[1][3]["target_url"], "https://c")
        self.assertEqual(api.calls[1][3]["context"], "merge-set")

    def test_the_api_reads_every_page_of_a_list(self):
        api = merge_set.GitHubApi("token")
        pages = [list(range(100)), list(range(100, 130))]
        with patch.object(merge_set.GitHubApi, "request", side_effect=pages) as request:
            self.assertEqual(api.pages("/x", {"state": "all"}), list(range(130)))
        self.assertEqual(
            [call.args[2] for call in request.call_args_list],
            [
                {"state": "all", "per_page": 100, "page": 1},
                {"state": "all", "per_page": 100, "page": 2},
            ],
        )

    def test_a_refused_call_names_the_message_github_gave(self):
        error = merge_set.urllib.error.HTTPError(
            "https://api.github.com/x",
            405,
            "Method Not Allowed",
            {},
            io.BytesIO(b'{"message": "Pull Request is not mergeable"}'),
        )
        with (
            patch.object(merge_set.urllib.request, "urlopen", side_effect=error),
            self.assertRaises(ApiError) as refused,
        ):
            merge_set.GitHubApi("token").request("PUT", "/x")
        self.assertEqual(refused.exception.status, 405)
        self.assertEqual(refused.exception.message, "Pull Request is not mergeable")


class GitFixture:
    """A repository on disk that GitMerges fetches from as it fetches from
    GitHub, and a work tree that makes its commits."""

    LINES = "".join(f"line {number}\n" for number in range(1, 9))

    def __init__(self, directory):
        self.remote = Path(directory) / "remote.git"
        self.work = Path(directory) / "work"
        self.environment = {
            **os.environ,
            "GIT_CONFIG_NOSYSTEM": "1",
            "GIT_CONFIG_GLOBAL": os.devnull,
            "GIT_AUTHOR_NAME": "Test",
            "GIT_AUTHOR_EMAIL": "test@example.com",
            "GIT_COMMITTER_NAME": "Test",
            "GIT_COMMITTER_EMAIL": "test@example.com",
        }
        self.git(directory, "init", "--quiet", "--bare", str(self.remote))
        # GitHub serves a fetch without the files and a fetch by commit.
        self.git(self.remote, "config", "uploadpack.allowFilter", "true")
        self.git(self.remote, "config", "uploadpack.allowAnySHA1InWant", "true")
        self.git(directory, "init", "--quiet", "--initial-branch=main", str(self.work))
        self.fork = self.commit("fork", shared=self.LINES, base_only="base\n")
        self.git(self.work, "checkout", "--quiet", "-b", SET_NAME)
        self.tested = self.commit(
            "the change of the branch",
            branch_only="branch\n",
            shared=self.LINES.replace("line 1\n", "branch 1\n"),
        )
        # The job ran the branch as it was pushed.
        self.push()

    def git(self, where, *arguments):
        return subprocess.run(
            ["git", "-C", str(where), *arguments],
            capture_output=True,
            text=True,
            env=self.environment,
            check=True,
        ).stdout.strip()

    def write(self, **files):
        for name, content in files.items():
            (self.work / name).write_text(content)

    def commit(self, message, **files):
        self.write(**files)
        self.git(self.work, "add", "--all")
        self.git(self.work, "commit", "--quiet", "--message", message)
        return self.git(self.work, "rev-parse", "HEAD")

    def commit_on_main(self, **files):
        """A commit of `main`, made while the branch is checked out."""
        self.git(self.work, "checkout", "--quiet", "main")
        base = self.commit("a change of main", **files)
        self.git(self.work, "checkout", "--quiet", SET_NAME)
        return base

    def merge_main(self, **edits):
        """Merge `main` into the branch. `edits` are written into the merge
        commit: the resolution of a conflict, or a change of the merge."""
        self.git(self.work, "merge", "--quiet", "--no-commit", "main")
        self.write(**edits)
        self.git(self.work, "add", "--all")
        self.git(self.work, "commit", "--quiet", "--no-edit")
        return self.git(self.work, "rev-parse", "HEAD")

    def merge_main_with_a_conflict(self, resolution):
        merge = subprocess.run(
            ["git", "-C", str(self.work), "merge", "--quiet", "main"],
            capture_output=True,
            env=self.environment,
        )
        if merge.returncode == 0:
            raise AssertionError("the merge of main has no conflict")
        self.write(shared=resolution)
        self.git(self.work, "add", "--all")
        self.git(self.work, "commit", "--quiet", "--no-edit")
        return self.git(self.work, "rev-parse", "HEAD")

    def push(self):
        self.git(self.work, "push", "--quiet", "--force", str(self.remote), "--all")

    def is_clean_merge(self, base, head):
        """What GitMerges answers for `head` and the commit the job ran, at
        the merge base GitHub would give."""
        self.push()
        merge_base = self.git(self.work, "merge-base", self.tested, base)
        git = merge_set.GitMerges("token", lambda repository: self.remote.as_uri())
        return git.is_clean_merge(NODES_HUB, merge_base, self.tested, base, head)


class GitMergesOnDisk(unittest.TestCase):
    def setUp(self):
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        self.fixture = GitFixture(directory.name)

    def test_a_merge_of_the_base_that_touches_no_file_of_the_branch_is_clean(self):
        base = self.fixture.commit_on_main(base_only="base 2\n")
        head = self.fixture.merge_main()
        self.assertTrue(self.fixture.is_clean_merge(base, head))

    def test_a_merge_git_makes_in_a_file_both_sides_changed_is_clean(self):
        base = self.fixture.commit_on_main(
            shared=GitFixture.LINES.replace("line 8\n", "main 8\n")
        )
        head = self.fixture.merge_main()
        self.assertTrue(self.fixture.is_clean_merge(base, head))

    def test_a_conflict_resolved_by_hand_is_no_clean_merge(self):
        base = self.fixture.commit_on_main(
            shared=GitFixture.LINES.replace("line 1\n", "main 1\n")
        )
        head = self.fixture.merge_main_with_a_conflict(
            GitFixture.LINES.replace("line 1\n", "branch and main 1\n")
        )
        self.assertFalse(self.fixture.is_clean_merge(base, head))

    def test_a_change_made_in_the_merge_commit_is_no_clean_merge(self):
        base = self.fixture.commit_on_main(base_only="base 2\n")
        head = self.fixture.merge_main(branch_only="branch, changed in the merge\n")
        self.assertFalse(self.fixture.is_clean_merge(base, head))

    def test_a_commit_of_the_branch_after_the_merge_is_no_clean_merge(self):
        base = self.fixture.commit_on_main(base_only="base 2\n")
        self.fixture.merge_main()
        head = self.fixture.commit("a fix", branch_only="branch fixed\n")
        self.assertFalse(self.fixture.is_clean_merge(base, head))

    def test_a_clean_rebase_onto_the_base_holds_the_same_tree_as_its_merge(self):
        base = self.fixture.commit_on_main(base_only="base 2\n")
        self.fixture.git(self.fixture.work, "rebase", "--quiet", "main")
        head = self.fixture.git(self.fixture.work, "rev-parse", "HEAD")
        self.assertTrue(self.fixture.is_clean_merge(base, head))

    def test_a_commit_the_remote_does_not_hold_raises_a_git_error(self):
        base = self.fixture.commit_on_main(base_only="base 2\n")
        head = self.fixture.merge_main()
        # A commit that no push carried, as a branch that was force pushed.
        unpushed = commit(NODES_HUB, "unpushed")
        with self.assertRaises(merge_set.GitError) as failure:
            self.fixture.is_clean_merge(base, unpushed)
        self.assertIn("`git fetch", str(failure.exception))
        self.assertTrue(self.fixture.is_clean_merge(base, head))

    def test_the_token_goes_to_github_alone_and_the_host_configuration_nowhere(
        self,
    ):
        environment = merge_set.GitMerges("secret").environment
        self.assertEqual(
            environment["GIT_CONFIG_KEY_0"], "http.https://github.com/.extraHeader"
        )
        scheme, credentials = environment["GIT_CONFIG_VALUE_0"].split(": ")[1].split()
        self.assertEqual(scheme, "basic")
        self.assertEqual(
            base64.b64decode(credentials).decode(), "x-access-token:secret"
        )
        self.assertEqual(environment["GIT_CONFIG_GLOBAL"], os.devnull)
        self.assertEqual(environment["GIT_CONFIG_NOSYSTEM"], "1")
        self.assertEqual(environment["GIT_TERMINAL_PROMPT"], "0")
        self.assertEqual(
            merge_set.github_remote(NODES_HUB),
            "https://github.com/Peppy-bot/nodes-hub.git",
        )


class Commands(unittest.TestCase):
    def test_the_repositories_command_names_them_in_the_step_output(self):
        with tempfile.TemporaryDirectory() as directory:
            output = Path(directory) / "output"
            output.touch()
            with patch.dict(os.environ, {"GITHUB_OUTPUT": str(output)}):
                self.assertEqual(merge_set.main(["repositories"]), 0)
            self.assertEqual(
                output.read_text(), f"repositories={merge_set.repository_list()}\n"
            )

    def test_an_error_stops_the_run_as_one_workflow_command(self):
        with (
            patch.dict(
                os.environ, {"EVENT_REPOSITORY": "landing-page", "EVENT_BRANCH": "x"}
            ),
            patch("sys.stdout", io.StringIO()) as log,
        ):
            self.assertEqual(merge_set.main(["sync"]), 1)
        self.assertTrue(
            log.getvalue().startswith("::error::`landing-page` is not a repository")
        )


def workflow_lines(name):
    return [line.strip() for line in (WORKFLOWS / name).read_text().splitlines()]


class RepositoryFacts(unittest.TestCase):
    def test_the_sync_workflow_takes_the_inputs_the_relay_gives(self):
        text = (WORKFLOWS / merge_set.SYNC_WORKFLOW).read_text()
        inputs = merge_set.dispatch_inputs(PEPPY, merge_set.RelaySubject(1, "b", "r"))
        for name in inputs:
            with self.subTest(input=name):
                self.assertIn(f"\n      {name}:\n", text)

    def test_the_sync_workflow_gives_the_script_every_variable_it_reads(self):
        lines = workflow_lines(merge_set.SYNC_WORKFLOW)
        for line in (
            "EVENT_REPOSITORY: ${{ inputs.repository }}",
            "EVENT_BRANCH: ${{ inputs.branch }}",
            "EVENT_HEAD_REPOSITORY: ${{ inputs.head-repository }}",
            "EVENT_PULL_REQUEST: ${{ inputs.pull-request }}",
            "MERGE_SET_TOKEN: ${{ steps.token.outputs.token }}",
            "MERGE_SET_BOT_LOGIN: ${{ steps.token.outputs.app-slug }}[bot]",
            "repositories: ${{ steps.repositories.outputs.repositories }}",
            "run: python3 .github/merge-set/merge_set.py repositories",
            "run: python3 .github/merge-set/merge_set.py sync",
        ):
            with self.subTest(line=line):
                self.assertIn(line, lines)

    def test_one_sync_runs_at_a_time_for_each_set_and_none_is_cancelled(self):
        lines = workflow_lines(merge_set.SYNC_WORKFLOW)
        self.assertIn("group: merge-set-${{ inputs.branch }}", lines)
        self.assertIn("cancel-in-progress: false", lines)

    def test_the_key_of_the_app_is_in_the_environment_of_the_sync(self):
        text = (WORKFLOWS / merge_set.SYNC_WORKFLOW).read_text()
        self.assertEqual(text.count("secrets.MERGE_SET_BOT_PRIVATE_KEY"), 1)
        self.assertIn("    environment:\n      name: merge-set\n", text)

    def test_the_sync_starts_at_once_and_ends_within_the_limit_of_its_runner(self):
        # An ubuntu-slim job stops after 15 minutes.
        lines = workflow_lines(merge_set.SYNC_WORKFLOW)
        self.assertIn("runs-on: ubuntu-slim", lines)
        (timeout,) = [line for line in lines if line.startswith("timeout-minutes:")]
        self.assertLessEqual(int(timeout.split(":")[1]), 15)

    def test_the_relay_hands_on_the_runs_of_the_ci_workflow(self):
        self.assertIn(
            f"name: {merge_set.CI_WORKFLOW}", workflow_lines(resolve.PEPPY_CI_WORKFLOW)
        )

    def test_the_changes_job_runs_these_cases(self):
        text = (WORKFLOWS / "tests.yml").read_text()
        self.assertIn("--start-directory .github/merge-set", text)


if __name__ == "__main__":
    unittest.main()
