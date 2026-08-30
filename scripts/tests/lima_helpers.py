"""Shared Lima VM helpers for install and container build tests."""

from __future__ import annotations

import logging
import os
import platform
import shutil
import subprocess
import sys
import time
from dataclasses import dataclass
from pathlib import Path
from typing import TYPE_CHECKING

if TYPE_CHECKING:
    from collections.abc import Generator

import pytest

REPO_ROOT = Path(__file__).resolve().parent.parent.parent
SCRIPTS_ROOT = REPO_ROOT / "scripts"
INSTALL_SCRIPT = SCRIPTS_ROOT / "install.sh"

logger = logging.getLogger(__name__)

_SSH_MAX_RETRIES = 3
_SSH_RETRY_DELAY = 5  # seconds


# ---------------------------------------------------------------------------
# VMConfig
# ---------------------------------------------------------------------------


def host_arch() -> str:
    """Return the normalised host architecture."""
    machine = platform.machine()
    if machine in ("aarch64", "arm64"):
        return "aarch64"
    if machine in ("x86_64", "AMD64"):
        return "x86_64"
    raise RuntimeError(f"Unsupported host architecture: {machine}")


def get_native_linux_targets() -> list[str]:
    """Return only the native-arch Linux target triple."""
    return [f"{host_arch()}-unknown-linux-gnu"]


@dataclass(frozen=True)
class VMConfig:
    """Describes a Lima VM target for install testing."""

    os: str  # "linux" or "darwin"
    arch: str  # "aarch64" or "x86_64"
    distro: str | None  # "ubuntu", "fedora", "archlinux", or None (macOS)
    instance_prefix: str = "pti"  # prefix for instance names
    template_override: str | None = None  # override the default template

    @property
    def target_triple(self) -> str:
        if self.os == "darwin":
            return "aarch64-apple-darwin"
        return f"{self.arch}-unknown-linux-gnu"

    @property
    def instance_name(self) -> str:
        # Keep short to stay under macOS UNIX_PATH_MAX=104 for ssh.sock paths.
        arch_short = "a64" if self.arch == "aarch64" else "x64"
        if self.distro:
            distro_map = {"ubuntu": "ubu", "fedora": "fed", "archlinux": "arch"}
            return f"{self.instance_prefix}-{arch_short}-{distro_map[self.distro]}"
        return f"{self.instance_prefix}-{arch_short}-{self.os[:3]}"

    @property
    def is_cross_arch(self) -> bool:
        """True if guest arch differs from host arch."""
        return self.arch != host_arch()

    @property
    def template(self) -> str:
        if self.template_override:
            return self.template_override
        if self.os == "darwin":
            return "template:macos"
        templates = {
            "ubuntu": "template:ubuntu-24.04",
            "fedora": "template:fedora",
            "archlinux": "template:archlinux",
        }
        return templates[self.distro]

    @property
    def lima_arch_flag(self) -> list[str]:
        """Return --arch flag if cross-arch, else empty."""
        if self.is_cross_arch:
            return [f"--arch={self.arch}"]
        return []

    def pytest_id(self) -> str:
        if self.distro:
            return f"{self.os}-{self.arch}-{self.distro}"
        return f"{self.os}-{self.arch}"


# ---------------------------------------------------------------------------
# Lima shell helpers
# ---------------------------------------------------------------------------


def _stream_text(stream: str | bytes | None) -> str:
    """Render one captured stream.

    Text mode hands back ``str``, but a run killed by its timeout raises with
    the raw bytes that had arrived so far, and either stream can be ``None``.
    """
    if stream is None:
        return ""
    return stream.decode(errors="replace") if isinstance(stream, bytes) else stream


def diagnostic(
    result: subprocess.CompletedProcess[str] | subprocess.TimeoutExpired,
) -> str:
    """Format stdout/stderr from a finished or timed-out process.

    TimeoutExpired carries the same two streams, and a command killed by its
    timeout is exactly when their partial contents matter most.
    """
    return (
        f"\n--- stdout ---\n{_stream_text(result.stdout)}"
        f"\n--- stderr ---\n{_stream_text(result.stderr)}"
    )


