#!/usr/bin/env python3
"""Tests for the resolution of the peppy build and the hubs of a hub's CI job.

Every case runs a pure function of resolve.py on fixed input data: an event
payload, the branch heads `git ls-remote` reports, a REST response. Nothing
touches the network. The last cases hold the hub list to the defaults peppy
bundles and the artifact names to the workflows that upload them, which are
files of this repository, so the changes job of tests.yml runs these cases on
every change.
"""

import json
import re
import unittest
from pathlib import Path

import resolve
from resolve import Arch, HubOrigin, PeppyBuildKind, PeppyBuildPolicy, ResolveError

REPOSITORY_ROOT = Path(__file__).resolve().parents[3]
ACTION_DIR = Path(__file__).resolve().parent

NODES_HUB = resolve.HUBS_BY_NAME["nodes-hub"]
PRIVATE_NODES_HUB = resolve.HUBS_BY_NAME["private-nodes-hub"]
PUBLIC_HUBS = [
    hub for hub in resolve.HUBS if hub.visibility is resolve.Visibility.PUBLIC
]

SET_NAME = "feat/gripper-force"
CHECKOUT_COMMIT = "c" * 40
PEPPY_BRANCH_COMMIT = "a" * 40
PEPPY_DEV_COMMIT = "d" * 40


def commit_of(hub, branch):
    """A distinct, fixed commit for each hub branch, so a case can tell which
    branch a choice took."""
    return f"{hub.repository_id:04d}{branch}".encode().hex()[:40].ljust(40, "0")


def heads(hub, *branches):
    """What `git ls-remote` reports for `branches` of `hub`."""
    return {branch: commit_of(hub, branch) for branch in branches}


def pull_request_event(head_branch, head_repository="Peppy-bot/nodes-hub"):
    return {
        "pull_request": {
            "head": {
                "ref": head_branch,
                "repo": None
                if head_repository is None
                else {"full_name": head_repository},
            },
            "base": {"ref": "main", "repo": {"full_name": "Peppy-bot/nodes-hub"}},
        }
    }


def runs_response(*runs):
    """A workflow runs response of (id, status) pairs."""
    return {
        "total_count": len(runs),
        "workflow_runs": [
            {
                "id": run_id,
                "status": status,
                "html_url": resolve.run_url(run_id),
            }
            for run_id, status in runs
        ],
    }


def artifacts_response(name, *artifacts):
    """An artifact list response of (id, expired) pairs named `name`."""
    return {
        "total_count": len(artifacts),
        "artifacts": [
            {
                "id": artifact_id,
                "name": name,
                "expired": expired,
                "archive_download_url": "https://api.github.com/repos/"
                f"Peppy-bot/peppy/actions/artifacts/{artifact_id}/zip",
            }
            for artifact_id, expired in artifacts
        ],
    }


def all_heads(set_branch_in=()):
    """Branch heads of every hub: `main` everywhere, and the branch of the set
    in the hubs named by `set_branch_in`."""
    return {
        hub: heads(hub, "main", *([SET_NAME] if hub.name in set_branch_in else []))
        for hub in resolve.HUBS
    }


def choose_by_branch(
    under_test=NODES_HUB, set_name=SET_NAME, key=True, set_branch_in=()
):
    """The set of a job on `under_test`, with the deploy key when `key`, when
    the hubs named by `set_branch_in` have a branch of the set."""
    return resolve.choose_hubs_by_branch(
        under_test, CHECKOUT_COMMIT, set_name, key, all_heads(set_branch_in)
    )


def by_name(hub_set):
    return {resolved.hub.name: resolved for resolved in hub_set.hubs}


def explicit_set_text(replaced=None, left_out=()):
    """An explicit set pinning every hub at `main` and nodes-hub at the
    checkout, with the entries of `replaced` (by hub name) swapped in and the
    hubs named in `left_out` removed."""
    entries = {
        hub.name: {"ref": "peppy-release/v0.31.2", "commit": commit_of(hub, "main")}
        for hub in resolve.HUBS
    }
    entries["nodes-hub"]["commit"] = CHECKOUT_COMMIT
    entries.update(replaced or {})
    for name in left_out:
        del entries[name]
    return json.dumps({"hubs": entries})


def installed_peppy():
    return resolve.InstalledPeppy(
        kind=PeppyBuildKind.LATEST_RELEASE,
        version="peppy v0.31.3",
        source=resolve.latest_release_url(Arch.X86_64),
        description="the latest release",
    )


