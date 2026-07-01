"""Lima VM management for cross-compiling Linux targets from macOS."""

from __future__ import annotations

import os
import subprocess
from pathlib import Path

from .cli import ReleaseError, console

LIMA_HOME = Path.home() / ".peppy" / "lima-build"
LIMA_INSTANCE = "peppy"
LIMA_TEMPLATE = "template:ubuntu-24.04"
GUEST_RUST_DIR = "/opt/peppy-rust"
GUEST_RUSTUP_HOME = f"{GUEST_RUST_DIR}/rustup"
GUEST_CARGO_HOME = f"{GUEST_RUST_DIR}/cargo"

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

# The peppylib package dir inside the shared `nodes_shared_code` checkout, where
# the generator build script reads the platform-suffixed bindings it embeds.
PEPPYLIB_SO_RELDIR = "peppyos-shared/peppylib-py/peppylib"

# Every release platform's binding plus the build-state marker. The generator
# embeds all of them and selects one at deploy time, so the in-VM build needs
# the full set present. Source of truth for the suffixes:
# crates/generator-internal/src/generator/python/scaffold.rs (the embedded
# platform .so set); mirrored here because it crosses the Rust/Python boundary.
RELEASE_PLATFORM_SO = (
    "_peppylib.abi3.linux-aarch64.so",
    "_peppylib.abi3.linux-x86_64.so",
    "_peppylib.abi3.macos-aarch64.so",
)
SO_BUILD_STATE_MARKER = ".so-build-state"


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


def _locked_nodes_shared_rev(repo_root: Path) -> str:
    """Return the nodes_shared_code git revision pinned in Cargo.lock.

    The in-VM build resolves the shared crates to a cargo checkout named by this
    rev, so the staged .so files must come from the host checkout of the same
    rev for their recorded source hash to match what the in-VM build recomputes.
    """
    lock_path = repo_root / "Cargo.lock"
    try:
        contents = lock_path.read_text()
    except OSError as exc:
        raise ReleaseError(f"could not read {lock_path}: {exc}") from exc

    marker = "nodes_shared_code#"
    for line in contents.splitlines():
        index = line.find(marker)
        if index != -1:
            return line[index + len(marker):].strip().rstrip('"')

    raise ReleaseError(
        "no nodes_shared_code git revision found in Cargo.lock; cannot locate "
        "the host-built peppylib bindings to stage into the Lima VM"
    )


def stage_prebuilt_peppylib_so(limactl: Path, repo_root: Path) -> None:
    """Seed the VM's cargo checkout with the host-built peppylib .so files.

    `cargo build` inside the VM resolves nodes_shared_code into the VM's own
    CARGO_HOME, a pristine checkout with no compiled bindings. The generator
    build script cannot rebuild them there (the VM has no pixi), so it aborts on
    the missing .so. The host build (release iteration 1) already cross-compiled
    every release platform's .so into the host cargo checkout, which Lima mounts
    into the guest at the same path. Copy that full set (the generator embeds
    all platforms and picks one at deploy time) plus the .so-build-state marker,
    keyed to the Cargo.lock-pinned rev so the host and guest checkouts always
    hold identical sources and the in-VM staleness guard passes.

    Fails loudly if the host bindings are absent (the host build must run first)
    rather than silently producing a release with missing or stale bindings.
    """
    short_rev = _locked_nodes_shared_rev(repo_root)[:7]
    host_cargo_home = os.environ.get(
        "CARGO_HOME", str(Path.home() / ".cargo")
    )
    host_peppylib = (
        f"{host_cargo_home}/git/checkouts/nodes_shared_code-*/{short_rev}"
        f"/{PEPPYLIB_SO_RELDIR}"
    )
    vm_peppylib = (
        f"{GUEST_CARGO_HOME}/git/checkouts/nodes_shared_code-*/{short_rev}"
        f"/{PEPPYLIB_SO_RELDIR}"
    )
    required = " ".join((*RELEASE_PLATFORM_SO, SO_BUILD_STATE_MARKER))

    stage_script = f"""\
set -eu
export RUSTUP_HOME={GUEST_RUSTUP_HOME}
export CARGO_HOME={GUEST_CARGO_HOME}
export PATH="{GUEST_CARGO_HOME}/bin:$PATH"
export RUSTC_WRAPPER=""
cd {repo_root}
cargo fetch --locked
src=$(ls -d {host_peppylib} 2>/dev/null | head -1)
if [ -z "$src" ]; then
  echo "stage-peppylib: no host-built peppylib checkout for rev {short_rev} \
under {host_cargo_home} (run the host build first)" >&2
  exit 3
fi
for f in {required}; do
  if [ ! -f "$src/$f" ]; then
    echo "stage-peppylib: host checkout $src is missing $f" >&2
    exit 4
  fi
done
dst=$(ls -d {vm_peppylib} 2>/dev/null | head -1)
if [ -z "$dst" ]; then
  echo "stage-peppylib: VM cargo checkout for rev {short_rev} not found \
after 'cargo fetch'" >&2
  exit 5
fi
cp -f "$src"/_peppylib.abi3.*.so "$dst"/
cp -f "$src/{SO_BUILD_STATE_MARKER}" "$dst"/{SO_BUILD_STATE_MARKER}
echo "stage-peppylib: staged peppylib bindings from $src into $dst"
"""
    console.print("Staging host-built peppylib bindings into Lima VM...")
    result = _lima_shell(limactl, stage_script)
    if result.returncode != 0:
        raise ReleaseError(
            "failed to stage prebuilt peppylib .so into Lima VM "
            f"(exit {result.returncode})"
        )


def cargo_build_in_lima(
    limactl: Path,
    tag: str,
    target_triple: str,
    repo_root: Path,
) -> None:
    """Run cargo build inside the Lima VM for a Linux target."""
    stage_prebuilt_peppylib_so(limactl, repo_root)

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