def lima_env() -> dict[str, str]:
    """Environment with LIMA_HOME pointing to a test-specific directory.

    When running under pytest-xdist, each worker gets its own LIMA_HOME
    to avoid conflicts between parallel VM operations. The optional
    PEPPY_TEST_LIMA_HOME sets a shorter base directory for environments whose
    home path would make Lima's Unix socket path exceed UNIX_PATH_MAX.
    """
    worker = os.environ.get("PYTEST_XDIST_WORKER", "gw0")
    base_dir = Path(
        os.environ.get("PEPPY_TEST_LIMA_HOME", Path.home() / ".peppy")
    )
    lima_home = base_dir / f"lti-{worker}"
    lima_home.mkdir(parents=True, exist_ok=True)
    return {**os.environ, "LIMA_HOME": str(lima_home)}


def is_ssh_connection_error(result: subprocess.CompletedProcess[str]) -> bool:
    """Return True if the process failed due to a transient SSH/SCP error."""
    if result.returncode == 0:
        return False
    stderr = (result.stderr or "").lower()
    return result.returncode == 255 or "connection closed" in stderr


def lima_shell(
    script: str, *, instance: str, timeout: int = 120, retries: int = _SSH_MAX_RETRIES
) -> subprocess.CompletedProcess[str]:
    """Run a bash script inside the test Lima VM for the given instance.

    Retries on transient SSH connection errors (common with cross-arch QEMU VMs).
    """
    cmd = [
        "limactl",
        "shell",
        "--workdir=/var/tmp",
        instance,
        "--",
        "bash",
        "-c",
        script,
    ]
    env = lima_env()
    for attempt in range(1, retries + 1):
        result = subprocess.run(
            cmd, env=env, capture_output=True, text=True, timeout=timeout
        )
        if not is_ssh_connection_error(result) or attempt == retries:
            return result
        logger.warning(
            "SSH connection error (attempt %d/%d) for %s, retrying in %ds...",
            attempt,
            retries,
            instance,
            _SSH_RETRY_DELAY,
        )
        time.sleep(_SSH_RETRY_DELAY)
    return result  # unreachable, but satisfies type checkers


def copy_to_lima(
    local_path: Path, guest_path: str, *, instance: str, retries: int = _SSH_MAX_RETRIES
) -> None:
    """Copy a file from the host to the guest VM.

    Pins ``--backend=scp`` rather than letting ``limactl copy`` pick its
    ``auto`` backend, which prefers rsync whenever the guest has it (Lima's
    own boot provisioning installs rsync into every guest). The rsync path
    runs a guest-side ``rsync --server``, so the host and guest rsync
    versions must interoperate: the Ubuntu runners ship rsync 3.2.7 while
    Arch guests roll with the latest release, and rsync 3.5.0 (Arch,
    2026-08-13) made the guest-side server die mid-handshake against the
    old client, failing every copy with rsync exit status 12. scp needs
    only the guest sshd, which the tests already depend on, so the copies
    stay independent of guest package versions.

    Retries on transient SSH connection errors (common with cross-arch QEMU VMs).
    """
    cmd = [
        "limactl",
        "copy",
        "--backend=scp",
        str(local_path),
        f"{instance}:{guest_path}",
    ]
    env = lima_env()
    for attempt in range(1, retries + 1):
        result = subprocess.run(
            cmd, env=env, capture_output=True, text=True, timeout=30
        )
        if result.returncode == 0:
            return
        if not is_ssh_connection_error(result) or attempt == retries:
            # Surface stderr: rsync/scp relay the guest-side error here, which
            # is the only clue when a copy dies inside the guest (swallowing
            # it turned the Arch rsync 3.5.0 failure into a bare exit 12).
            pytest.fail(
                f"limactl copy to {instance}:{guest_path} failed "
                f"(exit {result.returncode}){diagnostic(result)}"
            )
        logger.warning(
            "SCP connection error (attempt %d/%d) for %s, retrying in %ds...",
            attempt,
            retries,
            instance,
            _SSH_RETRY_DELAY,
        )
        time.sleep(_SSH_RETRY_DELAY)