class SetName(unittest.TestCase):
    def test_a_pull_request_names_the_set_after_its_head_branch(self):
        trigger = resolve.parse_trigger("pull_request", pull_request_event(SET_NAME))
        self.assertEqual(trigger.set_name, SET_NAME)
        self.assertFalse(trigger.from_fork)

    def test_a_pull_request_from_a_fork_has_no_set_name(self):
        for head_repository in ("someone/nodes-hub", None):
            with self.subTest(head_repository=head_repository):
                trigger = resolve.parse_trigger(
                    "pull_request", pull_request_event(SET_NAME, head_repository)
                )
                self.assertIsNone(trigger.set_name)
                self.assertTrue(trigger.from_fork)

    def test_a_pull_request_from_an_integration_branch_has_no_set_name(self):
        for branch in ("main", "dev"):
            with self.subTest(branch=branch):
                trigger = resolve.parse_trigger(
                    "pull_request", pull_request_event(branch)
                )
                self.assertIsNone(trigger.set_name)
                self.assertFalse(trigger.from_fork)

    def test_every_other_event_has_no_set_name(self):
        for event_name in ("push", "workflow_dispatch", "schedule", "merge_group"):
            with self.subTest(event_name=event_name):
                trigger = resolve.parse_trigger(event_name, {"ref": "refs/heads/x"})
                self.assertIsNone(trigger.set_name)
                self.assertFalse(trigger.from_fork)

    def test_a_pull_request_event_without_its_fields_is_refused(self):
        with self.assertRaises(ResolveError):
            resolve.parse_trigger("pull_request", {})


class HubUnderTest(unittest.TestCase):
    def test_each_hub_is_found_by_its_repository(self):
        for hub in resolve.HUBS:
            with self.subTest(hub=hub.name):
                self.assertIs(resolve.hub_under_test(f"Peppy-bot/{hub.name}"), hub)

    def test_a_repository_that_is_no_hub_is_refused_naming_every_hub(self):
        with self.assertRaises(ResolveError) as refused:
            resolve.hub_under_test("Peppy-bot/peppy")
        for hub in resolve.HUBS:
            self.assertIn(hub.repository, str(refused.exception))


class HubsByBranch(unittest.TestCase):
    def test_a_sibling_with_a_branch_of_the_set_runs_at_its_head(self):
        for hub in resolve.HUBS:
            if hub is NODES_HUB:
                continue
            with self.subTest(hub=hub.name):
                resolved = by_name(choose_by_branch(set_branch_in={hub.name}))[hub.name]
                self.assertEqual(resolved.origin, HubOrigin.SET_BRANCH)
                self.assertEqual(resolved.ref, SET_NAME)
                self.assertEqual(resolved.commit, commit_of(hub, SET_NAME))

    def test_a_sibling_without_a_branch_of_the_set_runs_at_main(self):
        for hub in resolve.HUBS:
            if hub is NODES_HUB:
                continue
            with self.subTest(hub=hub.name):
                resolved = by_name(choose_by_branch())[hub.name]
                self.assertEqual(resolved.origin, HubOrigin.MAIN)
                self.assertEqual(resolved.ref, "main")
                self.assertEqual(resolved.commit, commit_of(hub, "main"))

    def test_without_a_set_name_every_sibling_runs_at_main(self):
        hub_set = choose_by_branch(set_name=None, set_branch_in={"contracts-hub"})
        self.assertIsNone(hub_set.name)
        for resolved in hub_set.hubs:
            if resolved.hub is not NODES_HUB:
                self.assertEqual(resolved.origin, HubOrigin.MAIN)

    def test_the_hub_under_test_is_always_its_checkout(self):
        for hub in resolve.HUBS:
            for set_branch_in in ((), {hub.name}):
                with self.subTest(hub=hub.name, set_branch_in=set_branch_in):
                    hub_set = choose_by_branch(
                        under_test=hub, set_branch_in=set_branch_in
                    )
                    resolved = by_name(hub_set)[hub.name]
                    self.assertEqual(resolved.origin, HubOrigin.CHECKOUT)
                    self.assertEqual(resolved.ref, "checkout")
                    self.assertEqual(resolved.commit, CHECKOUT_COMMIT)

    def test_every_hub_is_in_the_set_once_ordered_by_id(self):
        hub_set = choose_by_branch()
        self.assertEqual(
            [resolved.hub for resolved in hub_set.hubs], list(resolve.HUBS)
        )
        self.assertEqual(hub_set.notes, ())

    def test_a_sibling_without_main_is_refused(self):
        with self.assertRaises(ResolveError) as refused:
            resolve.choose_sibling(NODES_HUB, SET_NAME, {"other": "0" * 40})
        self.assertIn("nodes-hub has no branch `main`", str(refused.exception))

    def test_without_the_deploy_key_the_private_hub_is_left_out_and_said_so(self):
        hub_set = choose_by_branch(key=False, set_branch_in={"private-nodes-hub"})
        self.assertNotIn("private-nodes-hub", by_name(hub_set))
        self.assertEqual(len(hub_set.notes), 1)
        self.assertIn("private-nodes-hub is left out", hub_set.notes[0])
        self.assertIn(
            f"A branch `{SET_NAME}` of private-nodes-hub, if any, is not part of "
            "this run.",
            hub_set.notes[0],
        )

    def test_without_the_deploy_key_or_a_set_name_the_note_names_no_branch(self):
        hub_set = choose_by_branch(key=False, set_name=None)
        self.assertEqual(len(hub_set.notes), 1)
        self.assertNotIn("A branch", hub_set.notes[0])

    def test_the_private_hub_is_looked_up_only_with_the_deploy_key(self):
        self.assertIn(PRIVATE_NODES_HUB, resolve.sibling_hubs(NODES_HUB, True))
        self.assertNotIn(PRIVATE_NODES_HUB, resolve.sibling_hubs(NODES_HUB, False))

    def test_the_private_hub_under_test_needs_no_deploy_key(self):
        hub_set = choose_by_branch(under_test=PRIVATE_NODES_HUB, key=False)
        self.assertEqual(
            by_name(hub_set)["private-nodes-hub"].origin, HubOrigin.CHECKOUT
        )
        self.assertEqual(hub_set.notes, ())

    def test_the_branches_looked_up_are_main_and_the_set_name(self):
        self.assertEqual(resolve.branches_to_look_up("main", None), ["main"])
        self.assertEqual(
            resolve.branches_to_look_up("main", SET_NAME), ["main", SET_NAME]
        )


