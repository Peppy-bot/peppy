"""Tests for functions.parallel_release."""

from __future__ import annotations

import hashlib
import json
import os
import re
import subprocess
import tarfile
from collections.abc import Callable
from dataclasses import dataclass
from pathlib import Path
from unittest.mock import MagicMock, patch

import httpx
import pytest
import respx

from functions.build import BuildArtifact
from functions.cli import RELEASE_TRIPLES, ReleaseError
from functions.github import ReleaseInfo, RepoSlug
from functions.lima import (
    GO_LINUX_ARM64_SHA256,
    GO_VERSION,
    RELEASE_PLATFORM_SO,
    SO_BUILD_STATE_MARKER,
)
from functions.parallel_release import (
    BINDINGS_ARCHIVE,
    ReleasePlan,
    _apptainer_arches_for,
    _find_published_release,
    _find_release_notes_commit,
    _install_pinned_go,
    _main_already_at,
    _parse_args,
    _require_unused_tag,
    _upload_archives_and_publish,
    _write_tarball,
    apptainer_archive_name,
    run_apptainer,
    run_bindings,
    run_build,
    run_hub_check,
    run_prepare,
    run_publish,
)
from functions.release_summary import ReleaseContent
from functions.release_line import update_release_branch as real_update_release_branch
from functions.release_line import (
    verify_publish_branch_state as real_verify_publish_branch_state,
)
from functions.repo import commit_paths as real_commit_paths
from functions.repo import merge_into_current_branch as real_merge
from functions.repo import push_branch as real_push_branch

from .helpers import HUB_COMMITS, hub_set_document, unwrapped

RELEASE_COMMIT = "1111111111111111111111111111111111111111"
OTHER_COMMIT = "2222222222222222222222222222222222222222"
CONTENT = ReleaseContent(
    title="Topics API hardening",
    description="Hardened the topics API against deadlocks.",
    notes="## What's Changed\n- Fixed topics public API\n",
)
PLAN = ReleasePlan(tag="v0.3.0", release_commit=RELEASE_COMMIT, content=CONTENT)
SLUG = RepoSlug(owner="test-owner", repo="test-repo")
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


def test_release_plan_rejects_a_tag_no_release_publishes(tmp_path: Path) -> None:
    path = tmp_path / "release-plan.json"
    ReleasePlan(tag="v0.3.0-rc1", release_commit=RELEASE_COMMIT, content=CONTENT).write(
        path
    )

    with pytest.raises(ReleaseError, match="is not a peppy release version"):
        ReleasePlan.load(path)


# --- command line ---


def test_prepare_refuses_a_tag_no_release_publishes(
    capsys: pytest.CaptureFixture[str],
) -> None:
    # The grammar of a release tag is resolve.py's, whose tests hold every form.
    with pytest.raises(SystemExit) as exc_info:
        _parse_args(
            [
                "prepare",
                "--tag",
                "v0.31.2-rc1",
                "--release-commit",
                RELEASE_COMMIT,
                "--no-minor-docs-pr",
                "--plan",
                "p",
            ]
        )

    assert exc_info.value.code == 2
    assert "reads the hubs' main" in unwrapped(capsys.readouterr().err)


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


def test_hub_check_takes_the_hub_set_and_the_archive() -> None:
    args = _parse_args(
        ["hub-check", "--hub-set", "hub-set.json", "--archive", "peppy.tgz"]
    )

    assert args.hub_set == Path("hub-set.json")
    assert args.archive == Path("peppy.tgz")


def test_hub_launch_takes_the_hub_set_and_the_run_id() -> None:
    args = _parse_args(
        ["hub-launch", "--hub-set", "hub-set.json", "--peppy-run-id", "18000000001"]
    )

    assert args.hub_set == Path("hub-set.json")
    assert args.peppy_run_id == 18000000001


@pytest.mark.parametrize("run_id", ["abc", "0", "-1", "12a", ""])
def test_hub_launch_refuses_a_run_id_that_is_no_run(run_id: str) -> None:
    with pytest.raises(SystemExit) as exc_info:
        _parse_args(["hub-launch", "--hub-set", "s", "--peppy-run-id", run_id])
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
        "functions.parallel_release.verify_release_branch_state",
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
        "functions.release_line.get_latest_release",
        return_value={"tag_name": "v0.2.0"},
    ), patch(
        "functions.release_line.fetch_tag"
    ) as fetch_tag, patch(
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
            fetch_tag=fetch_tag,
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
    # The notes cover the changes since the last published release, whose tag
    # is fetched first: it can be newer than the checkout.
    prepare_mocks.fetch_tag.assert_called_once_with("origin", "v0.2.0")
    prepare_mocks.collect.assert_called_once_with("v0.2.0", RELEASE_COMMIT, tmp_path)
    # Nobody reviews the draft, so the log shows it.
    output = unwrapped(capfd.readouterr().err)
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
    _fake_bindings_dir(_cache_root(tmp_path) / "peppylib-py" / "so" / "release")

    run_bindings(tmp_path / "out")

    # Checked up front: peppylib-py's build script refuses to start without it.
    mock_need_cmd.assert_any_call("uv")
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


def _fake_go_download(tmp_path: Path, digest: str):
    """Stand in for the Go download: a tarball with one top-level go/ directory,
    reported with *digest* as its SHA-256."""

    def download(url: str, destination: Path) -> str:
        toolchain = tmp_path / "go-release" / "go"
        (toolchain / "bin").mkdir(parents=True, exist_ok=True)
        (toolchain / "bin" / "go").write_text("go toolchain")
        _write_tarball(destination, [(toolchain, "go")])
        return digest

    return download


def test_install_pinned_go_verifies_and_installs_the_toolchain(tmp_path: Path) -> None:
    with patch(
        "functions.parallel_release._download",
        side_effect=_fake_go_download(tmp_path, GO_LINUX_ARM64_SHA256),
    ) as download:
        go_bin = _install_pinned_go("aarch64")

    assert download.call_args.args[0] == (
        f"https://go.dev/dl/go{GO_VERSION}.linux-arm64.tar.gz"
    )
    assert go_bin == _cache_root(tmp_path) / f"go-{GO_VERSION}-arm64" / "bin"
    assert (go_bin / "go").read_text() == "go toolchain"


def test_install_pinned_go_rejects_a_download_off_its_checksum(tmp_path: Path) -> None:
    with patch(
        "functions.parallel_release._download",
        side_effect=_fake_go_download(tmp_path, "0" * 64),
    ):
        with pytest.raises(ReleaseError, match="pinned SHA-256"):
            _install_pinned_go("x86_64")

    assert not (_cache_root(tmp_path) / f"go-{GO_VERSION}-amd64").exists()


def test_install_pinned_go_reuses_the_installed_toolchain(tmp_path: Path) -> None:
    go_bin = _cache_root(tmp_path) / f"go-{GO_VERSION}-amd64" / "bin"
    go_bin.mkdir(parents=True)
    (go_bin / "go").write_text("installed")

    with patch("functions.parallel_release._download") as download:
        assert _install_pinned_go("x86_64") == go_bin

    download.assert_not_called()


@patch("functions.parallel_release.is_linux", return_value=False)
def test_apptainer_refuses_to_build_off_linux(mock_linux: MagicMock, tmp_path: Path) -> None:
    with pytest.raises(ReleaseError, match="Linux only"):
        run_apptainer(tmp_path / "out")


@patch("functions.parallel_release._install_pinned_go", return_value=Path("/pinned/go/bin"))
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
    mock_go: MagicMock,
    tmp_path: Path,
) -> None:
    mock_repo_root.return_value = tmp_path
    mock_run.return_value = subprocess.CompletedProcess([], 0)
    _fake_apptainer_cache(_cache_root(tmp_path), "x86_64")

    run_apptainer(tmp_path / "out")

    assert mock_run.call_args.args[0][:4] == ["cargo", "build", "-p", "containers"]
    # apptainer compiles with the pinned Go, never one the build downloads.
    mock_go.assert_called_once_with("x86_64")
    env = mock_run.call_args.kwargs["env"]
    assert env["PATH"].startswith(f"/pinned/go/bin{os.pathsep}")
    assert env["GOTOOLCHAIN"] == "local"
    with tarfile.open(tmp_path / "out" / "apptainer-x86_64.tgz") as tar:
        binary = tar.getmember("apptainer-1.5.2-x86_64-nosuid/bin/apptainer")
    # The executable bit is why the cache travels as a tarball.
    assert binary.mode & 0o111