# ---------------------------------------------------------------------------
# Guest path helpers
# ---------------------------------------------------------------------------


def guest_home(test_name: str) -> str:
    """Return a unique PEPPY_HOME path on the guest for the given test."""
    return f"/var/tmp/peppy-test-home/{test_name}"


def find_release_archive(config: VMConfig) -> Path:
    """Find the pre-built release archive for the given VMConfig.

    Looks in PEPPY_TEST_ARCHIVE_DIR (env var), then falls back to
    REPO_ROOT/dist/.  Raises pytest.fail() if the archive is not found.
    """
    triple = config.target_triple
    archive_name = f"peppy-{triple}.tgz"

    search_dirs: list[Path] = []
    env_dir = os.environ.get("PEPPY_TEST_ARCHIVE_DIR")
    if env_dir:
        search_dirs.append(Path(env_dir))
    search_dirs.append(REPO_ROOT / "dist")

    for d in search_dirs:
        path = d / archive_name
        if path.is_file():
            return path

    pytest.fail(
        f"Release archive {archive_name} not found "
        f"(searched: {', '.join(str(d) for d in search_dirs)})"
    )


def archive_guest_path(config: VMConfig) -> str:
    """Return the guest-side path for the release archive."""
    return f"/var/tmp/peppy-test/peppy-{config.target_triple}.tgz"


def install_cmd(
    config: VMConfig,
    home: str,
    *,
    extra_env: str = "",
    skip_service_install: bool = True,
) -> str:
    """Build the install.sh invocation command for the guest."""
    env_parts = f"TMPDIR=/var/tmp PEPPY_HOME={home}"
    if skip_service_install:
        env_parts = f"{env_parts} PEPPY_NO_SERVICE_INSTALL=1"
    if extra_env:
        env_parts = f"{env_parts} {extra_env}"
    return f"{env_parts} sh /var/tmp/peppy-test/install.sh {archive_guest_path(config)}"


def setup_lima_guest(config: VMConfig, *, test_name: str) -> str:
    """Copy install.sh and the release archive into the Lima guest.

    Returns the guest PEPPY_HOME path for this test.
    """
    archive_path = find_release_archive(config)
    home = guest_home(test_name)
    instance = config.instance_name

    # Stop and remove any peppy daemon left over from a previous test.
    # An earlier service install would otherwise race with install.sh's
    # running-daemon detection and block the non-interactive install.
    # Use /var/tmp (disk-backed) instead of /tmp to avoid tmpfs size
    # limits on Fedora and Arch Linux where /tmp is RAM-backed.
    lima_shell(
        """
        # Only touch systemd when a peppy unit is actually present. Running
        # `systemctl --user` on a pristine VM would spawn a user DBus session
        # as a side effect, which can race with install.sh's later DBus setup.
        if [ -f "$HOME/.config/systemd/user/peppy.service" ] \\
           && command -v systemctl >/dev/null 2>&1; then
            systemctl --user stop peppy.service 2>/dev/null || true
            systemctl --user disable peppy.service 2>/dev/null || true
            rm -f "$HOME/.config/systemd/user/peppy.service"
            systemctl --user daemon-reload 2>/dev/null || true
        fi
        # Use `-x` (exact comm-field match) so the pattern never matches the
        # bash process running this script (whose cmdline contains 'peppy').
        pkill -x peppy 2>/dev/null || true
        pkill -x zenohd 2>/dev/null || true
        for _ in 1 2 3 4 5; do
            pgrep -x peppy >/dev/null 2>&1 || break
            sleep 1
        done
        rm -rf /var/tmp/peppy-test-home
        mkdir -p /var/tmp/peppy-test
        """,
        instance=instance,
    )
    copy_to_lima(INSTALL_SCRIPT, "/var/tmp/peppy-test/install.sh", instance=instance)
    copy_to_lima(archive_path, archive_guest_path(config), instance=instance)
    return home