class LsRemote(unittest.TestCase):
    def test_heads_are_keyed_by_their_exact_branch_name(self):
        output = (
            f"{'1' * 40}\trefs/heads/main\n"
            f"{'2' * 40}\trefs/heads/feat/x\n"
            f"{'3' * 40}\trefs/heads/other/refs/heads/main\n"
            f"{'4' * 40}\trefs/tags/v1\n"
        )
        self.assertEqual(
            resolve.parse_ls_remote(output),
            {
                "main": "1" * 40,
                "feat/x": "2" * 40,
                "other/refs/heads/main": "3" * 40,
            },
        )

    def test_no_output_is_no_branch(self):
        self.assertEqual(resolve.parse_ls_remote(""), {})


class PeppyBuildChoice(unittest.TestCase):
    def test_the_latest_release_policy_installs_the_latest_release(self):
        self.assertEqual(
            resolve.choose_peppy_build(PeppyBuildPolicy.LATEST_RELEASE, False, None),
            PeppyBuildKind.LATEST_RELEASE,
        )

    def test_the_dev_builds_policy_installs_a_dev_build(self):
        self.assertEqual(
            resolve.choose_peppy_build(PeppyBuildPolicy.DEV_BUILDS, False, None),
            PeppyBuildKind.DEV_BUILD,
        )

    def test_a_fork_always_gets_the_latest_release_and_the_summary_says_so(self):
        for policy in PeppyBuildPolicy:
            with self.subTest(policy=policy):
                kind = resolve.choose_peppy_build(policy, True, None)
                self.assertEqual(kind, PeppyBuildKind.LATEST_RELEASE)
                (note,) = resolve.peppy_build_notes(kind, True)
                self.assertIn("fork", note)
                self.assertIn("latest release", note)

    def test_a_release_run_wins_over_every_policy(self):
        for policy in PeppyBuildPolicy:
            for from_fork in (False, True):
                with self.subTest(policy=policy, from_fork=from_fork):
                    self.assertEqual(
                        resolve.choose_peppy_build(policy, from_fork, 123),
                        PeppyBuildKind.RELEASE_RUN,
                    )

    def test_no_note_without_a_fork(self):
        for kind in PeppyBuildKind:
            with self.subTest(kind=kind):
                self.assertEqual(resolve.peppy_build_notes(kind, False), ())

    def test_the_policy_in_force_is_dev_builds(self):
        self.assertIs(resolve.PEPPY_BUILD_POLICY, PeppyBuildPolicy.DEV_BUILDS)


