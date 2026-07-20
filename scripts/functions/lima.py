"""Lima VM management for cross-compiling Linux targets from macOS."""

from __future__ import annotations

import os
import subprocess
from pathlib import Path

from .build import _get_target_dir
from .cli import ReleaseError, console

LIMA_HOME = Path.home() / ".peppy" / "lima-build"
LIMA_INSTANCE = "peppy"
LIMA_TEMPLATE = "template:ubuntu-24.04"
GUEST_RUST_DIR = "/opt/peppy-rust"
GUEST_RUSTUP_HOME = f"{GUEST_RUST_DIR}/rustup"
GUEST_CARGO_HOME = f"{GUEST_RUST_DIR}/cargo"

# The Lima release the containers crate downloads and SHA-verifies. This MUST
# stay in sync with LIMA_VERSION in crates/containers-internal/build.rs (which
# also keys `lima_archive_sha256` on it); a limactl from one release driving a
# guest agent from another is a known way to land an instance in DEGRADED
# state. Nothing enforces the sync, so it is prose across the Rust/Python
# boundary, exactly like GO_VERSION below.
LIMA_VERSION = "2.1.3"

# Pinned Go toolchain for the in-VM Linux target builds. The containers crate
# builds apptainer from source, whose `mconfig` needs a Go newer than Ubuntu's
# `golang-go`; we install the official toolchain (SHA-verified) and pin it with
# `GOTOOLCHAIN=local` so the build never silently downloads a toolchain. These
# values MUST stay in sync with the GO_VERSION / GO_LINUX_*_SHA256 constants in
# crates/containers-internal/build.rs; this duplication crosses the Rust/Python
# boundary the same way RELEASE_PLATFORM_SO does.
GUEST_GO_DIR = "/usr/local/go"
GO_VERSION = "1.25.7"
GO_LINUX_AMD64_SHA256 = (
    "12e6d6a191091ae27dc31f6efc630e3a3b8ba409baf3573d955b196fdf086005"
)
GO_LINUX_ARM64_SHA256 = (
    "ba611a53534135a81067240eff9508cd7e256c560edd5d8c2fef54f083c07129"
)

# Every release platform's binding plus the build-state marker. The host build
# cross-compiles all of them into the peppy-owned .so dir; the in-VM build reads
# that dir (via PEPPYLIB_PREBUILT_SO_DIR) and embeds all of them, selecting one at
# deploy time. We verify the full set is present before starting the VM build so a
# missing host artifact fails here rather than deep inside cargo. Source of truth
# for the suffixes: crates/generator-internal/src/generator/python/scaffold.rs
# (the embedded platform .so set); mirrored here because it crosses the
# Rust/Python boundary.
RELEASE_PLATFORM_SO = (
    "_peppylib.abi3.linux-aarch64.so",
    "_peppylib.abi3.linux-x86_64.so",
    "_peppylib.abi3.macos-aarch64.so",
)
SO_BUILD_STATE_MARKER = ".so-build-state"


