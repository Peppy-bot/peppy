"""Tests for functions.parallel_release."""

from __future__ import annotations

import json
import subprocess
import tarfile
from pathlib import Path
from unittest.mock import MagicMock, patch

import pytest

from functions.build import BuildArtifact
from functions.cli import ReleaseError
from functions.docs import CheckResult, RequiredChange, UpdateOutcome, UpdateResult
from functions.github import RepoSlug
from functions.lima import RELEASE_PLATFORM_SO, SO_BUILD_STATE_MARKER
from functions.parallel_release import (
    BINDINGS_ARCHIVE,
    ReleasePlan,
    _apptainer_arches_for,
    _parse_args,
    _require_unused_tag,
    _write_tarball,
    apptainer_archive_name,
    run_apptainer,
    run_bindings,
    run_build,
    run_prepare,
    run_publish,
    verify_docs_gate,
)
from functions.release_summary import ReleaseContent

RELEASE_COMMIT = "1111111111111111111111111111111111111111"
OTHER_COMMIT = "2222222222222222222222222222222222222222"
CONTENT = ReleaseContent(
    title="Topics API hardening",
    description="Hardened the topics API against deadlocks.",
    notes="## What's Changed\n- Fixed topics public API\n",
)
PLAN = ReleasePlan(tag="v0.3.0", release_commit=RELEASE_COMMIT, content=CONTENT)
SLUG = RepoSlug(owner="test-owner", repo="test-repo")
BLOCKING = RequiredChange(file="docs/x.mdx", change="document --verbose", severity="blocking")
MINOR = RequiredChange(file="docs/y.mdx", change="reword the intro", severity="minor")
APPTAINER_CACHE = "apptainer-1.5.2-{arch}-nosuid"