class RunnerArch(unittest.TestCase):
    def test_a_release_archive_matches_the_runner(self):
        for kind in (PeppyBuildKind.LATEST_RELEASE, PeppyBuildKind.RELEASE_RUN):
            for machine, arch in (
                ("x86_64", Arch.X86_64),
                ("aarch64", Arch.AARCH64),
                ("arm64", Arch.AARCH64),
            ):
                with self.subTest(kind=kind, machine=machine):
                    self.assertEqual(resolve.runner_arch(machine, kind), arch)

    def test_a_dev_build_needs_an_x86_64_runner(self):
        self.assertEqual(
            resolve.runner_arch("x86_64", PeppyBuildKind.DEV_BUILD), Arch.X86_64
        )
        for machine in ("aarch64", "armv7l"):
            with self.subTest(machine=machine):
                with self.assertRaises(ResolveError) as refused:
                    resolve.runner_arch(machine, PeppyBuildKind.DEV_BUILD)
                self.assertEqual(
                    str(refused.exception),
                    f"peppy dev builds exist for x86_64 only (this runner is "
                    f"`{machine}`); run this job on an x86_64 runner.",
                )

    def test_a_runner_peppy_has_no_archive_for_is_refused(self):
        with self.assertRaises(ResolveError):
            resolve.runner_arch("armv7l", PeppyBuildKind.LATEST_RELEASE)

    def test_the_archive_names_follow_the_architecture(self):
        self.assertEqual(
            resolve.latest_release_url(Arch.AARCH64),
            "https://peppy.bot/latest/peppy-aarch64-unknown-linux-gnu.tgz",
        )
        self.assertEqual(
            resolve.release_run_artifact_name(Arch.X86_64),
            "archive-x86_64-unknown-linux-gnu",
        )


class PeppyBranch(unittest.TestCase):
    PEPPY_HEADS = {"dev": PEPPY_DEV_COMMIT, SET_NAME: PEPPY_BRANCH_COMMIT}

    def test_peppy_with_a_branch_of_the_set_runs_its_head(self):
        self.assertEqual(
            resolve.choose_peppy_branch(SET_NAME, self.PEPPY_HEADS),
            (SET_NAME, PEPPY_BRANCH_COMMIT),
        )

    def test_peppy_without_a_branch_of_the_set_runs_the_head_of_dev(self):
        self.assertEqual(
            resolve.choose_peppy_branch(SET_NAME, {"dev": PEPPY_DEV_COMMIT}),
            ("dev", PEPPY_DEV_COMMIT),
        )

    def test_without_a_set_name_peppy_runs_the_head_of_dev(self):
        self.assertEqual(
            resolve.choose_peppy_branch(None, self.PEPPY_HEADS),
            ("dev", PEPPY_DEV_COMMIT),
        )

    def test_peppy_without_dev_is_refused(self):
        with self.assertRaises(ResolveError):
            resolve.choose_peppy_branch(SET_NAME, {})


class DevBuild(unittest.TestCase):
    def artifacts(self, *artifacts):
        return resolve.parse_artifacts(
            artifacts_response(resolve.DEV_BUILD_ARTIFACT, *artifacts),
            resolve.DEV_BUILD_ARTIFACT,
        )

    def test_the_most_recent_run_is_the_one_of_the_highest_id(self):
        run = resolve.latest_ci_run(
            runs_response((41, "completed"), (43, "in_progress"), (42, "completed")),
            SET_NAME,
            PEPPY_BRANCH_COMMIT,
        )
        self.assertEqual(run, resolve.CiRun(43, "in_progress", resolve.run_url(43)))

    def test_a_peppy_branch_of_the_set_with_no_ci_run_is_refused(self):
        with self.assertRaises(ResolveError) as refused:
            resolve.latest_ci_run(runs_response(), SET_NAME, PEPPY_BRANCH_COMMIT)
        self.assertEqual(
            str(refused.exception),
            f"peppy branch `{SET_NAME}` has no dev build; open a pull request for it.",
        )

    def test_a_dev_head_with_no_ci_run_is_refused(self):
        with self.assertRaises(ResolveError) as refused:
            resolve.latest_ci_run(runs_response(), "dev", PEPPY_DEV_COMMIT)
        self.assertIn(PEPPY_DEV_COMMIT, str(refused.exception))

    def test_the_uploaded_artifact_is_the_dev_build(self):
        run = resolve.CiRun(7, "completed", resolve.run_url(7))
        artifact = resolve.dev_build_artifact(
            run, self.artifacts((70, False)), SET_NAME, PEPPY_BRANCH_COMMIT
        )
        self.assertEqual(artifact.id, 70)

    def test_an_artifact_is_used_before_the_run_finishes(self):
        run = resolve.CiRun(7, "in_progress", resolve.run_url(7))
        artifact = resolve.dev_build_artifact(
            run, self.artifacts((70, False)), SET_NAME, PEPPY_BRANCH_COMMIT
        )
        self.assertEqual(artifact.id, 70)

    def test_a_re_run_artifact_is_used_over_the_expired_one(self):
        run = resolve.CiRun(7, "completed", resolve.run_url(7))
        artifact = resolve.dev_build_artifact(
            run, self.artifacts((70, True), (71, False)), SET_NAME, PEPPY_BRANCH_COMMIT
        )
        self.assertEqual(artifact.id, 71)

    def test_a_run_that_has_not_finished_is_not_ready(self):
        url = resolve.run_url(7)
        for artifacts in ((), ((70, True),)):
            with self.subTest(artifacts=artifacts):
                with self.assertRaises(ResolveError) as refused:
                    resolve.dev_build_artifact(
                        resolve.CiRun(7, "in_progress", url),
                        self.artifacts(*artifacts),
                        SET_NAME,
                        PEPPY_BRANCH_COMMIT,
                    )
                self.assertEqual(
                    str(refused.exception),
                    f"peppy dev build for `{PEPPY_BRANCH_COMMIT}` is not ready (run "
                    f"`{url}` is `in_progress`); re-run this job when it finishes.",
                )

    def test_a_finished_run_without_the_artifact_produced_no_dev_build(self):
        url = resolve.run_url(7)
        with self.assertRaises(ResolveError) as refused:
            resolve.dev_build_artifact(
                resolve.CiRun(7, "completed", url),
                self.artifacts(),
                SET_NAME,
                PEPPY_BRANCH_COMMIT,
            )
        self.assertEqual(
            str(refused.exception),
            f"peppy CI run `{url}` produced no dev build; fix peppy CI first.",
        )

    def test_an_expired_artifact_is_refused_with_the_way_to_rebuild_it(self):
        url = resolve.run_url(7)
        with self.assertRaises(ResolveError) as refused:
            resolve.dev_build_artifact(
                resolve.CiRun(7, "completed", url),
                self.artifacts((70, True)),
                SET_NAME,
                PEPPY_BRANCH_COMMIT,
            )
        self.assertEqual(
            str(refused.exception),
            f"the peppy dev build for `{PEPPY_BRANCH_COMMIT}` expired; re-run peppy "
            f"CI run `{url}`, or push a new commit to `{SET_NAME}` if GitHub no "
            "longer offers the re-run.",
        )

    def test_only_artifacts_of_the_name_count(self):
        response = artifacts_response("install-archive", (70, False))
        self.assertEqual(
            resolve.parse_artifacts(response, resolve.DEV_BUILD_ARTIFACT), []
        )


