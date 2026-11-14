"""Build peppy release archives on this host without publishing them, or
publish the Docker base images.

Releases publish only from the "Parallel release" workflow
(.github/workflows/parallel-release.yml, see `parallel_release`). With
`--local`, this script builds the archives a release ships so they can be
tested before one is cut:

On macOS ARM64: builds all 3 targets (native + Linux via Lima VM).
On Linux: builds the native target only.

Requires:
  - git, cargo, rustc on PATH
  - Lima VM (macOS only, auto-managed)

Outputs:
  - Tar.gz archives in ./dist/

The workflow does not publish the Docker base images, which need a login to
Docker Hub. `--base-images` and then `--base-images-latest` publish them from
this host (see `docker`).
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
from .docker import move_latest_main as move_base_images_latest_main
from .lima import ensure_lima_vm, ensure_rust_in_vm, find_limactl, stop_lima_vm
from .repo import get_repo_root, has_uncommitted_changes
from .verify_release import verify_all_releases

_NO_MODE_MESSAGE = (
    'releases publish only from the "Parallel release" workflow '
    "(.github/workflows/parallel-release.yml). Pass --local to build the "
    "release archives on this host without publishing them, --base-images to "
    "build the Docker base images and push them under a tag of their own, or "
    "--base-images-latest to move `latest` of the Docker base images to such "
    "a tag."
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
        help=(
            "Build the Docker base images on the Ubuntu snapshot of one hour "
            "ago, push them to Docker Hub under a tag of their own, and probe "
            "them. "
            "`latest` does not move."
        ),
    )
    mode.add_argument(
        "--base-images-latest",
        action="store_true",
        help=(
            "Move `latest` of the Docker base images to a tag that "
            "--base-images pushed."
        ),
    )
    parser.add_argument(
        "--tag",
        help=(
            "The tag to build the artifacts for (--local), to push the base "
            "images under (--base-images) or to move `latest` to "
            "(--base-images-latest), instead of prompting for one."
        ),
    )
    args = parser.parse_args(argv)
    if not (args.local or args.base_images or args.base_images_latest):
        parser.error(_NO_MODE_MESSAGE)
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
                    ensure_rust_in_vm(limactl, repo_root)
                artifact = build_and_package(tag, triple, repo_root, limactl=limactl)
            else:
                artifact = build_and_package(tag, triple, repo_root)
            artifacts.append(artifact)

        # Verify the release archives that were actually built
        if artifacts:
            dist_dir = artifacts[0].asset_path.parent
            # A local build provisions the apptainer of its own architecture
            # alone, so its archives may lack what builds for the other
            # platform need.
            verify_all_releases(dist_dir, targets, ships_other_platform=False)
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

    if has_uncommitted_changes() and not prompt_yn(
        "Working tree has uncommitted changes. Continue?"
    ):
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
        build_base_images_main(args.tag)
        return
    if args.base_images_latest:
        move_base_images_latest_main(args.tag)
        return
    run_with_error_handling(lambda: _run_local(args.tag))
