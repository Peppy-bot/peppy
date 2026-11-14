"""Cargo build, binary discovery, and tarball packaging."""

from __future__ import annotations

import os
import shutil
import subprocess
import sys
import tarfile
import tempfile
from collections import deque
from dataclasses import dataclass
from pathlib import Path

from .cli import RELEASE_TRIPLES, ReleaseError, console

# Commands a release build needs on its host.
BUILD_COMMANDS = ("git", "cargo", "rustc")

# Where the build script of a package leaves what the release packs, as glob
# patterns relative to {target_dir}/{target_triple}/release/build. Cargo gives
# each unit of a package a directory of its own under the name of the package.
ZENOHD_BUILD_OUTPUT = "pmi/*/out/zenohd"
APPTAINER_BUILD_OUTPUT = "containers/*/out/apptainer-install"
LIMA_BUILD_OUTPUT = "containers/*/out/lima-install"


@dataclass(frozen=True)
class BuildArtifact:
    """Result of a successful build-and-package operation."""

    asset_name: str
    asset_path: Path
    target_triple: str


def _release_rustflags(repo_root: Path) -> str:
    """RUSTFLAGS that keep build machine paths out of the compiled binary.

    rustc records absolute source paths in panic locations and debug data: the
    cargo registry under CARGO_HOME and the workspace checkout both name the
    machine the release is built on. Remapping them to fixed prefixes removes
    that identity from the shipped binary. Appended to any RUSTFLAGS the
    environment already carries so local wrappers keep working.
    """
    cargo_home = Path(os.environ.get("CARGO_HOME", str(Path.home() / ".cargo")))
    parts = [
        os.environ.get("RUSTFLAGS", ""),
        f"--remap-path-prefix={cargo_home}=/cargo-home",
        f"--remap-path-prefix={repo_root}=/peppy",
    ]
    return " ".join(part for part in parts if part)


def cargo_build(
    tag: str,
    target_triple: str,
    repo_root: Path,
    *,
    target_dir: Path | None = None,
) -> None:
    """Run a release build for the peppy binary.

    Sets PEPPY_GIT_TAG env var for the build. When `target_dir` is given, cargo
    writes into that tree instead of the shared one, which is how a release
    invocation keeps pre-existing build output out of its archive.
    Each target triple gets its own directory under target/{triple}/release/,
    so no cargo clean is needed.

    Streams cargo's output live while retaining a bounded tail, so that
    when the build fails (e.g. a transient sccache crash) the error
    message includes the actual cargo output, even when the caller
    captures stdio, as pytest does by default.
    """
    console.print(f"Building peppy for [bold]{target_triple}[/bold]...")
    env = {**os.environ, "PEPPY_GIT_TAG": tag, "PEPPY_CROSS_ARCH": "1"}
    env["RUSTFLAGS"] = _release_rustflags(repo_root)
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
            # Build scripts forward output of any encoding: a strict decode
            # would raise in the middle of the stream and lose the tail.
            encoding="utf-8",
            errors="replace",
            bufsize=1,
        )
    except OSError as exc:
        raise ReleaseError(
            f"Failed to start cargo build for {target_triple}: {exc}"
        ) from exc
    if proc.stdout is None:
        raise ReleaseError("cargo process started but stdout is not available")
    tail: deque[str] = deque(maxlen=200)
    for line in proc.stdout:
        sys.stdout.write(line)
        sys.stdout.flush()
        tail.append(line)
    returncode = proc.wait()
    if returncode != 0:
        raise ReleaseError(
            f"cargo build failed (exit {returncode})\n"
            f"--- last {len(tail)} lines of cargo output ---\n" + "".join(tail)
        )


def _get_target_dir(repo_root: Path) -> Path:
    """Get the cargo target directory, respecting CARGO_TARGET_DIR.

    Cargo resolves a relative CARGO_TARGET_DIR against the current working
    directory, so a bare value like `target-release` must be resolved against
    repo_root here too; otherwise this function and cargo look in different
    places and artifact discovery silently finds nothing. An absolute value is
    honoured verbatim, including one outside repo_root: that is a legitimate
    configuration for a native build.
    """
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