class ReleaseRun(unittest.TestCase):
    NAME = resolve.release_run_artifact_name(Arch.AARCH64)
    URL = resolve.run_url(9)

    def artifacts(self, *artifacts):
        return resolve.parse_artifacts(
            artifacts_response(self.NAME, *artifacts), self.NAME
        )

    def test_the_run_archive_of_the_runner_is_used(self):
        artifact = resolve.release_run_artifact(
            self.URL, self.artifacts((90, False)), self.NAME
        )
        self.assertEqual(artifact.id, 90)

    def test_a_release_run_without_the_archive_is_refused(self):
        with self.assertRaises(ResolveError) as refused:
            resolve.release_run_artifact(self.URL, self.artifacts(), self.NAME)
        self.assertIn(self.NAME, str(refused.exception))

    def test_an_expired_release_run_archive_is_refused(self):
        with self.assertRaises(ResolveError) as refused:
            resolve.release_run_artifact(
                self.URL, self.artifacts((90, True)), self.NAME
            )
        self.assertIn("expired", str(refused.exception))


class ExplicitSet(unittest.TestCase):
    def refusal(self, text):
        with self.assertRaises(ResolveError) as refused:
            resolve.parse_explicit_set(text)
        return str(refused.exception)

    def test_an_explicit_set_is_used_as_given(self):
        pins = resolve.parse_explicit_set(explicit_set_text())
        hub_set = resolve.choose_hubs_from_explicit_set(
            pins, NODES_HUB, CHECKOUT_COMMIT, True
        )
        self.assertIsNone(hub_set.name)
        self.assertEqual(hub_set.notes, ())
        resolved = by_name(hub_set)
        self.assertEqual(list(resolved), [hub.name for hub in resolve.HUBS])
        self.assertEqual(resolved["nodes-hub"].origin, HubOrigin.CHECKOUT)
        self.assertEqual(resolved["nodes-hub"].ref, "checkout")
        self.assertEqual(resolved["nodes-hub"].commit, CHECKOUT_COMMIT)
        for hub in resolve.HUBS:
            if hub is NODES_HUB:
                continue
            with self.subTest(hub=hub.name):
                self.assertEqual(resolved[hub.name].origin, HubOrigin.EXPLICIT_SET)
                self.assertEqual(resolved[hub.name].ref, "peppy-release/v0.31.2")
                self.assertEqual(resolved[hub.name].commit, commit_of(hub, "main"))

    def test_the_private_hub_of_an_explicit_set_needs_the_deploy_key(self):
        pins = resolve.parse_explicit_set(explicit_set_text())
        hub_set = resolve.choose_hubs_from_explicit_set(
            pins, NODES_HUB, CHECKOUT_COMMIT, False
        )
        self.assertNotIn("private-nodes-hub", by_name(hub_set))
        (note,) = hub_set.notes
        self.assertIn("private-nodes-hub is left out", note)
        self.assertIn("no deploy key", note)

    def test_an_explicit_set_may_leave_out_the_private_hub(self):
        pins = resolve.parse_explicit_set(
            explicit_set_text(left_out=["private-nodes-hub"])
        )
        self.assertNotIn(PRIVATE_NODES_HUB, pins)
        hub_set = resolve.choose_hubs_from_explicit_set(
            pins, NODES_HUB, CHECKOUT_COMMIT, True
        )
        self.assertNotIn("private-nodes-hub", by_name(hub_set))
        (note,) = hub_set.notes
        self.assertIn("does not name it", note)

    def test_an_explicit_set_that_is_not_json_is_refused(self):
        self.assertIn("is not JSON", self.refusal("{hubs: "))

    def test_an_explicit_set_of_the_wrong_shape_is_refused(self):
        for text in (
            "[]",
            "{}",
            '{"hubs": []}',
            json.dumps({"hubs": {}, "name": "x"}),
        ):
            with self.subTest(text=text):
                self.assertIn("must have the shape", self.refusal(text))

    def test_an_explicit_set_entry_of_the_wrong_shape_is_refused(self):
        commit = commit_of(NODES_HUB, "main")
        for entry in (
            "main",
            {"commit": commit},
            {"ref": "main", "commit": commit, "extra": 1},
            {"ref": "", "commit": commit},
            {"ref": 1, "commit": commit},
        ):
            with self.subTest(entry=entry):
                self.assertIn(
                    "entry of contracts-hub must have the shape",
                    self.refusal(explicit_set_text({"contracts-hub": entry})),
                )

    def test_an_explicit_set_naming_an_unknown_hub_is_refused(self):
        refusal = self.refusal(
            explicit_set_text({"peppy": {"ref": "dev", "commit": "0" * 40}})
        )
        self.assertIn("unknown hub `peppy`", refusal)
        for hub in resolve.HUBS:
            self.assertIn(hub.name, refusal)

    def test_an_explicit_set_with_a_bad_commit_is_refused(self):
        for commit in ("A" * 40, "a" * 39, "a" * 41, "main", "g" * 40):
            with self.subTest(commit=commit):
                refusal = self.refusal(
                    explicit_set_text({"mcp-hub": {"ref": "main", "commit": commit}})
                )
                self.assertIn(f"records `{commit}` for mcp-hub", refusal)

    def test_an_explicit_set_leaving_out_a_public_hub_is_refused(self):
        for hub in PUBLIC_HUBS:
            with self.subTest(hub=hub.name):
                refusal = self.refusal(explicit_set_text(left_out=[hub.name]))
                self.assertIn(f"leaves out {hub.name}", refusal)

    def test_an_explicit_set_leaving_out_the_hub_under_test_is_refused(self):
        pins = resolve.parse_explicit_set(
            explicit_set_text(left_out=["private-nodes-hub"])
        )
        with self.assertRaises(ResolveError) as refused:
            resolve.choose_hubs_from_explicit_set(
                pins, PRIVATE_NODES_HUB, CHECKOUT_COMMIT, True
            )
        self.assertIn("leaves out private-nodes-hub", str(refused.exception))

    def test_a_checkout_the_explicit_set_does_not_record_is_refused(self):
        pins = resolve.parse_explicit_set(explicit_set_text())
        other = "e" * 40
        with self.assertRaises(ResolveError) as refused:
            resolve.choose_hubs_from_explicit_set(pins, NODES_HUB, other, True)
        self.assertEqual(
            str(refused.exception),
            f"the checkout of nodes-hub is at {other}, but the set records "
            f"{CHECKOUT_COMMIT}",
        )