@patch("functions.parallel_release._install_pinned_go", return_value=Path("/pinned/go/bin"))
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
    mock_go: MagicMock,
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

API_PATH = f"/repos/{SLUG.full}"
NOTES_FILE = "docs/src/content/releases/v0.3.0.html"
HUB_TAG = "peppy-release/v0.3.0"


def _built_archives(directory: Path) -> Path:
    """The archive of every release target, as the build jobs upload them."""
    directory.mkdir(parents=True)
    for triple in RELEASE_TRIPLES:
        (directory / f"peppy-{triple}.tgz").write_bytes(f"archive {triple}".encode())
    return directory


def _git(repo: Path, *args: str) -> str:
    result = subprocess.run(
        ["git", *args], cwd=repo, check=True, capture_output=True, text=True
    )
    return result.stdout.strip()


@dataclass(frozen=True)
class ReleaseRepo:
    """A checkout of `dev` and the `origin` it pushes to, as the publish job
    gets them: `main` at the last shipped commit, `dev` one release commit
    past it, both pushed."""

    path: Path
    origin: Path
    shipped_commit: str
    release_commit: str

    def remote_commit(self, ref: str) -> str:
        return _git(self.origin, "rev-parse", ref)

    def commit_on_dev(self, files: dict[str, str], message: str) -> str:
        """Commit *files* on `dev` and push it; return the commit."""
        for relative, text in files.items():
            path = self.path / relative
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(text)
        _git(self.path, "add", "--", *files)
        _git(self.path, "commit", "-q", "-m", message)
        _git(self.path, "push", "-q", "origin", "dev")
        return _git(self.path, "rev-parse", "HEAD")

    def commit_release_notes(self, tag: str = "v0.3.0") -> str:
        """The notes commit a publish pushes to `dev`."""
        return self.commit_on_dev(
            {f"docs/src/content/releases/{tag}.html": f"<entry>{tag}</entry>\n"},
            f"docs: add release notes for {tag}",
        )

    def merge_elsewhere(self, files: dict[str, str], message: str) -> str:
        """Commit *files* on `origin/dev` from another clone, as a pull request
        merged while the release runs does, and leave this checkout behind;
        return the commit."""
        other = self.path.parent / "elsewhere"
        if not other.exists():
            _git(self.path.parent, "clone", "-q", str(self.origin), str(other))
        _git(other, "fetch", "-q", "origin")
        _git(other, "switch", "-q", "-C", "dev", "origin/dev")
        for relative, text in files.items():
            path = other / relative
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(text)
        _git(other, "add", "--", *files)
        _git(other, "commit", "-q", "-m", message)
        _git(other, "push", "-q", "origin", "dev")
        return _git(other, "rev-parse", "HEAD")

    def rewrite_dev(self) -> str:
        """Force-push a `dev` that drops the release commit; return its tip."""
        _git(self.path, "reset", "-q", "--hard", self.shipped_commit)
        (self.path / "code.rs").write_text("fn rewritten() {}\n")
        _git(self.path, "commit", "-q", "-am", "rewritten")
        _git(self.path, "push", "-q", "--force", "origin", "dev")
        return _git(self.path, "rev-parse", "HEAD")

    def move_main_off_the_release(self) -> str:
        """Push to `main` a commit the release commit does not hold; return it."""
        _git(self.path, "switch", "-q", "-c", "hotfix", self.shipped_commit)
        (self.path / "hotfix.rs").write_text("fn hotfix() {}\n")
        _git(self.path, "add", "hotfix.rs")
        _git(self.path, "commit", "-q", "-m", "hotfix")
        _git(self.path, "push", "-q", "origin", "hotfix:main")
        _git(self.path, "switch", "-q", "dev")
        return _git(self.path, "rev-parse", "hotfix")

    def tag_on_origin(self, commit: str) -> None:
        """Create the release tag on `origin`, as publishing the release does."""
        _git(self.origin, "tag", PLAN.tag, commit)

    def align_main(self) -> None:
        _git(self.path, "push", "-q", "origin", "dev:main")

    def worktrees(self) -> list[str]:
        """The worktrees of the checkout, its own included."""
        listing = _git(self.path, "worktree", "list", "--porcelain")
        return [
            line.removeprefix("worktree ")
            for line in listing.splitlines()
            if line.startswith("worktree ")
        ]


