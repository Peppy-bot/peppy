"""Lima VM management for cross-compiling Linux targets from macOS."""

from __future__ import annotations

import os
import subprocess
from pathlib import Path

from .cli import ReleaseError, console

LIMA_HOME = Path.home() / ".peppy" / "lima-build"
LIMA_INSTANCE = "peppy"
GUEST_RUST_DIR = "/opt/peppy-rust"
GUEST_RUSTUP_HOME = f"{GUEST_RUST_DIR}/rustup"
GUEST_CARGO_HOME = f"{GUEST_RUST_DIR}/cargo"


def find_limactl(repo_root: Path) -> Path:
    """Find the limactl binary from the macOS build output or cache.

    Checks the cargo build output first (created by the containers crate build.rs),
    then falls back to the peppy Lima cache directory.
    """
    target_dir = Path(
        os.environ.get("CARGO_TARGET_DIR", str(repo_root / "target"))
    )
    build_dir = target_dir / "aarch64-apple-darwin" / "release" / "build"
    matches = sorted(build_dir.glob("containers-*/out/lima-install/bin/limactl"))
    if matches and matches[0].is_file():
        return matches[0]

    cache_matches = sorted(Path.home().glob(".peppy/tmp/lima-*/bin/limactl"))
    if cache_matches and cache_matches[-1].is_file():
        return cache_matches[-1]

    raise ReleaseError(
        "limactl not found. Build the macOS target first so that the "
        "containers crate downloads Lima, or check ~/.peppy/tmp/lima-*/bin/limactl"
    )


def _lima_env() -> dict[str, str]:
    """Return environment dict with LIMA_HOME set."""
    return {**os.environ, "LIMA_HOME": str(LIMA_HOME)}


def _run_limactl(
    limactl: Path,
    args: list[str],
    *,
    capture: bool = True,
) -> subprocess.CompletedProcess[str]:
    """Run a limactl command with LIMA_HOME set."""
    return subprocess.run(
        [str(limactl), *args],
        env=_lima_env(),
        capture_output=capture,
        text=True,
    )


def _lima_shell(
    limactl: Path,
    script: str,
) -> subprocess.CompletedProcess[str]:
    """Run a bash script inside the Lima VM."""
    return subprocess.run(
        [str(limactl), "shell", LIMA_INSTANCE, "--", "bash", "-c", script],
        env=_lima_env(),
    )


def ensure_lima_vm(limactl: Path) -> None:
    """Ensure the peppy Lima VM instance is running.

    The VM should already exist (created by the containers crate build.rs
    during the native macOS build). This function starts it if stopped.
    """
    result = _run_limactl(
        limactl,
        ["list", "--format", "{{.Status}}", LIMA_INSTANCE],
    )
    status = result.stdout.strip()

    if not status:
        raise ReleaseError(
            f"Lima VM instance '{LIMA_INSTANCE}' not found. "
            "Build the macOS target first (cargo build triggers VM creation)."
        )

    if status == "Running":
        return

    if status == "Stopped":
        console.print(f"Starting Lima VM '{LIMA_INSTANCE}'...")
        start = _run_limactl(limactl, ["start", LIMA_INSTANCE], capture=False)
        if start.returncode != 0:
            raise ReleaseError(
                f"failed to start Lima VM '{LIMA_INSTANCE}' (exit {start.returncode})"
            )
        return

    raise ReleaseError(
        f"Lima VM '{LIMA_INSTANCE}' is in unexpected state: {status}"
    )


def ensure_rust_in_vm(limactl: Path) -> None:
    """Install Rust and cross-compilation tools inside the Lima VM if missing."""
    check = _run_limactl(
        limactl,
        [
            "shell",
            LIMA_INSTANCE,
            "--",
            "test",
            "-x",
            f"{GUEST_CARGO_HOME}/bin/rustc",
        ],
    )
    if check.returncode == 0:
        console.print("Rust toolchain already installed in Lima VM.")
        return

    console.print("Installing Rust toolchain in Lima VM...")
    install_script = f"""\
set -eu
sudo mkdir -p {GUEST_RUST_DIR}
sudo chown "$(id -u):$(id -g)" {GUEST_RUST_DIR}
export RUSTUP_HOME={GUEST_RUSTUP_HOME}
export CARGO_HOME={GUEST_CARGO_HOME}
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
    | sh -s -- -y --no-modify-path
export PATH="{GUEST_CARGO_HOME}/bin:$PATH"
rustup target add x86_64-unknown-linux-gnu
sudo apt-get update -qq
sudo apt-get install -y -qq gcc-x86-64-linux-gnu > /dev/null 2>&1
"""
    result = _lima_shell(limactl, install_script)
    if result.returncode != 0:
        raise ReleaseError(
            f"failed to install Rust in Lima VM (exit {result.returncode})"
        )
    console.print("Rust toolchain installed in Lima VM.")


def cargo_build_in_lima(
    limactl: Path,
    tag: str,
    target_triple: str,
    repo_root: Path,
) -> None:
    """Run cargo build inside the Lima VM for a Linux target."""
    cross_linker = ""
    if "x86_64" in target_triple:
        cross_linker = (
            "export CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER=x86_64-linux-gnu-gcc"
        )

    console.print(
        f"Building peppy for [bold]{target_triple}[/bold] in Lima VM..."
    )
    build_script = f"""\
set -eu
export RUSTUP_HOME={GUEST_RUSTUP_HOME}
export CARGO_HOME={GUEST_CARGO_HOME}
export PATH="{GUEST_CARGO_HOME}/bin:$PATH"
export PEPPY_GIT_TAG={tag}
{cross_linker}
cd {repo_root}
cargo build -p peppy --release --locked --target {target_triple}
"""
    result = _lima_shell(limactl, build_script)
    if result.returncode != 0:
        raise ReleaseError(
            f"cargo build for {target_triple} failed in Lima VM "
            f"(exit {result.returncode})"
        )