class JobFiles(unittest.TestCase):
    HUB_PATH = "/runner/work/nodes-hub/nodes-hub"

    def test_the_repositories_file_pins_every_hub_of_the_set(self):
        hub_set = choose_by_branch(set_branch_in={"contracts-hub"})
        entries = json.loads(resolve.repositories_file(hub_set, self.HUB_PATH))
        self.assertEqual(
            [entry["id"] for entry in entries], [1000, 1001, 1002, 1003, 1004, 2000]
        )
        by_id = {entry["id"]: entry for entry in entries}
        self.assertEqual(by_id[1000], {"id": 1000, "type": "fs", "path": self.HUB_PATH})
        contracts_hub = resolve.HUBS_BY_NAME["contracts-hub"]
        self.assertEqual(
            by_id[1002],
            {
                "id": 1002,
                "type": "git",
                "url": "https://github.com/Peppy-bot/contracts-hub.git",
                "ref": commit_of(contracts_hub, SET_NAME),
            },
        )
        for name in ("launchers-hub", "mcp-hub", "pairings-hub"):
            hub = resolve.HUBS_BY_NAME[name]
            with self.subTest(hub=name):
                self.assertEqual(
                    by_id[hub.repository_id],
                    {
                        "id": hub.repository_id,
                        "type": "git",
                        "url": f"https://github.com/Peppy-bot/{name}.git",
                        "ref": commit_of(hub, "main"),
                    },
                )
        self.assertEqual(
            by_id[2000],
            {
                "id": 2000,
                "type": "git",
                "url": "git@github.com:Peppy-bot/private-nodes-hub.git",
                "ref": commit_of(PRIVATE_NODES_HUB, "main"),
            },
        )

    def test_the_repositories_file_leaves_out_what_the_set_leaves_out(self):
        hub_set = choose_by_branch(key=False)
        entries = json.loads(resolve.repositories_file(hub_set, self.HUB_PATH))
        self.assertEqual(
            [entry["id"] for entry in entries], [1000, 1001, 1002, 1003, 1004]
        )

    def test_the_set_output_follows_the_schema(self):
        hub_set = choose_by_branch(set_branch_in={"contracts-hub"})
        output = json.loads(
            resolve.compact_json(resolve.set_output(hub_set, installed_peppy()))
        )
        self.assertEqual(output["name"], SET_NAME)
        self.assertEqual(
            output["peppy"],
            {
                "build": "latest-release",
                "version": "peppy v0.31.3",
                "source": "https://peppy.bot/latest/peppy-x86_64-unknown-linux-gnu.tgz",
            },
        )
        self.assertEqual(list(output["hubs"]), [hub.name for hub in resolve.HUBS])
        self.assertEqual(
            output["hubs"]["nodes-hub"], {"ref": "checkout", "commit": CHECKOUT_COMMIT}
        )
        self.assertEqual(
            output["hubs"]["contracts-hub"],
            {
                "ref": SET_NAME,
                "commit": commit_of(resolve.HUBS_BY_NAME["contracts-hub"], SET_NAME),
            },
        )

    def test_the_set_output_is_one_line(self):
        hub_set = choose_by_branch(set_name=None)
        text = resolve.compact_json(resolve.set_output(hub_set, installed_peppy()))
        self.assertNotIn("\n", text)
        self.assertIsNone(json.loads(text)["name"])

    def test_the_summary_names_the_peppy_build_and_every_hub(self):
        hub_set = choose_by_branch(key=False, set_branch_in={"contracts-hub"})
        summary = resolve.summary_markdown(
            hub_set,
            installed_peppy(),
            "the head branch of this pull request",
            resolve.peppy_build_notes(PeppyBuildKind.LATEST_RELEASE, True),
        )
        self.assertIn(
            f"Set `{SET_NAME}`: the head branch of this pull request.", summary
        )
        self.assertIn("`peppy v0.31.3`, the latest release", summary)
        rows = [line for line in summary.splitlines() if line.startswith("| ")]
        # The header, its rule, and one row per hub.
        self.assertEqual(len(rows), 2 + len(hub_set.hubs))
        self.assertIn(
            f"| contracts-hub | 1002 | its branch of the set | `{SET_NAME}` "
            f"| `{commit_of(resolve.HUBS_BY_NAME['contracts-hub'], SET_NAME)}` |",
            rows,
        )
        self.assertIn(
            f"| nodes-hub | 1000 | this checkout, the hub under test | `checkout` "
            f"| `{CHECKOUT_COMMIT}` |",
            rows,
        )
        self.assertIn("- A pull request from a fork", summary)
        self.assertIn("- private-nodes-hub is left out", summary)

    def test_the_summary_without_a_set_name_says_why(self):
        summary = resolve.summary_markdown(
            choose_by_branch(set_name=None),
            installed_peppy(),
            "this run is a `push`",
            (),
        )
        self.assertIn("No set name: this run is a `push`.", summary)

    def test_an_error_stays_one_workflow_command(self):
        self.assertEqual(
            resolve.escape_workflow_command("100% of\nbranch"), "100%25 of%0Abranch"
        )


