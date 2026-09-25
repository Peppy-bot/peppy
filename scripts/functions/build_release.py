"""Build peppy release archives on this host without publishing them.

Releases publish only from the "Parallel release" workflow
(.github/workflows/parallel-release.yml, see `parallel_release`). This script
builds the archives a release ships so they can be tested before one is cut:

On macOS ARM64: builds all 3 targets (native + Linux via Lima VM).
On Linux: builds the native target only.

Requires:
  - git, cargo, rustc on PATH
  - Lima VM (macOS only, auto-managed)

Outputs:
  - Tar.gz archives in ./dist/
"""

from __future__ import annotations

import argparse
import os
import sys
from pathlib import Path

from .build import BUILD_COMMANDS, BuildArtifact, build_and_package
from .cli import (
    ReleaseError,
    console,
    get_targets_for_platform,
    is_macos_arm64,
    need_cmd,
    prompt,
    prompt_yn,
    run_with_error_handling,
)
from .docker import main as build_base_images_main
from .lima import ensure_lima_vm, ensure_rust_in_vm, find_limactl, stop_lima_vm
from .repo import get_repo_root, has_uncommitted_changes
from .verify_release import verify_all_releases

_NO_MODE_MESSAGE = (
    'releases publish only from the "Parallel release" workflow '
    "(.github/workflows/parallel-release.yml). Pass --local to build the "
    "release archives on this host without publishing them, or --base-images "
    "to build and push the Docker base images."
)


def _parse_args(argv: list[str] | None = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description=(
            "Build peppy release archives on this host without publishing them, "
            "or the Docker base images. Releases publish only from the "
            '"Parallel release" workflow.'
        )
    )
    mode = parser.add_mutually_exclusive_group()
    mode.add_argument(
        "--local",
        action="store_true",
        help="Build the release archives on this host without publishing them.",
    )
    mode.add_argument(
        "--base-images",
        action="store_true",
        help="Build and push Docker base images to Docker Hub.",
    )
    parser.add_argument(
        "--tag",
        help=(
            "With --local, the tag to build the artifacts for, instead of "
            "prompting for one."
        ),
    )
    args = parser.parse_args(argv)
    if not args.local and not args.base_images:
        parser.error(_NO_MODE_MESSAGE)
    if args.tag is not None and not args.local:
        parser.error("--tag applies to --local only")
    return args


def _build_all_targets(
    tag: str,
    targets: list[str],
    repo_root: Path,
) -> list[BuildArtifact]:
    """Build and package for all requested targets.

    On macOS: builds the native target first (which triggers Lima download
    via the containers crate build.rs), then uses Lima for Linux targets.
    Targets are built sequentially to avoid cargo metadata conflicts.
    """
    artifacts: list[BuildArtifact] = []
    limactl: Path | None = None

    try:
        for triple in targets:
            if "linux" in triple and is_macos_arm64():
                if limactl is None:
                    limactl = find_limactl(repo_root)
                    ensure_lima_vm(limactl)
                    ensure_rust_in_vm(limactl)
                artifact = build_and_package(tag, triple, repo_root, limactl=limactl)
            else:
                artifact = build_and_package(tag, triple, repo_root)
            artifacts.append(artifact)

        # Verify the release archives that were actually built
        if artifacts:
            dist_dir = artifacts[0].asset_path.parent
            verify_all_releases(dist_dir, targets)
            console.print("[green]All release archives verified successfully.[/green]")

        return artifacts
    finally:
        # The Lima VM is only started for Linux cross-builds. Stop it once the
        # build finishes (successfully or not) so it does not keep holding host
        # RAM. ensure_lima_vm restarts a stopped instance on the next build.
        if limactl is not None:
            stop_lima_vm(limactl)


def _run_local(tag: str | None = None) -> None:
    """Build release artifacts locally without uploading to GitHub.

    Prompts for the tag unless *tag* names it.
    """
    for cmd in BUILD_COMMANDS:
        need_cmd(cmd)
    repo_root = get_repo_root()
    os.chdir(repo_root)

    if has_uncommitted_changes():
        if not prompt_yn("Working tree has uncommitted changes. Continue?"):
            sys.exit(1)

    if tag is None:
        tag = prompt("Tag for the build (example: v0.0.1)")
    if not tag:
        raise ReleaseError("release tag cannot be empty")

    targets = get_targets_for_platform()
    artifacts = _build_all_targets(tag, targets, repo_root)

    console.print()
    for artifact in artifacts:
        console.print(f"[green]Built:[/green] {artifact.asset_path}", soft_wrap=True)


def main() -> None:
    args = _parse_args()
    if args.base_images:
        build_base_images_main()
        return
    run_with_error_handling(lambda: _run_local(args.tag))