# ---------------------------------------------------------------------------
# Lima VM lifecycle
# ---------------------------------------------------------------------------


# Run once per Ubuntu guest before the install tests. Debian/Ubuntu cloud
# images ship without dbus-user-session and without linger, so they have no
# user D-Bus session bus. We install the package, enable linger, then wait for
# the user bus to come up so install.sh's checks see the baseline a configured
# Ubuntu host would have. Fedora and Arch already ship a working user session
# bus, so they need no provisioning.
#
# Nothing here may run before the guest has finished booting. `limactl start`
# returns as soon as ssh answers, which is well before PID 1 has drained its
# boot transaction. Both waits are bounded and advisory.
#
# Linger is established through its on-disk form rather than by asking logind
# for it. `loginctl enable-linger` is served by logind over the bus, and these
# guests can boot into a logind that never answers it: eight 25s calls in a
# row came back "Could not enable linger: Connection timed out" while
# `systemctl list-jobs` sat empty, twice on the same commit, so neither
# settling nor retrying reaches such an instance. Writing the flag file,
# restarting logind so it enumerates the linger directory afresh, and starting
# the user manager through PID 1 need nothing from the wedged instance; the
# one `loginctl enable-linger` at the end verifies that the restarted logind
# serves the API install.sh's checks rely on.
_UBUNTU_PROVISION_SCRIPT = """
set -eu
export DEBIAN_FRONTEND=noninteractive

if command -v cloud-init >/dev/null 2>&1; then
    sudo timeout 120 cloud-init status --wait >/dev/null 2>&1 || true
fi
timeout 120 systemctl is-system-running --wait >/dev/null 2>&1 || true

# Whatever logind is stuck behind shows up in one of these: a boot job that
# never completed, the unit's own state, or its log. Without them the next
# occurrence is as opaque as the one that prompted this.
diagnose() {
    echo "provision: $1" >&2
    echo "--- systemctl list-jobs ---" >&2
    systemctl list-jobs >&2 || true
    echo "--- systemctl status systemd-logind ---" >&2
    sudo systemctl status systemd-logind --no-pager >&2 || true
    echo "--- journalctl -b -u systemd-logind ---" >&2
    sudo journalctl -b -u systemd-logind --no-pager -n 50 >&2 || true
    exit 1
}

# Enable linger before the dbus install can restart the system bus. The flag
# file goes in first so the restarted logind enumerates the user as lingering
# the moment it comes up. The 120s timeouts leave systemd room to SIGKILL a
# logind that ignores its stop signal (90s) before they fire.
user="$(id -un)"
sudo mkdir -p /var/lib/systemd/linger
sudo touch "/var/lib/systemd/linger/${user}"
sudo timeout 120 systemctl restart systemd-logind \\
    || diagnose "could not restart systemd-logind"
sudo timeout 120 systemctl start "user@$(id -u).service" \\
    || diagnose "could not start the user manager"

# install.sh's check_linger_enabled asks logind, not the linger directory, so
# a logind that cannot answer has to fail provisioning here, with its state on
# record, not twenty tests later.
sudo timeout 30 loginctl enable-linger "${user}" \\
    || diagnose "logind does not serve enable-linger after a restart"

sudo apt-get update -qq
sudo apt-get install -y -qq dbus-user-session

# Best-effort wait for the user session bus to come up so install.sh's no-root
# D-Bus check sees it in later sessions.
for _ in 1 2 3 4 5 6 7 8 9 10; do
    if busctl --user status >/dev/null 2>&1; then
        break
    fi
    sleep 1
done
"""


# Worst case for the script above: both boot waits run out (120s each), the
# logind restart and the user manager start each spend their full 120s, the
# verification call its 30s, and apt still has to fetch a package. The budget
# has to cover that, or a slow guest trades the script's own diagnostics for
# a bare TimeoutExpired traceback.
_PROVISION_TIMEOUT = 900


