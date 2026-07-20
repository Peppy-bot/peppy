"""Cargo build, binary discovery, and tarball packaging."""

from __future__ import annotations

import os
import subprocess
import sys
import tarfile
import tempfile
import shutil
from collections import deque
from dataclasses import dataclass
from pathlib import Path

from .cli import RELEASE_TRIPLES, ReleaseError, console

ZENOHD_TLS_POLICY_MARKER = (
    b"PEPPY_ZENOHD_TLS_POLICY=custom-roots-exclusive;"
    b"system-roots=platform-verifier;mtls=tls13;v=1"
)


@dataclass(frozen=True)
class BuildArtifact:
    """Result of a successful build-and-package operation."""

    asset_name: str
    asset_path: Path
    target_triple: str


def cargo_build(
    tag: str,
    target_triple: str,
    repo_root: Path,
    *,
    target_dir: Path | None = None,
) -> None:
    """Run a release build for the peppy binary.

    Sets PEPPY_GIT_TAG env var for the build.
    Each target triple gets its own directory under target/{triple}/release/,
    so no cargo clean is needed.

    Streams cargo's output live while retaining a bounded tail, so that
    when the build fails (e.g. a transient sccache crash) the error
    message includes the actual cargo output — even when the caller
    captures stdio, as pytest does by default.
    """
    console.print(f"Building peppy for [bold]{target_triple}[/bold]...")
    env = {**os.environ, "PEPPY_GIT_TAG": tag, "PEPPY_CROSS_ARCH": "1"}
    if target_dir is not None:
        env["CARGO_TARGET_DIR"] = str(target_dir)
    try:
        proc = subprocess.Popen(
            [
                "cargo",
                "build",
                "-p",
                "peppy",
                "--release",
                "--locked",
                "--target",
                target_triple,
            ],
            cwd=repo_root,
            env=env,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            text=True,
            bufsize=1,
        )
    except OSError as exc:
        raise ReleaseError(
            f"Failed to start cargo build for {target_triple}: {exc}"
        ) from exc
    if proc.stdout is None:
        raise ReleaseError(
            "cargo process started but stdout is not available"
        )
    tail: deque[str] = deque(maxlen=200)
    for line in proc.stdout:
        sys.stdout.write(line)
        sys.stdout.flush()
        tail.append(line)
    returncode = proc.wait()
    if returncode != 0:
        raise ReleaseError(
            f"cargo build failed (exit {returncode})\n"
            f"--- last {len(tail)} lines of cargo output ---\n"
            + "".join(tail)
        )


def _get_target_dir(repo_root: Path) -> Path:
    """Get the cargo target directory, respecting CARGO_TARGET_DIR."""
    configured = Path(os.environ.get("CARGO_TARGET_DIR", str(repo_root / "target")))
    return configured if configured.is_absolute() else repo_root / configured


def find_peppy_binary(
    target_triple: str,
    repo_root: Path,
    *,
    target_dir: Path | None = None,
) -> Path:
    """Locate the compiled peppy binary in the target directory.

    Checks {target_dir}/{target_triple}/release/peppy first, then
    falls back to {target_dir}/release/peppy.
    """
    target_dir = target_dir or _get_target_dir(repo_root)

    primary = target_dir / target_triple / "release" / "peppy"
    if primary.is_file():
        return primary

    fallback = target_dir / "release" / "peppy"
    if fallback.is_file():
        return fallback

    raise ReleaseError(f"peppy binary not found (expected '{primary}')")


def _contains_bytes(path: Path, needle: bytes) -> bool:
    """Search a binary without loading the whole artifact into memory."""
    overlap = b""
    try:
        with path.open("rb") as artifact:
            while chunk := artifact.read(1024 * 1024):
                window = overlap + chunk
                if needle in window:
                    return True
                overlap = window[-(len(needle) - 1) :]
    except OSError as exc:
        raise ReleaseError(f"failed to inspect zenohd artifact '{path}': {exc}") from exc
    return False


def validate_zenohd_tls_policy(path: Path) -> None:
    """Require the patched Zenoh TLS-policy marker in an exact artifact."""
    if not path.is_file():
        raise ReleaseError(f"zenohd artifact is not a regular file: '{path}'")
    if not _contains_bytes(path, ZENOHD_TLS_POLICY_MARKER):
        raise ReleaseError(
            f"zenohd artifact '{path}' lacks Peppy's required exclusive-root "
            "mTLS policy marker"
        )


def find_zenohd_binary(
    target_triple: str,
    repo_root: Path,
    *,
    target_dir: Path | None = None,
) -> Path:
    """Locate the built zenohd binary in the build output.

    Searches {target_dir}/{target_triple}/release/build/pmi-*/out/zenohd.
    Only a binary carrying Peppy's exact TLS-policy marker is eligible. An
    ambiguous build tree fails closed instead of selecting a stale candidate.
    """
    target_dir = target_dir or _get_target_dir(repo_root)
    build_dir = target_dir / target_triple / "release" / "build"

    matches = sorted(path for path in build_dir.glob("pmi-*/out/zenohd") if path.is_file())
    verified = [
        path for path in matches if _contains_bytes(path, ZENOHD_TLS_POLICY_MARKER)
    ]
    if len(verified) == 1:
        return verified[0]
    if len(verified) > 1:
        candidates = ", ".join(str(path) for path in verified)
        raise ReleaseError(
            "multiple TLS-policy-verified zenohd build artifacts were found; "
            f"refusing an ambiguous package selection: {candidates}"
        )
    if matches:
        raise ReleaseError(
            "zenohd build artifacts were found, but none carries Peppy's "
            "required exclusive-root mTLS policy marker"
        )

    raise ReleaseError(
        f"zenohd binary not found in build output "
        f"(expected it under '{build_dir}/pmi-*/out/zenohd')"
    )