def json5_to_json(text):
    """The JSON5 of default_repositories.json5 as JSON: `//` and `/* */`
    comments dropped, bare keys quoted, trailing commas removed. Strings are
    copied through untouched, so the `//` of a URL stays."""
    return drop_trailing_commas(quote_bare_keys(drop_comments(text)))


def string_end(text, start):
    """The index just past the double-quoted string starting at `start`."""
    index = start + 1
    while text[index] != '"':
        index += 2 if text[index] == "\\" else 1
    return index + 1


def drop_comments(text):
    out, index = [], 0
    while index < len(text):
        if text[index] == '"':
            end = string_end(text, index)
            out.append(text[index:end])
            index = end
        elif text.startswith("//", index):
            newline = text.find("\n", index)
            index = len(text) if newline == -1 else newline
        elif text.startswith("/*", index):
            index = text.index("*/", index + 2) + 2
        else:
            out.append(text[index])
            index += 1
    return "".join(out)


BARE_KEY = re.compile(r"[A-Za-z_$][A-Za-z0-9_$]*(?=\s*:)")


def quote_bare_keys(text):
    out, index = [], 0
    while index < len(text):
        if text[index] == '"':
            end = string_end(text, index)
            out.append(text[index:end])
            index = end
            continue
        key = BARE_KEY.match(text, index)
        if key:
            out.append(json.dumps(key.group()))
            index = key.end()
            continue
        out.append(text[index])
        index += 1
    return "".join(out)