def provision_guest(config: VMConfig, env: dict[str, str]) -> None:
    """Bring the guest to the baseline a properly-configured host would have.

    On Ubuntu this installs the user D-Bus session bus and enables linger,
    which install.sh's no-root mode requires and which a real Ubuntu user
    running the peppy service would already have set up. Other distros are
    left untouched (their cloud images already provide a user session bus).
    """
    if config.distro != "ubuntu":
        return

    cmd = [
        "limactl",
        "shell",
        config.instance_name,
        "--",
        "bash",
        "-c",
        _UBUNTU_PROVISION_SCRIPT,
    ]
    try:
        result = subprocess.run(
            cmd,
            env=env,
            capture_output=True,
            text=True,
            timeout=_PROVISION_TIMEOUT,
        )
    except subprocess.TimeoutExpired as expired:
        pytest.fail(
            f"Ubuntu guest provisioning for {config.pytest_id()} did not finish "
            f"within {_PROVISION_TIMEOUT}s{diagnostic(expired)}"
        )
    if result.returncode != 0:
        pytest.fail(
            f"Ubuntu guest provisioning failed for {config.pytest_id()}"
            f"{diagnostic(result)}"
        )


def _keep_failed_vm() -> bool:
    """Whether a guest that failed setup is left running instead of deleted.

    A wedged guest is the evidence for why it wedged, and deleting it throws
    that away. CI wants the disk and the process gone; someone reproducing the
    failure by hand wants to open a shell in it.
    """
    return os.environ.get("PEPPY_TEST_KEEP_VM", "") not in ("", "0")


def _teardown_lima_vm(instance: str, env: dict[str, str]) -> None:
    """Stop and delete a test VM, whatever state it is in.

    A guest whose systemd/D-Bus is wedged can ignore the ACPI power button, so
    `limactl stop` hangs. Escalate to a forced stop, then always force-delete.
    Never raises: one bad VM must not error out the next module's setup.
    """
    try:
        subprocess.run(
            ["limactl", "stop", instance],
            env=env,
            capture_output=True,
            timeout=60,
        )
    except subprocess.TimeoutExpired:
        try:
            subprocess.run(
                ["limactl", "stop", "--force", instance],
                env=env,
                capture_output=True,
                timeout=60,
            )
        except subprocess.TimeoutExpired:
            pass

    try:
        subprocess.run(
            ["limactl", "delete", "--force", instance],
            env=env,
            capture_output=True,
            timeout=120,
        )
    except subprocess.TimeoutExpired:
        pass