def find_build_dir(
    target_triple: str,
    repo_root: Path,
    pattern: str,
    label: str,
    *,
    target_dir: Path | None = None,
) -> Path:
    """Locate a required directory in the build output.

    Searches {target_dir}/{target_triple}/release/build/{pattern}.
    Raises ReleaseError if not found.
    """
    target_dir = target_dir or _get_target_dir(repo_root)
    build_dir = target_dir / target_triple / "release" / "build"

    matches = sorted(build_dir.glob(pattern))
    if matches and matches[0].is_dir():
        return matches[0]

    raise ReleaseError(
        f"{label} directory not found in build output "
        f"(expected it under '{build_dir}/{pattern}')"
    )


def package_release(
    target_triple: str,
    repo_root: Path,
    peppy_bin: Path,
    zenohd_bin: Path,
    apptainer_dir: Path,
    lima_dir: Path | None = None,
) -> BuildArtifact:
    """Create a .tgz release archive.

    Creates a temporary directory with the package layout:
      ./bin/peppy
      ./bin/zenohd
      ./bin/apptainer/
      ./bin/lima/         (macOS only)

    Writes the archive to {dist_dir}/peppy-{target_triple}.tgz.
    """
    # Recheck the exact source immediately before it is copied. Callers cannot
    # bypass the policy gate by supplying a stock or stale zenohd directly.
    validate_zenohd_tls_policy(zenohd_bin)

    dist_dir = Path(os.environ.get("PEPPY_DIST_DIR", str(repo_root / "dist")))
    dist_dir.mkdir(parents=True, exist_ok=True)

    asset_name = f"peppy-{target_triple}.tgz"
    asset_path = dist_dir / asset_name

    with tempfile.TemporaryDirectory(prefix="peppy_release_pkg_") as pkg_dir_str:
        pkg_dir = Path(pkg_dir_str)
        bin_dir = pkg_dir / "bin"
        bin_dir.mkdir()

        shutil.copy2(peppy_bin, bin_dir / "peppy")
        (bin_dir / "peppy").chmod(0o755)

        shutil.copy2(zenohd_bin, bin_dir / "zenohd")
        (bin_dir / "zenohd").chmod(0o755)

        shutil.copytree(apptainer_dir, bin_dir / "apptainer")
        if lima_dir is not None:
            shutil.copytree(lima_dir, bin_dir / "lima")

        with tarfile.open(asset_path, "w:gz") as tar:
            for child in sorted(pkg_dir.iterdir()):
                tar.add(child, arcname=f"./{child.name}")

    console.print(f"Built artifact: [bold]{asset_path}[/bold]")
    return BuildArtifact(
        asset_name=asset_name,
        asset_path=asset_path,
        target_triple=target_triple,
    )


def build_and_package(
    tag: str,
    target_triple: str,
    repo_root: Path,
    *,
    limactl: Path | None = None,
) -> BuildArtifact:
    """Build and package for a specific target triple.

    If limactl is provided, builds inside the Lima VM (for Linux targets from macOS).
    Otherwise builds natively.
    """
    if target_triple not in RELEASE_TRIPLES:
        supported = ", ".join(sorted(RELEASE_TRIPLES))
        raise ReleaseError(
            f"unsupported target '{target_triple}' (supported: {supported})"
        )

    target_root = _get_target_dir(repo_root)
    target_root.mkdir(parents=True, exist_ok=True)
    # A release invocation gets a fresh Cargo output tree. This makes candidate
    # discovery exact even on long-lived builders whose ordinary target tree
    # contains old PMI build hashes.
    with tempfile.TemporaryDirectory(
        prefix=f".peppy-release-{target_triple}-",
        dir=target_root,
    ) as isolated_target:
        target_dir = Path(isolated_target)
        if limactl is not None:
            from .lima import cargo_build_in_lima

            cargo_build_in_lima(
                limactl,
                tag,
                target_triple,
                repo_root,
                target_dir=target_dir,
            )
        else:
            cargo_build(
                tag,
                target_triple,
                repo_root,
                target_dir=target_dir,
            )

        peppy_bin = find_peppy_binary(
            target_triple, repo_root, target_dir=target_dir
        )
        zenohd_bin = find_zenohd_binary(
            target_triple, repo_root, target_dir=target_dir
        )

        apptainer_dir = find_build_dir(
            target_triple,
            repo_root,
            "containers-*/out/apptainer-install",
            "apptainer install",
            target_dir=target_dir,
        )

        lima_dir = (
            find_build_dir(
                target_triple,
                repo_root,
                "containers-*/out/lima-install",
                "lima install",
                target_dir=target_dir,
            )
            if "apple-darwin" in target_triple
            else None
        )

        return package_release(
            target_triple,
            repo_root,
            peppy_bin,
            zenohd_bin,
            apptainer_dir,
            lima_dir,
        )