TRAILING_COMMA = re.compile(r",(?=\s*[\]}])")


def drop_trailing_commas(text):
    out, index = [], 0
    while index < len(text):
        if text[index] == '"':
            end = string_end(text, index)
            out.append(text[index:end])
            index = end
            continue
        if TRAILING_COMMA.match(text, index):
            index += 1
            continue
        out.append(text[index])
        index += 1
    return "".join(out)


class RepositoryFacts(unittest.TestCase):
    def test_the_json5_reader_reads_what_the_defaults_file_is_written_in(self):
        text = """
        // a comment with "quotes" and a trailing comma,
        [
          /* block */ { id: 1, type: "git", url: "https://x//y", ref: "a,]" },
        ]
        """
        self.assertEqual(
            json.loads(json5_to_json(text)),
            [{"id": 1, "type": "git", "url": "https://x//y", "ref": "a,]"}],
        )

    def test_the_public_hubs_are_the_defaults_peppy_bundles(self):
        defaults_path = (
            REPOSITORY_ROOT
            / "crates/core-node-internal/assets/default_repositories.json5"
        )
        defaults = json.loads(json5_to_json(defaults_path.read_text()))
        self.assertEqual(
            {entry["id"]: entry["url"] for entry in defaults},
            {hub.repository_id: hub.clone_url for hub in PUBLIC_HUBS},
        )

    def test_the_private_hub_is_in_the_reserved_id_band(self):
        self.assertGreaterEqual(PRIVATE_NODES_HUB.repository_id, 2000)
        self.assertEqual(
            PRIVATE_NODES_HUB.clone_url,
            "git@github.com:Peppy-bot/private-nodes-hub.git",
        )

    def assert_workflow_has_line(self, workflow, line):
        path = REPOSITORY_ROOT / ".github/workflows" / workflow
        lines = [text.strip() for text in path.read_text().splitlines()]
        self.assertTrue(line in lines, f"{path} has no line `{line}`")

    def test_peppy_ci_uploads_the_dev_build_this_action_downloads(self):
        workflow = resolve.PEPPY_CI_WORKFLOW
        self.assert_workflow_has_line(workflow, f"name: {resolve.DEV_BUILD_ARTIFACT}")
        self.assert_workflow_has_line(
            workflow, f"path: dist/{Arch.X86_64.archive_name}"
        )

    def test_the_release_uploads_the_archives_this_action_downloads(self):
        workflow = "parallel-release.yml"
        name_template = "archive-${{ matrix.target }}"
        self.assert_workflow_has_line(workflow, f"name: {name_template}")
        self.assert_workflow_has_line(
            workflow, "path: dist/peppy-${{ matrix.target }}.tgz"
        )
        for arch in Arch:
            with self.subTest(arch=arch):
                self.assert_workflow_has_line(workflow, f"- target: {arch.triple}")
                self.assertEqual(
                    resolve.release_run_artifact_name(arch),
                    name_template.replace("${{ matrix.target }}", arch.triple),
                )

    def test_the_action_forwards_every_output_the_script_writes(self):
        # A composite action forwards only the outputs it declares, and an
        # undeclared one reaches the workflow as the empty string.
        action = (ACTION_DIR / "action.yml").read_text()
        for output in ("set", "peppy-version"):
            with self.subTest(output=output):
                self.assertIn(
                    f"value: ${{{{ steps.resolve.outputs.{output} }}}}", action
                )

    def test_the_action_gives_the_script_every_variable_it_reads(self):
        action = (ACTION_DIR / "action.yml").read_text()
        for variable in resolve.ACTION_VARIABLES:
            with self.subTest(variable=variable):
                self.assertRegex(action, rf"\n\s+{variable}: ")


if __name__ == "__main__":
    unittest.main()