def lima_vm_lifecycle(request: pytest.FixtureRequest) -> Generator[VMConfig, None, None]:
    """Shared VM lifecycle: create, provision, yield, teardown."""
    config: VMConfig = request.param
    instance = config.instance_name
    template = config.template

    if sys.platform == "linux" and shutil.which("qemu-img") is None:
        pytest.fail(
            "QEMU is required to run Lima VMs on Linux. "
            "Install it via: sudo apt install qemu-utils qemu-system"
        )

    if (
        sys.platform == "linux"
        and os.path.exists("/dev/kvm")
        and not os.access("/dev/kvm", os.R_OK | os.W_OK)
    ):
        pytest.fail(
            "KVM is not accessible (permission denied on /dev/kvm). "
            "Add your user to the kvm group: sudo usermod -aG kvm $(whoami) && newgrp kvm"
        )

    env = lima_env()

    result = subprocess.run(
        ["limactl", "list", "--format", "{{.Status}}", instance],
        env=env,
        capture_output=True,
        text=True,
    )
    status = result.stdout.strip()

    # macOS guests download a ~14GB IPSW; cross-arch VMs use slow QEMU emulation.
    if config.os == "darwin":
        vm_start_timeout = 3600
    elif config.is_cross_arch:
        vm_start_timeout = 1800
    else:
        vm_start_timeout = 900
    vm_label = config.pytest_id()

    def _start_lima_vm(extra_args: list[str] | None = None) -> None:
        cmd = [
            "limactl",
            "start",
            *(extra_args or []),
            *config.lima_arch_flag,
            f"--name={instance}",
            "--tty=false",
            "--mount-writable",
            "--containerd=none",
            "--disk=20",
            template,
        ]
        try:
            start_result = subprocess.run(
                cmd, env=env, capture_output=True, text=True, timeout=vm_start_timeout
            )
        except subprocess.TimeoutExpired:
            # The half-started guest is the caller's to clean up: every path
            # out of setup goes through the one teardown around the call site.
            pytest.fail(
                f"Lima VM start for {vm_label} timed out after {vm_start_timeout}s "
                f"(cloud image download may be slow)"
            )
        if start_result.returncode != 0:
            pytest.fail(
                f"limactl start failed for {vm_label} "
                f"(exit {start_result.returncode}):\n{start_result.stderr}"
            )

    # Setup owns the guest until the yield hands it to the tests: a fixture
    # that raises before yielding never reaches the code after it, so a failure
    # here has to tear its own guest down. The run that prompted this left a
    # booted VM and its qemu alive for the rest of the job, and only the CI
    # runner's orphan-process sweep reaped them.
    try:
        if not status:
            _start_lima_vm()
        elif status == "Stopped":
            try:
                restart = subprocess.run(
                    ["limactl", "start", instance],
                    env=env,
                    capture_output=True,
                    timeout=vm_start_timeout,
                )
            except subprocess.TimeoutExpired:
                pytest.fail(
                    f"Lima VM restart for {vm_label} timed out after "
                    f"{vm_start_timeout}s"
                )
            if restart.returncode != 0:
                subprocess.run(
                    ["limactl", "delete", "--force", instance],
                    env=env,
                    capture_output=True,
                    timeout=60,
                )
                _start_lima_vm()

        provision_guest(config, env)
    except BaseException:  # pytest.fail raises BaseException, not Exception
        if _keep_failed_vm():
            logger.warning(
                "PEPPY_TEST_KEEP_VM set: leaving %s up after a failed setup",
                instance,
            )
        else:
            _teardown_lima_vm(instance, env)
        raise

    yield config

    _teardown_lima_vm(instance, env)


# ---------------------------------------------------------------------------
# Build fixture helper
# ---------------------------------------------------------------------------


def build_release_archives(targets: list[str] | None = None) -> None:
    """Build release archives from the current source tree.

    When *targets* is ``None`` (the default), all targets for the current
    platform are built.  Pass an explicit list to restrict which triples
    are built — this avoids spawning cross-arch QEMU VMs when only
    native-arch archives are needed.

    Runs once per test session before any install test executes.  The
    archives land in REPO_ROOT/dist/ where ``find_release_archive``
    picks them up.

    Cleans the containers crate build cache first to ensure build.rs
    changes (e.g. architecture detection fixes) take effect.
    """
    from functions.build_release import _build_all_targets
    from functions.cli import get_targets_for_platform

    # Force rebuild of the containers crate so build.rs re-runs with the
    # latest arch detection logic.  Without this, stale build artifacts
    # may bundle the wrong architecture binaries.
    subprocess.run(
        ["cargo", "clean", "-p", "containers", "--release"],
        cwd=REPO_ROOT,
        capture_output=True,
    )

    # Clear apptainer source caches to force a full source build, ensuring
    # that build.rs source-build logic (e.g. VERSION file creation) is
    # always exercised rather than masked by stale cached binaries.
    peppy_tmp = Path.home() / ".peppy" / "tmp"
    if peppy_tmp.exists():
        for entry in peppy_tmp.iterdir():
            if entry.name.startswith("apptainer-") and entry.is_dir():
                shutil.rmtree(entry, ignore_errors=True)

    tag = "test"
    if targets is None:
        targets = get_targets_for_platform()
    _build_all_targets(tag, targets, REPO_ROOT)