def find_zenohd_binary(
    target_triple: str,
    repo_root: Path,
    *,
    target_dir: Path | None = None,
) -> Path:
    """Locate the built zenohd binary in the build output.

    Searches {target_dir}/{target_triple}/release/build/{ZENOHD_BUILD_OUTPUT}.
    Raises ReleaseError if not found.
    """
    target_dir = target_dir or _get_target_dir(repo_root)
    build_dir = target_dir / target_triple / "release" / "build"

    matches = sorted(build_dir.glob(ZENOHD_BUILD_OUTPUT))
    if matches:
        return matches[0]

    raise ReleaseError(
        f"zenohd binary not found in build output "
        f"(expected it under '{build_dir}/{ZENOHD_BUILD_OUTPUT}')"
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


def find_optional_build_dir(
    target_triple: str,
    repo_root: Path,
    pattern: str,
    *,
    target_dir: Path | None = None,
) -> Path | None:
    """Locate a directory the build output may hold: the first match of
    {target_dir}/{target_triple}/release/build/{pattern}, or None."""
    target_dir = target_dir or _get_target_dir(repo_root)
    build_dir = target_dir / target_triple / "release" / "build"
    matches = sorted(build_dir.glob(pattern))
    if matches and matches[0].is_dir():
        return matches[0]
    return None


def release_dist_dir(repo_root: Path) -> Path:
    """The directory release archives are written to.

    PEPPY_DIST_DIR overrides the default of {repo_root}/dist. Everything that
    reads or writes release archives resolves the directory here, so the
    override is honoured consistently.
    """
    return Path(os.environ.get("PEPPY_DIST_DIR", str(repo_root / "dist")))


def build_cache_root() -> Path:
    """The persistent cache the Rust build scripts share.

    build-helpers' `cache_dir` roots it at `$HOME/.peppy/tmp` whatever
    PEPPY_HOME says.
    """
    return Path.home() / ".peppy" / "tmp"


def release_archive_name(target_triple: str) -> str:
    """The file name of the release archive of *target_triple*.

    install.sh downloads the release asset of this name, and the release
    workflow hands the archive between its jobs under it.
    """
    return f"peppy-{target_triple}.tgz"


def _sanitize_tar_member(member: tarfile.TarInfo) -> tarfile.TarInfo:
    """Zero member ownership so the archive never carries the packer's uid,
    gid, or user and group names. A root install restoring them would either
    abort in a user namespace or hand the tree to the packing machine's (or a
    same-named local) account."""
    member.uid = 0
    member.gid = 0
    member.uname = ""
    member.gname = ""
    return member


def other_linux_arch(target_triple: str) -> str:
    """The Linux architecture other than the one the containers of
    *target_triple* run on: a macOS target runs its containers in an aarch64
    VM."""
    container_arch = (
        "aarch64" if "apple-darwin" in target_triple else target_triple.split("-", 1)[0]
    )
    return "aarch64" if container_arch == "x86_64" else "x86_64"


def package_release(
    target_triple: str,
    repo_root: Path,
    peppy_bin: Path,
    zenohd_bin: Path,
    apptainer_dir: Path,
    lima_dir: Path | None = None,
    other_apptainer: tuple[str, Path] | None = None,
) -> BuildArtifact:
    """Create a .tgz release archive.

    Creates a temporary directory with the package layout:
      ./bin/peppy
      ./bin/zenohd
      ./bin/apptainer/          (the Linux architecture of the target)
      ./bin/apptainer-<arch>/   (the other Linux architecture, when built)
      ./bin/lima/               (when built)

    The apptainer and Lima of the other architecture run the builds for its
    platform in an emulated VM (`peppy node publish --platforms`); on macOS,
    Lima also runs every container. A release has both (see
    verify_release); a local build of one architecture has the apptainer of
    that architecture alone.

    Writes the archive to {dist_dir}/peppy-{target_triple}.tgz.
    """
    dist_dir = release_dist_dir(repo_root)
    dist_dir.mkdir(parents=True, exist_ok=True)

    asset_name = release_archive_name(target_triple)
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
        if other_apptainer is not None:
            other_arch, other_apptainer_dir = other_apptainer
            shutil.copytree(other_apptainer_dir, bin_dir / f"apptainer-{other_arch}")
        if lima_dir is not None:
            shutil.copytree(lima_dir, bin_dir / "lima")

        with tarfile.open(asset_path, "w:gz") as tar:
            for child in sorted(pkg_dir.iterdir()):
                tar.add(child, arcname=f"./{child.name}", filter=_sanitize_tar_member)

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
    # A release invocation gets a fresh cargo output tree, so no pre-existing
    # build output of `pmi` or `containers` on a long-lived builder can be
    # globbed into the archive. The cost is that a release build is always a
    # cold build.
    #
    # The isolated tree must stay under `target_root`. The Lima cross build
    # compiles on the guest's own disk but copies its artifacts back into this
    # host path, which the guest reaches through the repo mount; the default
    # `repo_root/target` satisfies that automatically, and an operator who
    # points CARGO_TARGET_DIR at an absolute path elsewhere owns keeping it
    # visible to the VM.
    with tempfile.TemporaryDirectory(
        prefix=f".peppy-release-{target_triple}-",
        dir=target_root,
    ) as isolated_target:
        target_dir = Path(isolated_target)
        if limactl is not None:
            # Imported lazily: `lima` imports `_get_target_dir` from this module
            # at module level, so hoisting this to the top would create a
            # circular import.
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

        peppy_bin = find_peppy_binary(target_triple, repo_root, target_dir=target_dir)
        zenohd_bin = find_zenohd_binary(target_triple, repo_root, target_dir=target_dir)

        apptainer_dir = find_build_dir(
            target_triple,
            repo_root,
            APPTAINER_BUILD_OUTPUT,
            "apptainer install",
            target_dir=target_dir,
        )

        # macOS runs every container in Lima, so its build always has it. A
        # Linux build has it when its build script could download it.
        lima_dir = (
            find_build_dir(
                target_triple,
                repo_root,
                LIMA_BUILD_OUTPUT,
                "lima install",
                target_dir=target_dir,
            )
            if "apple-darwin" in target_triple
            else find_optional_build_dir(
                target_triple,
                repo_root,
                LIMA_BUILD_OUTPUT,
                target_dir=target_dir,
            )
        )
        other_arch = other_linux_arch(target_triple)
        other_apptainer_dir = find_optional_build_dir(
            target_triple,
            repo_root,
            f"{APPTAINER_BUILD_OUTPUT}-{other_arch}",
            target_dir=target_dir,
        )

        # Inside the `with`, because the tarball is assembled from files under
        # the isolated tree and `TemporaryDirectory` deletes it on exit.
        return package_release(
            target_triple,
            repo_root,
            peppy_bin,
            zenohd_bin,
            apptainer_dir,
            lima_dir,
            (other_arch, other_apptainer_dir) if other_apptainer_dir else None,
        )