def find_limactl(repo_root: Path) -> Path:
    """Find the limactl binary for the pinned Lima release.

    Checks the exact SHA-verified cache entry for LIMA_VERSION first, then falls
    back to the cargo build output created by the containers crate build.rs.
    There is deliberately no `lima-*` glob: sorting one picked the
    lexicographically last directory, so an unrelated cache entry such as
    `lima-99.0.0-Darwin-arm64` would win over the verified download.
    """
    cached = (
        Path.home()
        / ".peppy"
        / "tmp"
        / f"lima-{LIMA_VERSION}-Darwin-arm64"
        / "bin"
        / "limactl"
    )
    if cached.is_file():
        return cached

    build_dir = (
        _get_target_dir(repo_root) / "aarch64-apple-darwin" / "release" / "build"
    )
    matches = sorted(build_dir.glob("containers-*/out/lima-install/bin/limactl"))
    if matches and matches[0].is_file():
        return matches[0]

    raise ReleaseError(
        f"limactl not found for the pinned Lima {LIMA_VERSION}. Expected "
        f"{cached}, or build the macOS target first so the containers crate "
        "downloads and verifies it."
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
    """Ensure the peppy Lima VM instance exists and is running.

    Creates the VM if it does not exist, starts it if stopped.
    """
    result = _run_limactl(
        limactl,
        ["list", "--format", "{{.Status}}", LIMA_INSTANCE],
    )
    status = result.stdout.strip()

    if not status:
        console.print(
            f"Creating Lima VM '{LIMA_INSTANCE}' with {LIMA_TEMPLATE} "
            "(this may take a few minutes on first run)..."
        )
        create = _run_limactl(
            limactl,
            [
                "start",
                f"--name={LIMA_INSTANCE}",
                "--tty=false",
                "--mount-writable",
                "--containerd=none",
                "--memory=12",
                LIMA_TEMPLATE,
            ],
            capture=False,
        )
        if create.returncode != 0:
            raise ReleaseError(
                f"failed to create Lima VM '{LIMA_INSTANCE}' "
                f"(exit {create.returncode})"
            )
        return

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


def stop_lima_vm(limactl: Path) -> None:
    """Stop the peppy Lima VM so it stops consuming host RAM after the build.

    Best-effort: a stop failure (the VM was never created, or is already stopped)
    is warned, not raised, so cleanup never masks the build's own outcome. The
    instance is stopped, not deleted, so its disk and provisioning survive for a
    fast restart on the next build.
    """
    console.print(f"Stopping Lima VM '{LIMA_INSTANCE}' to free memory...")
    result = _run_limactl(limactl, ["stop", LIMA_INSTANCE])
    if result.returncode != 0:
        console.print(
            f"[yellow]Warning: failed to stop Lima VM '{LIMA_INSTANCE}' "
            f"(exit {result.returncode}); it may still be running.[/yellow]"
        )


def _ensure_pinned_go_in_vm(limactl: Path) -> None:
    """Install the pinned Go toolchain in the Lima VM if missing or wrong version.

    Idempotent: skips the download when the correct version is already present.
    Kept separate from the Rust setup so it also runs on VMs that already have
    Rust but predate Go pinning.
    """
    install_script = f"""\
set -eu
go_arch=$(dpkg --print-architecture)
case "$go_arch" in
  arm64) go_sha={GO_LINUX_ARM64_SHA256} ;;
  amd64) go_sha={GO_LINUX_AMD64_SHA256} ;;
  *) echo "unsupported architecture for pinned Go toolchain: $go_arch" >&2; exit 1 ;;
esac
if [ -x {GUEST_GO_DIR}/bin/go ] && {GUEST_GO_DIR}/bin/go version | grep -q "go{GO_VERSION} "; then
    exit 0
fi
curl -fsSL "https://go.dev/dl/go{GO_VERSION}.linux-$go_arch.tar.gz" -o /tmp/go-{GO_VERSION}.tar.gz
echo "$go_sha  /tmp/go-{GO_VERSION}.tar.gz" | sha256sum -c -
sudo rm -rf {GUEST_GO_DIR}
sudo tar -C /usr/local -xzf /tmp/go-{GO_VERSION}.tar.gz
rm -f /tmp/go-{GO_VERSION}.tar.gz
"""
    console.print(f"Ensuring pinned Go {GO_VERSION} in Lima VM...")
    result = _lima_shell(limactl, install_script)
    if result.returncode != 0:
        raise ReleaseError(
            f"failed to install pinned Go {GO_VERSION} in Lima VM "
            f"(exit {result.returncode})"
        )


def ensure_rust_in_vm(limactl: Path) -> None:
    """Install Rust and cross-compilation tools inside the Lima VM if missing."""
    _ensure_pinned_go_in_vm(limactl)
    check = _run_limactl(
        limactl,
        [
            "shell",
            LIMA_INSTANCE,
            "--",
            "bash",
            "-c",
            f"test -x {GUEST_CARGO_HOME}/bin/rustc && command -v cc >/dev/null 2>&1",
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
sudo apt-get install -y -qq build-essential unzip gcc-x86-64-linux-gnu \
    libseccomp-dev make pkg-config squashfs-tools cryptsetup > /dev/null 2>&1
"""
    result = _lima_shell(limactl, install_script)
    if result.returncode != 0:
        raise ReleaseError(
            f"failed to install Rust in Lima VM (exit {result.returncode})"
        )
    console.print("Rust toolchain installed in Lima VM.")


def _prebuilt_peppylib_so_dir() -> Path:
    """Host directory holding the peppylib native bindings the release embeds.

    Mirrors build-helpers' `cache_dir("peppylib-py")/so`, rooted at $HOME exactly
    as the Rust side resolves it (not the PEPPY_HOME override). The native
    aarch64-apple-darwin build populates it. Lima mounts the host home into the
    guest at the same path, so the in-VM build reads the bindings straight from
    here via PEPPYLIB_PREBUILT_SO_DIR, with no cargo-cache staging.
    """
    return Path.home() / ".peppy" / "tmp" / "peppylib-py" / "so"


def require_prebuilt_peppylib_so() -> Path:
    """Return the prebuilt .so dir, failing loudly if the host build has not run.

    The in-VM build cannot produce the bindings (the VM has no pixi), so every
    release platform's .so and the build-state marker must already exist. Checking
    here surfaces a missing host artifact before the VM build starts instead of as
    an opaque failure deep inside cargo.
    """
    so_dir = _prebuilt_peppylib_so_dir()
    missing = [
        name
        for name in (*RELEASE_PLATFORM_SO, SO_BUILD_STATE_MARKER)
        if not (so_dir / name).is_file()
    ]
    if missing:
        raise ReleaseError(
            f"prebuilt peppylib bindings missing from {so_dir}: "
            f"{', '.join(missing)}. Run the host build (aarch64-apple-darwin) "
            "before cross-building Linux targets."
        )
    return so_dir


def cargo_build_in_lima(
    limactl: Path,
    tag: str,
    target_triple: str,
    repo_root: Path,
    *,
    target_dir: Path | None = None,
) -> None:
    """Run cargo build inside the Lima VM for a Linux target.

    The VM has no pixi, so it cannot build the peppylib bindings. It reads the
    host-built .so directly from PEPPYLIB_PREBUILT_SO_DIR (the host home is mounted
    into the guest), so the host build must have run first.
    """
    so_dir = require_prebuilt_peppylib_so()

    cross_linker_lines: list[str] = []
    if "x86_64" in target_triple:
        cross_linker_lines.append(
            "export CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER=x86_64-linux-gnu-gcc"
        )
    cross_linker = "\n".join(cross_linker_lines)

    console.print(
        f"Building peppy for [bold]{target_triple}[/bold] in Lima VM..."
    )
    build_script = f"""\
set -eu
export RUSTUP_HOME={GUEST_RUSTUP_HOME}
export CARGO_HOME={GUEST_CARGO_HOME}
export PATH="{GUEST_GO_DIR}/bin:{GUEST_CARGO_HOME}/bin:$PATH"
export GOTOOLCHAIN=local
export PEPPY_GIT_TAG={tag}
export PEPPY_CROSS_ARCH=1
export RUSTC_WRAPPER=""
export PEPPYLIB_PREBUILT_SO_DIR={so_dir}
{f"export CARGO_TARGET_DIR={target_dir}" if target_dir is not None else ""}
{cross_linker}
cd {repo_root}
cargo build -p peppy --release --locked --target {target_triple} -j 8
"""
    result = _lima_shell(limactl, build_script)
    if result.returncode != 0:
        raise ReleaseError(
            f"cargo build for {target_triple} failed in Lima VM "
            f"(exit {result.returncode})"
        )