@pytest.fixture
def release_repo(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> ReleaseRepo:
    # Git reads no configuration of the host, so the commits the publish makes
    # are never signed and always have the same author, whoever runs the suite.
    gitconfig = tmp_path / "gitconfig"
    gitconfig.write_text(
        "[user]\n\tname = Release Test\n\temail = release@example.com\n"
        "[commit]\n\tgpgsign = false\n"
        "[tag]\n\tgpgsign = false\n"
        "[init]\n\tdefaultBranch = main\n"
    )
    monkeypatch.setenv("GIT_CONFIG_GLOBAL", str(gitconfig))
    monkeypatch.setenv("GIT_CONFIG_NOSYSTEM", "1")

    origin = tmp_path / "origin.git"
    _git(tmp_path, "init", "-q", "--bare", str(origin))
    path = tmp_path / "repo"
    path.mkdir()
    _git(path, "init", "-q")
    _git(path, "remote", "add", "origin", str(origin))
    (path / "code.rs").write_text("fn shipped() {}\n")
    _git(path, "add", "code.rs")
    _git(path, "commit", "-q", "-m", "shipped")
    _git(path, "push", "-q", "origin", "main")
    _git(path, "switch", "-q", "-c", "dev")
    (path / "code.rs").write_text("fn shipped() {}\nfn released() {}\n")
    _git(path, "commit", "-q", "-am", "released")
    _git(path, "push", "-q", "origin", "dev")
    monkeypatch.chdir(path)
    return ReleaseRepo(
        path=path.resolve(),
        origin=origin,
        shipped_commit=_git(path, "rev-parse", "main"),
        release_commit=_git(path, "rev-parse", "dev"),
    )


def _sha(text: str) -> str:
    return hashlib.sha1(text.encode()).hexdigest()


class FakeGitHub:
    """The releases API of test-owner/test-repo and the git API of the hubs of
    test-owner, as the publish stage uses them.

    The router mocks all of httpx, and any request this fake does not answer
    fails the test. Every request it answers is recorded in `events`, which
    tests share with the git steps they record, to check the order of both.
    """

    def __init__(self, router: respx.MockRouter, events: list[str]) -> None:
        self.requests: list[str] = []
        self.releases: dict[int, dict] = {}
        self.uploads: list[str] = []
        self.failing_upload: str | None = None
        # The tag refs of each hub, by name, at the object each names, and the
        # annotated tag objects, by sha.
        self.hub_tags: dict[str, dict[str, dict]] = {}
        self.tag_objects: dict[str, dict] = {}
        self.events = events
        self._next_id = 7
        router.route(host="api.github.com").mock(side_effect=self._api)
        router.route(host="uploads.github.com").mock(side_effect=self._upload)

    def add_hub_tag(self, hub: str, tag: str, commit: str, *, annotated: bool) -> None:
        """A tag of *hub* that exists before the publish runs."""
        if annotated:
            sha = _sha(f"{hub} {tag} {commit}")
            self.tag_objects[sha] = {
                "sha": sha,
                "tag": tag,
                "message": "an earlier tag",
                "object": {"type": "commit", "sha": commit},
            }
            target = {"type": "tag", "sha": sha}
        else:
            target = {"type": "commit", "sha": commit}
        self.hub_tags.setdefault(hub, {})[tag] = target

    def hub_tag_commit(self, hub: str, tag: str) -> str | None:
        target = self.hub_tags.get(hub, {}).get(tag)
        if target is None:
            return None
        if target["type"] == "tag":
            return self.tag_objects[target["sha"]]["object"]["sha"]
        return target["sha"]

    def _hub_git(self, request: httpx.Request, hub: str, path: str) -> httpx.Response:
        method = request.method
        tags = self.hub_tags.setdefault(hub, {})
        if method == "GET" and (name := re.fullmatch(r"ref/tags/(.+)", path)):
            self.events.append(f"read the tag of {hub}")
            target = tags.get(name[1])
            if target is None:
                return httpx.Response(404, json={"message": "Not Found"})
            return httpx.Response(200, json={"ref": f"refs/tags/{name[1]}", "object": target})
        if method == "GET" and (sha := re.fullmatch(r"tags/([0-9a-f]{40})", path)):
            return httpx.Response(200, json=self.tag_objects[sha[1]])
        if method == "POST" and path == "tags":
            payload = json.loads(request.content)
            assert payload["type"] == "commit"
            tag_sha = _sha(f"{hub} {payload['tag']} {payload['object']}")
            self.tag_objects[tag_sha] = {
                "sha": tag_sha,
                "tag": payload["tag"],
                "message": payload["message"],
                "object": {"type": "commit", "sha": payload["object"]},
            }
            return httpx.Response(201, json=self.tag_objects[tag_sha])
        if method == "POST" and path == "refs":
            payload = json.loads(request.content)
            name = payload["ref"].removeprefix("refs/tags/")
            if name in tags:
                return httpx.Response(422, json={"message": "Reference already exists"})
            self.events.append(f"tag {hub}")
            tags[name] = {"type": "tag", "sha": payload["sha"]}
            return httpx.Response(201, json={"ref": payload["ref"], "object": tags[name]})
        raise AssertionError(f"unexpected GitHub request: {method} {request.url}")

    def add_release(self, tag: str, *, draft: bool) -> dict:
        release_id = self._next_id
        self._next_id += 1
        release = {
            "id": release_id,
            "tag_name": tag,
            "name": "A release",
            "body": "- a change",
            "body_html": "<ul><li>a change</li></ul>",
            "created_at": "2026-09-25T09:00:00Z",
            "draft": True,
            "published_at": None,
            "html_url": (
                f"https://github.com/{SLUG.full}/releases/tag/untagged-{release_id}"
            ),
        }
        self.releases[release_id] = release
        if not draft:
            self._go_live(release)
        return release

    def published(self) -> list[dict]:
        return [r for r in self.releases.values() if not r["draft"]]

    def _go_live(self, release: dict) -> None:
        release["draft"] = False
        release["published_at"] = "2026-09-25T10:00:00Z"
        release["html_url"] = (
            f"https://github.com/{SLUG.full}/releases/tag/{release['tag_name']}"
        )

    def _api(self, request: httpx.Request) -> httpx.Response:
        method = request.method
        hub_git = re.fullmatch(
            rf"/repos/{SLUG.owner}/([^/]+)/git/(.+)", request.url.path
        )
        if hub_git and hub_git[1] != SLUG.repo:
            self.requests.append(f"{method} {request.url.path}")
            return self._hub_git(request, hub_git[1], hub_git[2])
        path = request.url.path.removeprefix(API_PATH)
        self.requests.append(f"{method} {path}")
        if method == "GET" and (tag := re.fullmatch(r"/releases/tags/(.+)", path)):
            self.events.append("look up the published release")
            matching = [r for r in self.published() if r["tag_name"] == tag[1]]
            if not matching:
                return httpx.Response(404, json={"message": "Not Found"})
            return httpx.Response(200, json=matching[0])
        if method == "GET" and path == "/releases":
            return httpx.Response(200, json=list(self.releases.values()))
        if method == "POST" and path == "/releases":
            payload = json.loads(request.content)
            self.events.append("create the draft")
            assert payload["draft"] is True
            release = self.add_release(payload["tag_name"], draft=True)
            release.update(
                name=payload["name"],
                body=payload["body"],
                target_commitish=payload["target_commitish"],
            )
            return httpx.Response(201, json=release)
        if method == "GET" and re.fullmatch(r"/releases/\d+/assets", path):
            return httpx.Response(200, json=[])
        match = re.fullmatch(r"/releases/(\d+)", path)
        release = self.releases.get(int(match[1])) if match else None
        if release is not None and method == "GET":
            return httpx.Response(200, json=release)
        if release is not None and method == "DELETE":
            self.events.append(f"delete draft {release['id']}")
            del self.releases[release["id"]]
            return httpx.Response(204)
        if release is not None and method == "PATCH":
            assert json.loads(request.content) == {"draft": False}
            self.events.append("publish")
            self._go_live(release)
            return httpx.Response(200, json=release)
        raise AssertionError(f"unexpected GitHub request: {method} {request.url}")

    def _upload(self, request: httpx.Request) -> httpx.Response:
        name = request.url.params["name"]
        self.requests.append(f"UPLOAD {name}")
        request.read()
        if name == self.failing_upload:
            return httpx.Response(422, json={"message": "Validation Failed"})
        self.events.append(f"upload {name}")
        self.uploads.append(name)
        return httpx.Response(201, json={"id": len(self.uploads), "name": name})


@pytest.fixture
def events() -> list[str]:
    return []


@pytest.fixture
def github(mock_api: respx.MockRouter, events: list[str]) -> FakeGitHub:
    return FakeGitHub(mock_api, events)


def _plan_of(repo: ReleaseRepo) -> ReleasePlan:
    return ReleasePlan(
        tag=PLAN.tag, release_commit=repo.release_commit, content=CONTENT
    )


class InstallCheck:
    """Stands in for the check that the installed archive reads the default
    hubs: it records the archive it was given, and fails when told to."""

    def __init__(self, events: list[str]) -> None:
        self.events = events
        self.archives: list[Path] = []
        self.failure: ReleaseError | None = None
        # What happens elsewhere while the install is checked, in the middle
        # of the publish.
        self.meanwhile: Callable[[], object] | None = None

    def __call__(self, archive: Path) -> None:
        self.events.append("check the install")
        self.archives.append(archive)
        if self.meanwhile is not None:
            self.meanwhile()
        if self.failure is not None:
            raise self.failure


@pytest.fixture
def install_check(events: list[str]) -> InstallCheck:
    return InstallCheck(events)


def _hub_set_text() -> str:
    return json.dumps(hub_set_document())


@pytest.fixture
def publish(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    release_repo: ReleaseRepo,
    github: FakeGitHub,
    events: list[str],
    install_check: InstallCheck,
):
    """Run the publish stage of the release of `release_repo`'s release commit,
    against the fake GitHub, on an x86_64 Linux host.

    The archives are the three a build would leave; their contents are not
    real archives, so verifying them and checking the install are recorded
    instead of done.
    """
    monkeypatch.setenv("PEPPY_RELEASE_TOKEN", "token")
    monkeypatch.setenv("GITHUB_REPOSITORY", SLUG.full)
    plan_path = tmp_path / "release-plan.json"
    _plan_of(release_repo).write(plan_path)
    archives = _built_archives(tmp_path / "archives")
    hub_set_path = tmp_path / "hub-set.json"
    hub_set_path.write_text(_hub_set_text())

    def run() -> None:
        with patch(
            "functions.parallel_release.verify_all_releases",
            side_effect=lambda dist_dir: events.append("verify the archives"),
        ), patch(
            "functions.parallel_release.check_release_install",
            side_effect=install_check,
        ), patch(
            "functions.parallel_release.get_native_triple",
            return_value="x86_64-unknown-linux-gnu",
        ):
            run_publish(
                plan_path=plan_path, archives_dir=archives, hub_set_path=hub_set_path
            )

    return run


def _uploaded_names() -> list[str]:
    return [f"peppy-{triple}.tgz" for triple in RELEASE_TRIPLES]


def _parents(repo: ReleaseRepo, commit: str) -> list[str]:
    return _git(repo.origin, "rev-list", "--parents", "-n", "1", commit).split()[1:]


def test_publish_releases_then_commits_the_notes_and_aligns_main(
    publish,
    release_repo: ReleaseRepo,
    github: FakeGitHub,
    capfd: pytest.CaptureFixture[str],
) -> None:
    publish()

    [release] = github.published()
    # The draft carried the drafted content, tagged at the commit the
    # archives were built from rather than at a branch name.
    assert release["tag_name"] == "v0.3.0"
    assert release["name"] == CONTENT.title
    assert release["body"] == CONTENT.notes
    assert release["target_commitish"] == release_repo.release_commit
    assert github.uploads == _uploaded_names()

    # `dev` fast-forwarded to the notes commit, and `main` followed it.
    dev = release_repo.remote_commit("dev")
    assert _parents(release_repo, dev) == [release_repo.release_commit]
    assert _git(
        release_repo.origin, "diff", "--name-only", release_repo.release_commit, dev
    ) == NOTES_FILE
    assert _git(release_repo.origin, "log", "-1", "--format=%s", dev) == (
        "docs: add release notes for v0.3.0"
    )
    assert release_repo.remote_commit("main") == dev
    notes = _git(release_repo.origin, "show", f"dev:{NOTES_FILE}")
    assert CONTENT.description in notes
    assert "Released on September 25, 2026" in notes
    # The worktree the notes were committed in is gone.
    assert release_repo.worktrees() == [str(release_repo.path)]

    # The run ends on the outcome and the published release, never the
    # draft's untagged placeholder.
    err = capfd.readouterr().err
    assert "untagged" not in err
    assert err.strip().splitlines()[-1] == (
        "Released v0.3.0. Release notes: "
        "https://github.com/test-owner/test-repo/releases/tag/v0.3.0"
    )


def test_publish_does_its_steps_in_order(
    publish, github: FakeGitHub, events: list[str]
) -> None:
    def recorded_push(remote: str, source: str, remote_branch: str) -> None:
        events.append(f"push {remote_branch}")
        real_push_branch(remote, source, remote_branch)

    def recorded_commit(paths: list[Path], message: str, *, cwd: Path) -> str:
        events.append("commit the notes")
        return real_commit_paths(paths, message, cwd=cwd)

    def recorded_branch_check(release_commit: str) -> str:
        events.append("check the branches")
        return real_verify_publish_branch_state(release_commit)

    def recorded_notes_lookup(*args: object) -> str | None:
        events.append("look up the notes commit")
        return _find_release_notes_commit(*args)

    def recorded_update() -> str:
        events.append("update dev")
        return real_update_release_branch()

    def recorded_merge(commit: str, message: str) -> None:
        events.append("merge the notes")
        real_merge(commit, message)

    with patch(
        "functions.parallel_release.push_branch", side_effect=recorded_push
    ), patch(
        "functions.parallel_release.commit_paths", side_effect=recorded_commit
    ), patch(
        "functions.parallel_release.verify_publish_branch_state",
        side_effect=recorded_branch_check,
    ), patch(
        "functions.parallel_release._find_release_notes_commit",
        side_effect=recorded_notes_lookup,
    ), patch(
        "functions.parallel_release.update_release_branch",
        side_effect=recorded_update,
    ), patch(
        "functions.parallel_release.merge_into_current_branch",
        side_effect=recorded_merge,
    ):
        publish()

    # Nothing is written before every check passed: the branches first, so a
    # run that stops there tags no hub. Every hub is read before any is
    # tagged, the install is checked once the hubs carry their tag and before
    # the draft exists, and the git side waits for the release to be live:
    # the notes are read from the published release. `dev` is read again
    # right before the notes merge into it.
    assert events == [
        "check the branches",
        "look up the notes commit",
        "look up the published release",
        "verify the archives",
        *(f"read the tag of {hub}" for hub in HUB_COMMITS),
        *(f"tag {hub}" for hub in HUB_COMMITS),
        "check the install",
        "create the draft",
        *(f"upload {name}" for name in _uploaded_names()),
        "publish",
        "commit the notes",
        "update dev",
        "merge the notes",
        "push dev",
        "push main",
    ]


def test_publish_rerun_does_not_upload_a_release_already_published(
    publish, release_repo: ReleaseRepo, github: FakeGitHub, events: list[str]
) -> None:
    # The earlier attempt published the release, then failed before the notes.
    github.add_release("v0.3.0", draft=False)
    release_repo.tag_on_origin(release_repo.release_commit)

    publish()

    assert "create the draft" not in events
    assert "verify the archives" not in events
    # Its hubs were tagged and its install checked before it went live.
    assert not [request for request in github.requests if "/git/" in request]
    assert "check the install" not in events
    assert github.uploads == []
    assert len(github.releases) == 1
    # The steps left are done.
    dev = release_repo.remote_commit("dev")
    assert _git(release_repo.origin, "rev-parse", "dev^") == release_repo.release_commit
    assert release_repo.remote_commit("main") == dev


def test_publish_rerun_does_not_commit_notes_already_committed(
    publish,
    release_repo: ReleaseRepo,
    github: FakeGitHub,
    capfd: pytest.CaptureFixture[str],
) -> None:
    # The earlier attempt pushed the notes, then failed to fast-forward main.
    github.add_release("v0.3.0", draft=False)
    release_repo.tag_on_origin(release_repo.release_commit)
    notes_commit = release_repo.commit_release_notes()

    with patch(
        "functions.parallel_release.commit_paths",
        side_effect=AssertionError("committed"),
    ):
        publish()

    assert release_repo.remote_commit("dev") == notes_commit
    assert release_repo.remote_commit("main") == notes_commit
    assert github.uploads == []
    assert "already committed" in " ".join(capfd.readouterr().err.split())


def test_publish_rerun_does_not_push_main_already_aligned(
    publish,
    release_repo: ReleaseRepo,
    github: FakeGitHub,
    capfd: pytest.CaptureFixture[str],
) -> None:
    # The earlier attempt did everything and failed afterwards.
    github.add_release("v0.3.0", draft=False)
    release_repo.tag_on_origin(release_repo.release_commit)
    notes_commit = release_repo.commit_release_notes()
    release_repo.align_main()

    with patch(
        "functions.parallel_release.push_branch", side_effect=AssertionError("pushed")
    ), patch(
        "functions.parallel_release.commit_paths",
        side_effect=AssertionError("committed"),
    ):
        publish()

    assert release_repo.remote_commit("dev") == notes_commit
    assert release_repo.remote_commit("main") == notes_commit
    assert github.uploads == []
    assert "Released v0.3.0." in capfd.readouterr().err


MERGED_SINCE = {"code.rs": "fn shipped() {}\nfn released() {}\nfn merged_since() {}\n"}


def _assert_released_the_built_commit(
    repo: ReleaseRepo, github: FakeGitHub, merged: str
) -> None:
    """The release of *repo* is its release commit, and `dev` holds both its
    notes and *merged*, the tip `dev` had moved to: the notes commit sits on
    the release commit, merged into `dev`, and `main` is at the notes commit,
    without the merge."""
    [release] = github.published()
    assert release["target_commitish"] == repo.release_commit
    for hub, commit in HUB_COMMITS.items():
        assert github.hub_tag_commit(hub, HUB_TAG) == commit

    dev = repo.remote_commit("dev")
    merged_tip, notes_commit = _parents(repo, dev)
    assert merged_tip == merged
    assert _git(repo.origin, "log", "-1", "--format=%s", dev) == (
        "Merge the release notes of v0.3.0 into 'dev'"
    )
    assert _parents(repo, notes_commit) == [repo.release_commit]
    assert _git(
        repo.origin, "diff", "--name-only", repo.release_commit, notes_commit
    ) == NOTES_FILE
    assert repo.remote_commit("main") == notes_commit
    assert _git(repo.origin, "show", "main:code.rs") == _git(
        repo.origin, "show", f"{repo.release_commit}:code.rs"
    )
    assert _git(repo.origin, "show", "dev:code.rs") == MERGED_SINCE["code.rs"].strip()
    assert repo.worktrees() == [str(repo.path)]


@pytest.mark.parametrize(
    "merge",
    [
        # The publish job checked out the `dev` that moved.
        lambda repo: repo.commit_on_dev(MERGED_SINCE, "merged since the build"),
        # `dev` moved once the publish job had checked it out.
        lambda repo: repo.merge_elsewhere(MERGED_SINCE, "merged since the build"),
    ],
    ids=["before-the-checkout", "after-the-checkout"],
)
def test_publish_releases_the_built_commit_when_dev_moved_since_the_build(
    publish,
    release_repo: ReleaseRepo,
    github: FakeGitHub,
    merge: Callable[[ReleaseRepo], str],
) -> None:
    merged = merge(release_repo)

    publish()

    _assert_released_the_built_commit(release_repo, github, merged)


def test_publish_takes_in_a_merge_into_dev_while_it_runs(
    publish,
    release_repo: ReleaseRepo,
    github: FakeGitHub,
    install_check: InstallCheck,
) -> None:
    merged: list[str] = []
    install_check.meanwhile = lambda: merged.append(
        release_repo.merge_elsewhere(MERGED_SINCE, "merged during the publish")
    )

    publish()

    _assert_released_the_built_commit(release_repo, github, merged[0])


def test_publish_rerun_finds_the_notes_merged_into_dev_that_moved(
    publish, release_repo: ReleaseRepo, github: FakeGitHub
) -> None:
    release_repo.merge_elsewhere(MERGED_SINCE, "merged since the build")

    def rejected_main_push(remote: str, source: str, remote_branch: str) -> None:
        if remote_branch == "main":
            raise ReleaseError(f"failed to push '{source}' to 'origin/main': rejected")
        real_push_branch(remote, source, remote_branch)

    with patch(
        "functions.parallel_release.push_branch", side_effect=rejected_main_push
    ), pytest.raises(ReleaseError, match="Re-run the failed publish job"):
        publish()

    merged_notes = release_repo.remote_commit("dev")
    _, notes_commit = _parents(release_repo, merged_notes)
    # GitHub created the tag when the release was published, and `dev` took
    # one more merge before the re-run.
    release_repo.tag_on_origin(release_repo.release_commit)
    later = release_repo.merge_elsewhere({"later.rs": "fn later() {}\n"}, "later")

    with patch(
        "functions.parallel_release.commit_paths",
        side_effect=AssertionError("committed"),
    ):
        publish()

    assert len(github.published()) == 1
    assert release_repo.remote_commit("dev") == later
    assert release_repo.remote_commit("main") == notes_commit


def test_publish_refuses_a_rewritten_dev_before_any_request(
    publish, release_repo: ReleaseRepo, github: FakeGitHub
) -> None:
    rewritten = release_repo.rewrite_dev()

    with pytest.raises(ReleaseError, match="was rewritten since the release started"):
        publish()

    # No request at all, so no hub is tagged.
    assert github.requests == []
    assert release_repo.remote_commit("dev") == rewritten
    assert release_repo.remote_commit("main") == release_repo.shipped_commit


def test_publish_refuses_main_off_the_release_before_any_request(
    publish, release_repo: ReleaseRepo, github: FakeGitHub
) -> None:
    hotfix = release_repo.move_main_off_the_release()

    with pytest.raises(ReleaseError) as excinfo:
        publish()

    message = unwrapped(str(excinfo.value))
    assert f"origin/main is at {hotfix[:12]}" in message
    assert "'main' cannot fast-forward to the release of v0.3.0" in message
    assert github.requests == []
    assert release_repo.remote_commit("dev") == release_repo.release_commit


def test_publish_refuses_unpushed_dev_commits_before_any_request(
    publish, release_repo: ReleaseRepo, github: FakeGitHub
) -> None:
    (release_repo.path / "code.rs").write_text("fn unpushed() {}\n")
    _git(release_repo.path, "commit", "-q", "-am", "unpushed")

    with pytest.raises(ReleaseError, match="commits that are not on origin/dev"):
        publish()

    assert github.requests == []
    assert release_repo.remote_commit("dev") == release_repo.release_commit


def test_publish_tags_every_hub_at_its_recorded_commit(
    publish, github: FakeGitHub
) -> None:
    publish()

    for hub, commit in HUB_COMMITS.items():
        target = github.hub_tags[hub][HUB_TAG]
        # An annotated tag, whose object names the recorded commit.
        assert target["type"] == "tag"
        tag_object = github.tag_objects[target["sha"]]
        assert tag_object["object"] == {"type": "commit", "sha": commit}
        assert tag_object["tag"] == HUB_TAG
        assert tag_object["message"] == "Released with peppy v0.3.0"


def test_publish_keeps_a_hub_tag_already_at_the_recorded_commit(
    publish, github: FakeGitHub, events: list[str]
) -> None:
    # An earlier attempt, or an earlier run of v0.3.0, tagged two hubs.
    github.add_hub_tag("mcp-hub", HUB_TAG, HUB_COMMITS["mcp-hub"], annotated=True)
    github.add_hub_tag(
        "pairings-hub", HUB_TAG, HUB_COMMITS["pairings-hub"], annotated=False
    )
    kept = {hub: dict(github.hub_tags[hub]) for hub in ("mcp-hub", "pairings-hub")}

    publish()

    assert [event for event in events if event.startswith("tag ")] == [
        f"tag {hub}" for hub in HUB_COMMITS if hub not in kept
    ]
    for hub, tags in kept.items():
        assert github.hub_tags[hub] == tags
    for hub, commit in HUB_COMMITS.items():
        assert github.hub_tag_commit(hub, HUB_TAG) == commit
    assert len(github.published()) == 1


@pytest.mark.parametrize("annotated", [True, False])
def test_publish_refuses_a_hub_tag_at_another_commit_and_tags_no_hub(
    publish,
    release_repo: ReleaseRepo,
    github: FakeGitHub,
    events: list[str],
    annotated: bool,
) -> None:
    github.add_hub_tag("private-nodes-hub", HUB_TAG, OTHER_COMMIT, annotated=annotated)

    with pytest.raises(ReleaseError) as excinfo:
        publish()

    message = unwrapped(str(excinfo.value))
    assert (
        f"{HUB_TAG} already names another commit than the one this run tested"
    ) in message
    assert (
        f"private-nodes-hub: tagged at {OTHER_COMMIT[:12]}, tested at "
        f"{HUB_COMMITS['private-nodes-hub'][:12]}"
    ) in message
    assert "This publish tagged no hub and publishes nothing." in message
    # Every hub was read, none tagged, and the tag that was there stays.
    assert [event for event in events if event.startswith("tag ")] == []
    assert github.hub_tag_commit("private-nodes-hub", HUB_TAG) == OTHER_COMMIT
    assert "check the install" not in events
    assert github.releases == {}
    assert release_repo.remote_commit("dev") == release_repo.release_commit


def test_publish_checks_the_install_of_the_hosts_archive(
    publish, tmp_path: Path, install_check: InstallCheck
) -> None:
    publish()

    [archive] = install_check.archives
    assert archive.name == "peppy-x86_64-unknown-linux-gnu.tgz"
    assert archive.read_bytes() == b"archive x86_64-unknown-linux-gnu"


def test_publish_publishes_nothing_when_the_install_check_fails(
    publish,
    release_repo: ReleaseRepo,
    github: FakeGitHub,
    events: list[str],
    install_check: InstallCheck,
) -> None:
    install_check.failure = ReleaseError("`peppy repo refresh --strict` failed (exit 1)")

    with pytest.raises(ReleaseError, match="repo refresh --strict"):
        publish()

    # The hubs carry their tag, which no published peppy reads.
    assert [event for event in events if event.startswith("tag ")] == [
        f"tag {hub}" for hub in HUB_COMMITS
    ]
    assert "create the draft" not in events
    assert github.releases == {}
    assert release_repo.remote_commit("dev") == release_repo.release_commit
    assert release_repo.remote_commit("main") == release_repo.shipped_commit


def test_publish_rerun_after_a_failed_install_check_keeps_the_tags(
    publish, github: FakeGitHub, events: list[str], install_check: InstallCheck
) -> None:
    install_check.failure = ReleaseError("the daemon did not serve")
    with pytest.raises(ReleaseError):
        publish()
    events.clear()
    install_check.failure = None

    publish()

    # The re-run finds every tag at its recorded commit and tags nothing.
    assert [event for event in events if event.startswith("tag ")] == []
    assert "check the install" in events
    assert len(github.published()) == 1


def test_publish_refuses_a_hub_set_it_cannot_read(
    publish, tmp_path: Path, github: FakeGitHub
) -> None:
    (tmp_path / "hub-set.json").write_text('{"hubs": {}}')

    with pytest.raises(ReleaseError, match="leaves out nodes-hub"):
        publish()

    assert github.requests == []


def test_publish_refuses_a_published_release_tagged_elsewhere(
    publish, release_repo: ReleaseRepo, github: FakeGitHub
) -> None:
    github.add_release("v0.3.0", draft=False)
    release_repo.tag_on_origin(release_repo.shipped_commit)

    with pytest.raises(ReleaseError, match="its tag points at") as excinfo:
        publish()

    assert release_repo.shipped_commit[:12] in str(excinfo.value)
    assert release_repo.release_commit[:12] in str(excinfo.value)
    assert github.uploads == []
    assert release_repo.remote_commit("dev") == release_repo.release_commit
    assert release_repo.remote_commit("main") == release_repo.shipped_commit


def test_publish_refuses_notes_committed_without_a_published_release(
    publish, release_repo: ReleaseRepo, github: FakeGitHub, events: list[str]
) -> None:
    release_repo.commit_release_notes()

    with pytest.raises(ReleaseError, match="no published release of v0.3.0 exists"):
        publish()

    assert "create the draft" not in events
    assert release_repo.remote_commit("main") == release_repo.shipped_commit


def test_publish_deletes_a_draft_an_earlier_attempt_left(
    publish, github: FakeGitHub, events: list[str]
) -> None:
    leftover = github.add_release("v0.3.0", draft=True)

    publish()

    deleted = events.index(f"delete draft {leftover['id']}")
    assert deleted < events.index("create the draft")
    assert leftover["id"] not in github.releases
    assert len(github.releases) == 1


def test_publish_rerun_after_a_failed_upload_publishes_the_release(
    publish, release_repo: ReleaseRepo, github: FakeGitHub
) -> None:
    github.failing_upload = "peppy-x86_64-unknown-linux-gnu.tgz"

    with pytest.raises(ReleaseError, match="failed to upload asset"):
        publish()

    # The half-uploaded draft is gone, and nothing reached git.
    assert github.releases == {}
    assert release_repo.remote_commit("dev") == release_repo.release_commit

    github.failing_upload = None
    publish()

    [release] = github.published()
    assert release["tag_name"] == "v0.3.0"
    assert release_repo.remote_commit("main") == release_repo.remote_commit("dev")


def test_publish_rerun_after_a_failed_push_finishes_only_the_git_side(
    publish, release_repo: ReleaseRepo, github: FakeGitHub
) -> None:
    def rejected_main_push(remote: str, source: str, remote_branch: str) -> None:
        if remote_branch == "main":
            raise ReleaseError(f"failed to push '{source}' to 'origin/main': rejected")
        real_push_branch(remote, source, remote_branch)

    with patch(
        "functions.parallel_release.push_branch", side_effect=rejected_main_push
    ), pytest.raises(ReleaseError) as excinfo:
        publish()

    # The release is live, so the failure points at the re-run, which does
    # only what is left.
    message = " ".join(str(excinfo.value).split())
    assert "rejected" in message
    assert "The GitHub release v0.3.0 is published" in message
    assert "Re-run the failed publish job" in message
    notes_commit = release_repo.remote_commit("dev")
    assert release_repo.remote_commit("main") == release_repo.shipped_commit
    # GitHub created the tag when the release was published.
    release_repo.tag_on_origin(release_repo.release_commit)

    publish()

    assert len(github.published()) == 1
    assert github.uploads == _uploaded_names()
    assert release_repo.remote_commit("dev") == notes_commit
    assert release_repo.remote_commit("main") == notes_commit


def test_publish_removes_the_notes_worktree_when_the_commit_fails(
    publish, release_repo: ReleaseRepo, github: FakeGitHub
) -> None:
    with patch(
        "functions.parallel_release.commit_paths",
        side_effect=ReleaseError("failed to commit the notes"),
    ), pytest.raises(ReleaseError, match="Re-run the failed publish job"):
        publish()

    assert release_repo.worktrees() == [str(release_repo.path)]
    assert release_repo.remote_commit("dev") == release_repo.release_commit
    assert len(github.published()) == 1


def test_publish_stops_when_an_archive_is_missing(
    publish, tmp_path: Path, github: FakeGitHub, events: list[str]
) -> None:
    (tmp_path / "archives" / "peppy-x86_64-unknown-linux-gnu.tgz").unlink()

    with pytest.raises(ReleaseError, match="x86_64-unknown-linux-gnu archive is missing"):
        publish()

    assert "create the draft" not in events


def test_publish_requires_the_release_token(
    publish, monkeypatch: pytest.MonkeyPatch, events: list[str]
) -> None:
    monkeypatch.setenv("PEPPY_RELEASE_TOKEN", " ")

    with pytest.raises(ReleaseError, match="PEPPY_RELEASE_TOKEN env var is required"):
        publish()

    assert events == []


# --- the checks that let a publish run again ---


def _notes_commit_lookup(repo: ReleaseRepo) -> str | None:
    return _find_release_notes_commit(
        _git(repo.path, "rev-parse", "HEAD"), _plan_of(repo), Path(NOTES_FILE)
    )


def _notes_on_a_side_branch(repo: ReleaseRepo, branch: str, message: str) -> str:
    """Commit the notes on a branch of the release commit; return the commit."""
    _git(repo.path, "switch", "-q", "-c", branch, repo.release_commit)
    path = repo.path / NOTES_FILE
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text("<entry>v0.3.0</entry>\n")
    _git(repo.path, "add", NOTES_FILE)
    _git(repo.path, "commit", "-q", "-m", message)
    _git(repo.path, "switch", "-q", "dev")
    return _git(repo.path, "rev-parse", branch)


def _merge_into_dev(repo: ReleaseRepo, branch: str) -> None:
    _git(repo.path, "merge", "-q", "--no-ff", "-m", f"merge {branch}", branch)


def test_dev_at_the_release_commit_holds_no_notes_commit(
    release_repo: ReleaseRepo,
) -> None:
    assert _notes_commit_lookup(release_repo) is None


def test_the_notes_commit_dev_fast_forwarded_to_is_found(
    release_repo: ReleaseRepo,
) -> None:
    notes_commit = release_repo.commit_release_notes()
    release_repo.commit_on_dev({"later.rs": "fn later() {}\n"}, "later")

    assert _notes_commit_lookup(release_repo) == notes_commit


def test_the_notes_commit_merged_into_dev_that_moved_is_found(
    release_repo: ReleaseRepo,
) -> None:
    release_repo.commit_on_dev({"code.rs": "fn merged() {}\n"}, "merged")
    notes_commit = _notes_on_a_side_branch(release_repo, "notes", "the notes")
    _merge_into_dev(release_repo, "notes")
    release_repo.commit_on_dev({"later.rs": "fn later() {}\n"}, "later")

    assert _notes_commit_lookup(release_repo) == notes_commit


def test_the_notes_of_another_release_are_not_its_notes_commit(
    release_repo: ReleaseRepo,
) -> None:
    release_repo.commit_release_notes("v0.2.9")

    assert _notes_commit_lookup(release_repo) is None


@pytest.mark.parametrize(
    "change_notes",
    [
        # The notes, and a code change in the same commit.
        lambda repo: repo.commit_on_dev(
            {NOTES_FILE: "<entry>v0.3.0</entry>\n", "code.rs": "fn merged() {}\n"},
            "notes and code",
        ),
        # The notes alone, on a commit that is not the release commit.
        lambda repo: (
            repo.commit_on_dev({"code.rs": "fn merged() {}\n"}, "merged"),
            repo.commit_release_notes(),
        )[-1],
    ],
    ids=["notes-and-code", "notes-off-the-release-commit"],
)
def test_other_changes_to_the_notes_file_are_refused(
    release_repo: ReleaseRepo, change_notes: Callable[[ReleaseRepo], str]
) -> None:
    changed = change_notes(release_repo)

    with pytest.raises(ReleaseError) as excinfo:
        _notes_commit_lookup(release_repo)

    message = unwrapped(str(excinfo.value))
    assert (
        f"'dev' changed {NOTES_FILE} since {release_repo.release_commit[:12]}"
    ) in message
    assert changed[:12] in message
    assert "cannot merge into 'dev'" in message


def test_two_notes_commits_of_the_release_are_refused(
    release_repo: ReleaseRepo,
) -> None:
    first = _notes_on_a_side_branch(release_repo, "first", "the notes")
    second = _notes_on_a_side_branch(release_repo, "second", "the notes again")
    _merge_into_dev(release_repo, "first")
    _merge_into_dev(release_repo, "second")

    with pytest.raises(ReleaseError) as excinfo:
        _notes_commit_lookup(release_repo)

    message = unwrapped(str(excinfo.value))
    assert "'dev' holds 2 release notes commits of v0.3.0" in message
    assert first[:12] in message
    assert second[:12] in message


def test_find_published_release_is_none_while_there_is_none(
    release_repo: ReleaseRepo, github: FakeGitHub, github_client: httpx.Client
) -> None:
    # A draft of the tag is not a published release.
    github.add_release("v0.3.0", draft=True)

    assert _find_published_release(github_client, SLUG, _plan_of(release_repo)) is None


def test_find_published_release_checks_the_tag_before_answering(
    release_repo: ReleaseRepo, github: FakeGitHub, github_client: httpx.Client
) -> None:
    published = github.add_release("v0.3.0", draft=False)
    release_repo.tag_on_origin(release_repo.release_commit)

    release = _find_published_release(github_client, SLUG, _plan_of(release_repo))

    assert release is not None
    assert release.release_id == published["id"]
    assert release.html_url.endswith("/releases/tag/v0.3.0")


def test_main_already_at_reads_origin_main(release_repo: ReleaseRepo) -> None:
    assert _main_already_at(release_repo.shipped_commit) is True
    assert _main_already_at(release_repo.release_commit) is False


# --- the draft release ---

UNTAGGED_DRAFT_URL = "https://github.com/t/releases/tag/untagged-1"


@patch("functions.parallel_release.delete_draft_release", return_value=True)
@patch("functions.parallel_release.publish_release")
@patch("functions.parallel_release.replace_and_upload_asset", side_effect=KeyboardInterrupt)
def test_upload_interrupt_deletes_the_draft(
    mock_upload: MagicMock,
    mock_publish: MagicMock,
    mock_delete_draft: MagicMock,
    tmp_path: Path,
    capfd: pytest.CaptureFixture[str],
) -> None:
    client = MagicMock()
    draft = ReleaseInfo(release_id=1, html_url=UNTAGGED_DRAFT_URL)
    archives = _built_archives(tmp_path / "archives")

    # The interrupt a cancelled CI run hands the script.
    with pytest.raises(KeyboardInterrupt):
        _upload_archives_and_publish(
            client,
            SLUG,
            draft,
            [BuildArtifact(p.name, p, "t") for p in sorted(archives.iterdir())],
        )

    mock_delete_draft.assert_called_once_with(client, 1, SLUG)
    mock_publish.assert_not_called()
    assert "Draft release deleted." in capfd.readouterr().err


@patch("functions.parallel_release.delete_draft_release", return_value=False)
@patch("functions.parallel_release.publish_release", side_effect=KeyboardInterrupt)
@patch("functions.parallel_release.replace_and_upload_asset")
def test_publish_interrupt_leaves_a_release_that_went_live(
    mock_upload: MagicMock,
    mock_publish: MagicMock,
    mock_delete_draft: MagicMock,
    capfd: pytest.CaptureFixture[str],
) -> None:
    draft = ReleaseInfo(release_id=1, html_url=UNTAGGED_DRAFT_URL)

    with pytest.raises(KeyboardInterrupt):
        _upload_archives_and_publish(MagicMock(), SLUG, draft, [])

    output = unwrapped(capfd.readouterr().err)
    assert "Draft release deleted." not in output
    assert (
        "The release went live before the failure, so it is left in place: "
        "https://github.com/test-owner/test-repo/releases"
    ) in output


@patch(
    "functions.parallel_release.delete_draft_release",
    side_effect=ReleaseError("cleanup failed"),
)
@patch("functions.parallel_release.publish_release")
@patch(
    "functions.parallel_release.replace_and_upload_asset",
    side_effect=ReleaseError("upload timeout"),
)
def test_failed_draft_cleanup_warns_and_keeps_the_upload_error(
    mock_upload: MagicMock,
    mock_publish: MagicMock,
    mock_delete_draft: MagicMock,
    tmp_path: Path,
    capfd: pytest.CaptureFixture[str],
) -> None:
    draft = ReleaseInfo(release_id=1, html_url=UNTAGGED_DRAFT_URL)
    archive = _built_archives(tmp_path / "archives") / "peppy-aarch64-apple-darwin.tgz"

    # The original error is raised, not the cleanup error.
    with pytest.raises(ReleaseError, match="upload timeout"):
        _upload_archives_and_publish(
            MagicMock(), SLUG, draft, [BuildArtifact(archive.name, archive, "t")]
        )

    mock_publish.assert_not_called()
    assert "Manual cleanup required" in unwrapped(capfd.readouterr().err)


# --- hub-check ---


def test_hub_check_checks_the_recorded_set_with_the_archive(tmp_path: Path) -> None:
    hub_set_path = tmp_path / "hub-set.json"
    hub_set_path.write_text(_hub_set_text())
    archive = tmp_path / "peppy-x86_64-unknown-linux-gnu.tgz"

    with patch("functions.parallel_release.check_hub_set") as check_hub_set:
        run_hub_check(hub_set_path=hub_set_path, archive=archive)

    [(hub_set, checked_archive)] = [call.args for call in check_hub_set.call_args_list]
    assert {r.hub.name: r.commit for r in hub_set.hubs} == HUB_COMMITS
    assert checked_archive == archive


def test_hub_check_refuses_a_hub_set_it_cannot_read(tmp_path: Path) -> None:
    hub_set_path = tmp_path / "hub-set.json"
    hub_set_path.write_text("not json")

    with (
        patch("functions.parallel_release.check_hub_set") as check_hub_set,
        pytest.raises(ReleaseError, match="is not JSON"),
    ):
        run_hub_check(hub_set_path=hub_set_path, archive=tmp_path / "peppy.tgz")

    check_hub_set.assert_not_called()