@pytest.fixture(autouse=True)
def _isolated_host(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Keep every stage off the real home, dist directory and working directory."""
    monkeypatch.setenv("HOME", str(tmp_path / "home"))
    monkeypatch.delenv("PEPPY_DIST_DIR", raising=False)
    # Registered so the value the build stage sets is undone after each test.
    monkeypatch.setenv("PEPPYLIB_PREBUILT_SO_DIR", "")
    monkeypatch.chdir(tmp_path)


@pytest.fixture(autouse=True)
def _no_prompts() -> object:
    """No stage may ask anything: every prompt fails the test."""
    with patch(
        "functions.cli.Prompt.ask", side_effect=AssertionError("prompted")
    ), patch("functions.cli.Confirm.ask", side_effect=AssertionError("prompted")):
        yield


def _unwrapped(output: str) -> str:
    return " ".join(output.split())


def _cache_root(tmp_path: Path) -> Path:
    return tmp_path / "home" / ".peppy" / "tmp"


def _fake_apptainer_cache(root: Path, arch: str) -> Path:
    cache = root / APPTAINER_CACHE.format(arch=arch)
    (cache / "bin").mkdir(parents=True)
    binary = cache / "bin" / "apptainer"
    binary.write_bytes(f"apptainer for {arch}".encode())
    binary.chmod(0o755)
    (cache / ".peppy-version-1.5.2-r3-sq0.6.1").write_text("built\n")
    return cache


def _fake_bindings_dir(directory: Path) -> Path:
    directory.mkdir(parents=True, exist_ok=True)
    for name in (*RELEASE_PLATFORM_SO, SO_BUILD_STATE_MARKER):
        (directory / name).write_text(f"contents of {name}")
    return directory


def _provisioned(tmp_path: Path, arches: tuple[str, ...]) -> Path:
    """A provisioned directory as the provision jobs leave it."""
    provisioned = tmp_path / "provisioned"
    bindings = _fake_bindings_dir(tmp_path / "producer" / "so")
    _write_tarball(
        provisioned / BINDINGS_ARCHIVE,
        [(bindings / name, f"so/{name}") for name in (*RELEASE_PLATFORM_SO, SO_BUILD_STATE_MARKER)],
    )
    for arch in arches:
        cache = _fake_apptainer_cache(tmp_path / "producer" / arch, arch)
        _write_tarball(provisioned / apptainer_archive_name(arch), [(cache, cache.name)])
    return provisioned


# --- the release plan ---


def test_release_plan_round_trips(tmp_path: Path) -> None:
    path = tmp_path / "plan" / "release-plan.json"

    PLAN.write(path)

    assert ReleasePlan.load(path) == PLAN


def test_release_plan_rejects_a_missing_field(tmp_path: Path) -> None:
    path = tmp_path / "release-plan.json"
    payload = {"tag": "v0.3.0", "release_commit": RELEASE_COMMIT, "title": "T", "description": "D"}
    path.write_text(json.dumps(payload))

    with pytest.raises(ReleaseError, match="no usable 'notes'"):
        ReleasePlan.load(path)


def test_release_plan_rejects_a_file_that_is_not_json(tmp_path: Path) -> None:
    path = tmp_path / "release-plan.json"
    path.write_text("not json")

    with pytest.raises(ReleaseError, match="cannot read the release plan"):
        ReleasePlan.load(path)


# --- command line ---


def test_prepare_takes_every_answer_up_front() -> None:
    args = _parse_args(
        [
            "prepare",
            "--tag",
            " v0.3.0 ",
            "--release-commit",
            RELEASE_COMMIT.upper(),
            "--no-minor-docs-pr",
            "--skip-prod-cert-check",
            "--plan",
            "plan.json",
        ]
    )

    assert args.tag == "v0.3.0"
    assert args.release_commit == RELEASE_COMMIT
    assert args.minor_docs_pr is False
    assert args.skip_prod_cert_check is True
    assert args.skip_docs_check is False


@pytest.mark.parametrize(
    "argv",
    [
        # The minor-docs offer has no default: it must be answered.
        ["--tag", "v1", "--release-commit", RELEASE_COMMIT, "--plan", "p"],
        ["--tag", "  ", "--release-commit", RELEASE_COMMIT, "--minor-docs-pr", "--plan", "p"],
        ["--tag", "v1", "--release-commit", "abc123", "--minor-docs-pr", "--plan", "p"],
    ],
)
def test_prepare_rejects_missing_or_malformed_answers(argv: list[str]) -> None:
    with pytest.raises(SystemExit) as exc_info:
        _parse_args(["prepare", *argv])
    assert exc_info.value.code == 2


def test_build_accepts_release_targets_only() -> None:
    with pytest.raises(SystemExit):
        _parse_args(["build", "--plan", "p", "--target", "riscv64gc-unknown-linux-gnu", "--provisioned", "d"])


def test_the_macos_build_needs_apptainer_of_both_linux_architectures() -> None:
    assert _apptainer_arches_for("aarch64-apple-darwin") == ("aarch64", "x86_64")
    assert _apptainer_arches_for("aarch64-unknown-linux-gnu") == ("aarch64",)
    assert _apptainer_arches_for("x86_64-unknown-linux-gnu") == ("x86_64",)


# --- prepare ---


@patch("functions.parallel_release.subprocess.run")
def test_require_unused_tag_accepts_a_new_tag(mock_run: MagicMock) -> None:
    mock_run.return_value = subprocess.CompletedProcess([], 0, stdout="", stderr="")

    _require_unused_tag("v0.3.0")

    assert mock_run.call_args.args[0][-1] == "refs/tags/v0.3.0"


@patch("functions.parallel_release.subprocess.run")
def test_require_unused_tag_rejects_an_existing_tag(mock_run: MagicMock) -> None:
    mock_run.return_value = subprocess.CompletedProcess(
        [], 0, stdout=f"{OTHER_COMMIT}\trefs/tags/v0.3.0\n", stderr=""
    )

    with pytest.raises(ReleaseError, match="already exists"):
        _require_unused_tag("v0.3.0")


def _prepare(tmp_path: Path, **overrides: object) -> Path:
    plan_path = tmp_path / "out" / "release-plan.json"
    options = {
        "tag": "v0.3.0",
        "release_commit": RELEASE_COMMIT,
        "open_minor_docs_pr": False,
        "skip_prod_cert_check": False,
        "skip_docs_check": False,
        "plan_path": plan_path,
    }
    options.update(overrides)
    run_prepare(**options)  # type: ignore[arg-type]
    return plan_path


@pytest.fixture
def prepare_mocks(tmp_path: Path):
    with patch(
        "functions.parallel_release.validate_release_environment", return_value="token"
    ) as validate, patch(
        "functions.parallel_release.get_repo_root", return_value=tmp_path
    ), patch(
        "functions.parallel_release._verify_release_branch_state",
        return_value=RELEASE_COMMIT,
    ) as branch_state, patch(
        "functions.parallel_release._require_unused_tag"
    ) as unused_tag, patch(
        "functions.parallel_release.github_repo_slug", return_value=SLUG
    ), patch(
        "functions.parallel_release.build_github_client"
    ) as client, patch(
        "functions.parallel_release.verify_docs_gate"
    ) as docs_gate, patch(
        "functions.parallel_release.get_latest_release",
        return_value={"tag_name": "v0.2.0"},
    ), patch(
        "functions.parallel_release.collect_release_changes"
    ) as collect, patch(
        "functions.parallel_release.generate_release_content", return_value=CONTENT
    ):
        yield MagicMock(
            validate=validate,
            branch_state=branch_state,
            unused_tag=unused_tag,
            client=client,
            docs_gate=docs_gate,
            collect=collect,
        )


def test_prepare_writes_the_plan_without_asking_anything(
    tmp_path: Path, prepare_mocks: MagicMock, capfd: pytest.CaptureFixture[str]
) -> None:
    plan_path = _prepare(tmp_path, open_minor_docs_pr=True, skip_prod_cert_check=True)

    assert ReleasePlan.load(plan_path) == PLAN
    prepare_mocks.validate.assert_called_once_with(
        required_commands=("git", "claude"),
        skip_prod_router_check=True,
    )
    prepare_mocks.unused_tag.assert_called_once_with("v0.3.0")
    prepare_mocks.docs_gate.assert_called_once_with(
        prepare_mocks.client.return_value,
        SLUG,
        RELEASE_COMMIT,
        tmp_path,
        open_minor_docs_pr=True,
    )
    # The notes cover the changes since the last published release.
    prepare_mocks.collect.assert_called_once_with("v0.2.0", RELEASE_COMMIT, tmp_path)
    # Nobody reviews the draft, so the log shows it.
    output = _unwrapped(capfd.readouterr().err)
    assert CONTENT.title in output
    assert CONTENT.description in output


def test_prepare_stops_when_dev_is_not_at_the_commit_the_run_builds(
    tmp_path: Path, prepare_mocks: MagicMock
) -> None:
    prepare_mocks.branch_state.return_value = OTHER_COMMIT

    with pytest.raises(ReleaseError, match="start a new run"):
        _prepare(tmp_path)

    prepare_mocks.docs_gate.assert_not_called()
    assert not (tmp_path / "out" / "release-plan.json").exists()


def test_prepare_skips_the_docs_gate_when_asked(
    tmp_path: Path, prepare_mocks: MagicMock
) -> None:
    plan_path = _prepare(tmp_path, skip_docs_check=True)

    prepare_mocks.docs_gate.assert_not_called()
    assert plan_path.exists()


# --- the docs gate ---


def _gate(tmp_path: Path, *, open_minor_docs_pr: bool) -> None:
    verify_docs_gate(
        MagicMock(), SLUG, RELEASE_COMMIT, tmp_path, open_minor_docs_pr=open_minor_docs_pr
    )


def _update_result(status: str, change: RequiredChange) -> UpdateResult:
    return UpdateResult(
        results=(UpdateOutcome(file=change.file, change=change.change, status=status),),
        summary="updated",
    )


@patch("functions.parallel_release.has_changes_in_paths", return_value=True)
def test_docs_gate_refuses_uncommitted_docs(mock_changes: MagicMock, tmp_path: Path) -> None:
    with pytest.raises(ReleaseError, match="uncommitted changes"):
        _gate(tmp_path, open_minor_docs_pr=False)


@patch("functions.parallel_release._open_docs_pr", return_value="https://github.com/o/r/pull/7")
@patch("functions.parallel_release._push_docs_sync_branch")
@patch("functions.parallel_release._find_open_docs_sync_pr", return_value=None)
@patch("functions.parallel_release.update_docs")
@patch("functions.parallel_release.check_docs", return_value=CheckResult(changes=(BLOCKING,)))
@patch("functions.parallel_release.has_changes_in_paths", side_effect=[False, True])
def test_docs_gate_opens_the_sync_pr_and_stops_on_a_blocking_gap(
    mock_changes: MagicMock,
    mock_check: MagicMock,
    mock_update: MagicMock,
    mock_find: MagicMock,
    mock_push: MagicMock,
    mock_open: MagicMock,
    tmp_path: Path,
) -> None:
    mock_update.return_value = _update_result("implemented", BLOCKING)

    with pytest.raises(SystemExit) as exc_info:
        _gate(tmp_path, open_minor_docs_pr=False)

    assert exc_info.value.code == 1
    branch = f"auto/docs-update-{RELEASE_COMMIT[:12]}"
    mock_update.assert_called_once_with("origin/main", RELEASE_COMMIT, (BLOCKING,))
    mock_push.assert_called_once_with(
        branch, tmp_path / "docs", "docs: sync with the code being released"
    )
    assert mock_open.call_args.args[2] == branch


@patch("functions.parallel_release.update_docs")
@patch("functions.parallel_release._find_open_docs_sync_pr", return_value="https://github.com/o/r/pull/6")
@patch("functions.parallel_release.check_docs", return_value=CheckResult(changes=(BLOCKING,)))
@patch("functions.parallel_release.has_changes_in_paths", return_value=False)
def test_docs_gate_stops_on_the_sync_pr_already_open(
    mock_changes: MagicMock,
    mock_check: MagicMock,
    mock_find: MagicMock,
    mock_update: MagicMock,
    tmp_path: Path,
) -> None:
    with pytest.raises(SystemExit):
        _gate(tmp_path, open_minor_docs_pr=False)

    mock_update.assert_not_called()


@patch("functions.parallel_release._find_open_docs_sync_pr", return_value=None)
@patch("functions.parallel_release.update_docs")
@patch("functions.parallel_release.check_docs", return_value=CheckResult(changes=(BLOCKING,)))
@patch("functions.parallel_release.has_changes_in_paths", return_value=False)
def test_docs_gate_continues_when_every_gap_is_already_documented(
    mock_changes: MagicMock,
    mock_check: MagicMock,
    mock_update: MagicMock,
    mock_find: MagicMock,
    tmp_path: Path,
) -> None:
    mock_update.return_value = _update_result("already_covered", BLOCKING)

    _gate(tmp_path, open_minor_docs_pr=False)


@patch("functions.parallel_release._find_open_docs_sync_pr", return_value=None)
@patch("functions.parallel_release.update_docs")
@patch("functions.parallel_release.check_docs", return_value=CheckResult(changes=(BLOCKING,)))
@patch("functions.parallel_release.has_changes_in_paths", return_value=False)
def test_docs_gate_fails_when_the_update_changes_nothing(
    mock_changes: MagicMock,
    mock_check: MagicMock,
    mock_update: MagicMock,
    mock_find: MagicMock,
    tmp_path: Path,
) -> None:
    mock_update.return_value = _update_result("implemented", BLOCKING)

    with pytest.raises(ReleaseError, match="nothing changed there"):
        _gate(tmp_path, open_minor_docs_pr=False)


@patch("functions.parallel_release._find_open_docs_sync_pr")
@patch("functions.parallel_release.update_docs")
@patch("functions.parallel_release.check_docs", return_value=CheckResult(changes=(MINOR,)))
@patch("functions.parallel_release.has_changes_in_paths", return_value=False)
def test_docs_gate_leaves_minor_suggestions_alone_when_declined(
    mock_changes: MagicMock,
    mock_check: MagicMock,
    mock_update: MagicMock,
    mock_find: MagicMock,
    tmp_path: Path,
    capfd: pytest.CaptureFixture[str],
) -> None:
    _gate(tmp_path, open_minor_docs_pr=False)

    mock_find.assert_not_called()
    mock_update.assert_not_called()
    # Still printed for whoever reads the log.
    assert "reword the intro" in capfd.readouterr().err


@patch("functions.parallel_release._open_docs_pr", return_value="https://github.com/o/r/pull/8")
@patch("functions.parallel_release._push_docs_sync_branch")
@patch("functions.parallel_release._find_open_docs_sync_pr", return_value=None)
@patch("functions.parallel_release.update_docs")
@patch("functions.parallel_release.check_docs", return_value=CheckResult(changes=(MINOR,)))
@patch("functions.parallel_release.has_changes_in_paths", side_effect=[False, True])
def test_docs_gate_opens_the_polish_pr_when_accepted_and_continues(
    mock_changes: MagicMock,
    mock_check: MagicMock,
    mock_update: MagicMock,
    mock_find: MagicMock,
    mock_push: MagicMock,
    mock_open: MagicMock,
    tmp_path: Path,
    capfd: pytest.CaptureFixture[str],
) -> None:
    mock_update.return_value = _update_result("implemented", MINOR)

    _gate(tmp_path, open_minor_docs_pr=True)

    branch = f"auto/docs-polish-{RELEASE_COMMIT[:12]}"
    mock_push.assert_called_once_with(branch, tmp_path / "docs", "docs: minor polish")
    assert mock_open.call_args.args[2] == branch
    assert "does not block" in _unwrapped(capfd.readouterr().err)


@patch("functions.parallel_release.update_docs")
@patch("functions.parallel_release._find_open_docs_sync_pr", return_value="https://github.com/o/r/pull/5")
@patch("functions.parallel_release.check_docs", return_value=CheckResult(changes=(MINOR,)))
@patch("functions.parallel_release.has_changes_in_paths", return_value=False)
def test_docs_gate_reuses_the_polish_pr_already_open(
    mock_changes: MagicMock,
    mock_check: MagicMock,
    mock_find: MagicMock,
    mock_update: MagicMock,
    tmp_path: Path,
) -> None:
    _gate(tmp_path, open_minor_docs_pr=True)

    mock_update.assert_not_called()


# --- provisioning ---


@patch("functions.parallel_release.is_macos_arm64", return_value=False)
def test_bindings_refuse_to_build_off_macos(mock_macos: MagicMock, tmp_path: Path) -> None:
    with pytest.raises(ReleaseError, match="macOS ARM64 only"):
        run_bindings(tmp_path / "out")


@patch("functions.parallel_release.need_cmd")
@patch("functions.parallel_release.subprocess.run")
@patch("functions.parallel_release.get_repo_root")
@patch("functions.parallel_release.is_macos_arm64", return_value=True)
def test_bindings_build_every_platform_and_pack_them(
    mock_macos: MagicMock,
    mock_repo_root: MagicMock,
    mock_run: MagicMock,
    mock_need_cmd: MagicMock,
    tmp_path: Path,
) -> None:
    mock_repo_root.return_value = tmp_path
    mock_run.return_value = subprocess.CompletedProcess([], 0)
    # What the generator build script leaves in the build cache.
    _fake_bindings_dir(_cache_root(tmp_path) / "peppylib-py" / "so")

    run_bindings(tmp_path / "out")

    command = mock_run.call_args.args[0]
    assert command[:4] == ["cargo", "build", "-p", "generator"]
    assert "--release" in command and "--locked" in command
    # Fresh bindings, compiled with the release RUSTFLAGS.
    env = mock_run.call_args.kwargs["env"]
    assert env["PEPPYLIB_REBUILD"] == "1"
    assert "--remap-path-prefix" in env["RUSTFLAGS"]
    with tarfile.open(tmp_path / "out" / BINDINGS_ARCHIVE) as tar:
        assert sorted(tar.getnames()) == sorted(
            f"so/{name}" for name in (*RELEASE_PLATFORM_SO, SO_BUILD_STATE_MARKER)
        )


@patch("functions.parallel_release.is_linux", return_value=False)
def test_apptainer_refuses_to_build_off_linux(mock_linux: MagicMock, tmp_path: Path) -> None:
    with pytest.raises(ReleaseError, match="Linux only"):
        run_apptainer(tmp_path / "out")


@patch("functions.parallel_release.need_cmd")
@patch("functions.parallel_release.subprocess.run")
@patch("functions.parallel_release.get_repo_root")
@patch("functions.parallel_release.get_native_triple", return_value="x86_64-unknown-linux-gnu")
@patch("functions.parallel_release.is_linux", return_value=True)
def test_apptainer_packs_the_cache_the_build_left(
    mock_linux: MagicMock,
    mock_triple: MagicMock,
    mock_repo_root: MagicMock,
    mock_run: MagicMock,
    mock_need_cmd: MagicMock,
    tmp_path: Path,
) -> None:
    mock_repo_root.return_value = tmp_path
    mock_run.return_value = subprocess.CompletedProcess([], 0)
    _fake_apptainer_cache(_cache_root(tmp_path), "x86_64")

    run_apptainer(tmp_path / "out")

    assert mock_run.call_args.args[0][:4] == ["cargo", "build", "-p", "containers"]
    with tarfile.open(tmp_path / "out" / "apptainer-x86_64.tgz") as tar:
        binary = tar.getmember("apptainer-1.5.2-x86_64-nosuid/bin/apptainer")
    # The executable bit is why the cache travels as a tarball.
    assert binary.mode & 0o111


@patch("functions.parallel_release.need_cmd")
@patch("functions.parallel_release.subprocess.run")
@patch("functions.parallel_release.get_repo_root")
@patch("functions.parallel_release.get_native_triple", return_value="x86_64-unknown-linux-gnu")
@patch("functions.parallel_release.is_linux", return_value=True)
def test_apptainer_fails_when_the_build_left_no_cache(
    mock_linux: MagicMock,
    mock_triple: MagicMock,
    mock_repo_root: MagicMock,
    mock_run: MagicMock,
    mock_need_cmd: MagicMock,
    tmp_path: Path,
) -> None:
    mock_repo_root.return_value = tmp_path
    mock_run.return_value = subprocess.CompletedProcess([], 0)

    with pytest.raises(ReleaseError, match="found none"):
        run_apptainer(tmp_path / "out")


# --- build ---


@pytest.fixture
def build_mocks(tmp_path: Path):
    plan_path = tmp_path / "release-plan.json"
    PLAN.write(plan_path)
    with patch("functions.parallel_release.need_cmd"), patch(
        "functions.parallel_release.get_repo_root", return_value=tmp_path
    ), patch(
        "functions.parallel_release.get_commit", return_value=RELEASE_COMMIT
    ) as get_commit, patch(
        "functions.parallel_release.get_native_triple",
        return_value="aarch64-unknown-linux-gnu",
    ) as native, patch(
        "functions.parallel_release.build_and_package"
    ) as build, patch(
        "functions.parallel_release.verify_all_releases"
    ) as verify:
        yield MagicMock(
            plan_path=plan_path,
            get_commit=get_commit,
            native=native,
            build=build,
            verify=verify,
        )


def test_build_embeds_the_provisioned_bindings_and_apptainer(
    tmp_path: Path, build_mocks: MagicMock
) -> None:
    provisioned = _provisioned(tmp_path, ("aarch64",))
    embedded: dict[str, list[str]] = {}

    def fake_build(tag: str, target: str, repo_root: Path) -> BuildArtifact:
        import os

        so_dir = Path(os.environ["PEPPYLIB_PREBUILT_SO_DIR"])
        embedded["so"] = sorted(p.name for p in so_dir.iterdir())
        asset = tmp_path / "dist" / f"peppy-{target}.tgz"
        return BuildArtifact(asset.name, asset, target)

    build_mocks.build.side_effect = fake_build

    run_build(
        plan_path=build_mocks.plan_path,
        target="aarch64-unknown-linux-gnu",
        provisioned_dir=provisioned,
    )

    build_mocks.build.assert_called_once_with(
        "v0.3.0", "aarch64-unknown-linux-gnu", tmp_path
    )
    assert embedded["so"] == sorted((*RELEASE_PLATFORM_SO, SO_BUILD_STATE_MARKER))
    seeded = _cache_root(tmp_path) / "apptainer-1.5.2-aarch64-nosuid" / "bin" / "apptainer"
    assert seeded.is_file()
    assert seeded.stat().st_mode & 0o111
    build_mocks.verify.assert_called_once_with(
        tmp_path / "dist", ["aarch64-unknown-linux-gnu"]
    )


def test_build_refuses_a_target_this_host_does_not_build(
    tmp_path: Path, build_mocks: MagicMock
) -> None:
    with pytest.raises(ReleaseError, match="built natively"):
        run_build(
            plan_path=build_mocks.plan_path,
            target="x86_64-unknown-linux-gnu",
            provisioned_dir=tmp_path,
        )

    build_mocks.build.assert_not_called()


def test_build_refuses_a_checkout_off_the_planned_commit(
    tmp_path: Path, build_mocks: MagicMock
) -> None:
    build_mocks.get_commit.return_value = OTHER_COMMIT

    with pytest.raises(ReleaseError, match="release plan builds"):
        run_build(
            plan_path=build_mocks.plan_path,
            target="aarch64-unknown-linux-gnu",
            provisioned_dir=_provisioned(tmp_path, ("aarch64",)),
        )

    build_mocks.build.assert_not_called()


def test_macos_build_stops_before_building_without_the_x86_64_apptainer(
    tmp_path: Path, build_mocks: MagicMock
) -> None:
    # Without it the macOS build would start a Lima VM to build it.
    build_mocks.native.return_value = "aarch64-apple-darwin"

    with pytest.raises(ReleaseError, match="apptainer-x86_64.tgz"):
        run_build(
            plan_path=build_mocks.plan_path,
            target="aarch64-apple-darwin",
            provisioned_dir=_provisioned(tmp_path, ("aarch64",)),
        )

    build_mocks.build.assert_not_called()


def test_build_refuses_an_apptainer_archive_for_another_architecture(
    tmp_path: Path, build_mocks: MagicMock
) -> None:
    provisioned = _provisioned(tmp_path, ("x86_64",))
    (provisioned / "apptainer-x86_64.tgz").rename(provisioned / "apptainer-aarch64.tgz")

    with pytest.raises(ReleaseError, match="not a cache for aarch64"):
        run_build(
            plan_path=build_mocks.plan_path,
            target="aarch64-unknown-linux-gnu",
            provisioned_dir=provisioned,
        )

    build_mocks.build.assert_not_called()


# --- publish ---


def _built_archives(directory: Path) -> Path:
    directory.mkdir(parents=True)
    for triple in (
        "aarch64-apple-darwin",
        "x86_64-unknown-linux-gnu",
        "aarch64-unknown-linux-gnu",
    ):
        (directory / f"peppy-{triple}.tgz").write_bytes(f"archive {triple}".encode())
    return directory


@pytest.fixture
def publish_mocks(tmp_path: Path, monkeypatch: pytest.MonkeyPatch):
    monkeypatch.setenv("PEPPY_RELEASE_TOKEN", "token")
    plan_path = tmp_path / "release-plan.json"
    PLAN.write(plan_path)
    with patch("functions.parallel_release.need_cmd"), patch(
        "functions.parallel_release.get_repo_root", return_value=tmp_path
    ), patch(
        "functions.parallel_release._verify_release_branch_state",
        return_value=RELEASE_COMMIT,
    ) as branch_state, patch(
        "functions.parallel_release.verify_all_releases"
    ) as verify, patch(
        "functions.parallel_release.github_repo_slug", return_value=SLUG
    ), patch(
        "functions.parallel_release.build_github_client"
    ) as client, patch(
        "functions.parallel_release._publish_pending_upload"
    ) as publish:
        yield MagicMock(
            plan_path=plan_path,
            branch_state=branch_state,
            verify=verify,
            client=client,
            publish=publish,
        )


def test_publish_records_the_archives_and_publishes_them(
    tmp_path: Path, publish_mocks: MagicMock
) -> None:
    archives = _built_archives(tmp_path / "archives")

    run_publish(plan_path=publish_mocks.plan_path, archives_dir=archives)

    dist = tmp_path / "dist"
    publish_mocks.verify.assert_called_once_with(dist)
    args = publish_mocks.publish.call_args.args
    assert args[0] is publish_mocks.client.return_value
    assert args[1] == SLUG
    pending = args[2]
    assert (pending.tag, pending.release_commit, pending.content) == (
        "v0.3.0",
        RELEASE_COMMIT,
        CONTENT,
    )
    assert sorted(a.asset_path for a in pending.artifacts) == sorted(dist.glob("peppy-*.tgz"))
    # The manifest is on disk before the first request, so a failed publish can
    # be resumed by build_release.sh.
    assert args[3] == dist / "pending-upload.json"
    assert args[3].is_file()


def test_publish_stops_when_dev_moved_during_the_build(
    tmp_path: Path, publish_mocks: MagicMock
) -> None:
    publish_mocks.branch_state.return_value = OTHER_COMMIT

    with pytest.raises(ReleaseError, match="moved to"):
        run_publish(plan_path=publish_mocks.plan_path, archives_dir=_built_archives(tmp_path / "a"))

    publish_mocks.publish.assert_not_called()


def test_publish_stops_when_an_archive_is_missing(
    tmp_path: Path, publish_mocks: MagicMock
) -> None:
    archives = _built_archives(tmp_path / "archives")
    (archives / "peppy-x86_64-unknown-linux-gnu.tgz").unlink()

    with pytest.raises(ReleaseError, match="x86_64-unknown-linux-gnu archive is missing"):
        run_publish(plan_path=publish_mocks.plan_path, archives_dir=archives)

    publish_mocks.publish.assert_not_called()


def test_publish_requires_the_release_token(
    tmp_path: Path, publish_mocks: MagicMock, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.setenv("PEPPY_RELEASE_TOKEN", " ")

    with pytest.raises(ReleaseError, match="PEPPY_RELEASE_TOKEN env var is required"):
        run_publish(plan_path=publish_mocks.plan_path, archives_dir=tmp_path)

    publish_mocks.publish.assert_not_called()
