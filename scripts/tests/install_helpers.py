"""Shared helpers for the install.sh tests.

The tests install peppy on the machine they run on, the way a user does:
scripts/install.sh with the release archive of this machine's platform. The
host tests change that machine (apt packages, systemd linger, an AppArmor
profile, a user service, ~/.bashrc), so every install test runs only on a
machine that PEPPY_DISPOSABLE_TEST_HOST=1 declares disposable, meaning one
discarded after the run, such as a GitHub-hosted runner. Anywhere else they
skip (see conftest).

A user runs the installer from a login session: an SSH session on a robot or
a terminal on a desktop. On Linux that session is what gives the shell its
systemd user manager, XDG_RUNTIME_DIR and D-Bus user bus, which install.sh
probes. A CI job step is not a login session, so on Linux every command of
the tests goes through SSH to localhost. On macOS the runner already runs in
the logged-in user's GUI session, the one Terminal shells run in, so the
commands run there directly.

The tests that need a system of their own (another distribution, another
board) run install.sh in a Docker container instead, through
run_in_container.
"""

from __future__ import annotations

import functools
import os
import platform
import shlex
import subprocess
import sys
from collections.abc import Mapping
from pathlib import Path

import pytest

from functions.build import release_archive_name, release_dist_dir
from functions.cli import get_native_triple

REPO_ROOT = Path(__file__).resolve().parent.parent.parent
INSTALL_SCRIPT = REPO_ROOT / "scripts" / "install.sh"

DISPOSABLE_HOST_VARIABLE = "PEPPY_DISPOSABLE_TEST_HOST"

# Everything the tests write outside a user's own files. Short, because a
# PEPPY_HOME under it holds the daemon's Unix sockets and macOS caps a socket
# path at 104 bytes.
TEST_ROOT = Path("/var/tmp/peppy-install-test")
HOMES_ROOT = TEST_ROOT / "homes"
SSH_DIR = TEST_ROOT / "ssh"

# Every apt-get the tests run waits for the dpkg lock, as install.sh's own do:
# a freshly booted runner holds it for its own apt runs.
APT_GET = "apt-get -o DPkg::Lock::Timeout=300"

IS_LINUX = sys.platform == "linux"
IS_MACOS = sys.platform == "darwin"
IS_LINUX_AARCH64 = IS_LINUX and platform.machine() == "aarch64"

linux_only = pytest.mark.skipif(not IS_LINUX, reason="applies to Linux only")
linux_aarch64_only = pytest.mark.skipif(
    not IS_LINUX_AARCH64, reason="applies to Linux on aarch64 only"
)

# Where each container sees the two files it is given, and where it installs.
CONTAINER_INSTALL_SCRIPT = "/peppy-test/install.sh"
CONTAINER_PEPPY_HOME = "/peppy-home"

# Where the kernel shows the device tree of the board, the path install.sh
# reads to identify a Raspberry Pi.
DEVICE_TREE = "/sys/firmware/devicetree/base"


def is_disposable_host() -> bool:
    """True when the environment declares this machine disposable."""
    return os.environ.get(DISPOSABLE_HOST_VARIABLE) == "1"


def release_archive() -> Path:
    """The release archive of this machine's platform, which the tests install.

    CI builds it in a job of its own and hands it over as an artifact; locally
    `./scripts/build_release.sh --local --tag test` writes it.
    """
    archive = release_dist_dir(REPO_ROOT) / release_archive_name(get_native_triple())
    if not archive.is_file():
        pytest.fail(
            f"release archive {archive} not found; build it with "
            "`./scripts/build_release.sh --local --tag test`"
        )
    return archive


def peppy_home(name: str) -> Path:
    """The PEPPY_HOME of one test, under the directory the tests clear."""
    return HOMES_ROOT / name


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


def assert_installed(result: subprocess.CompletedProcess[str]) -> None:
    """Assert install.sh exited 0 and reported where it installed peppy."""
    assert result.returncode == 0, (
        f"install.sh exited with {result.returncode}{diagnostic(result)}"
    )
    assert "peppy installed to" in result.stdout, (
        f"Missing 'peppy installed to' in the output{diagnostic(result)}"
    )


def run_as_root(script: str, *, timeout: int = 600) -> None:
    """Run a shell script as root to set up what a test starts from.

    Setup, not the subject of a test: a failure fails the test with the
    script's output rather than letting it start from the wrong state.
    """
    result = subprocess.run(
        ["sudo", "sh", "-c", script],
        capture_output=True,
        text=True,
        timeout=timeout,
        stdin=subprocess.DEVNULL,
        check=False,
    )
    if result.returncode != 0:
        pytest.fail(f"setup as root failed:\n{script}{diagnostic(result)}")


# Run by root once per session. The runner image may lack sshd, and may lack
# the package that serves the D-Bus user bus. A user's Ubuntu machine has
# both, and the tests reach the machine through the first and probe the
# second, so both belong to the baseline rather than to any test. A user
# manager that started before dbus-user-session was installed does not serve
# the bus, hence the restart.
_LOGIN_BASELINE_SCRIPT = f"""
set -eu
if ! command -v sshd >/dev/null 2>&1; then
    {APT_GET} update -qq
    {APT_GET} install -y -qq openssh-server
fi
systemctl start ssh
if ! dpkg -s dbus-user-session >/dev/null 2>&1; then
    {APT_GET} update -qq
    {APT_GET} install -y -qq dbus-user-session
    systemctl try-restart "user@$SUDO_UID.service"
fi
"""

# Run by the user once per session: a key of the harness's own, authorised
# for this user, and the host key pinned in a known_hosts of the harness's own.
_SSH_KEY_SCRIPT = f"""
set -eu
mkdir -p {SSH_DIR}
key={SSH_DIR}/id_ed25519
[ -f "$key" ] || ssh-keygen -q -t ed25519 -N '' -f "$key"
mkdir -p "$HOME/.ssh"
chmod 700 "$HOME/.ssh"
touch "$HOME/.ssh/authorized_keys"
chmod 600 "$HOME/.ssh/authorized_keys"
grep -qxF "$(cat "$key.pub")" "$HOME/.ssh/authorized_keys" \\
    || cat "$key.pub" >> "$HOME/.ssh/authorized_keys"
ssh-keyscan -t ed25519 localhost > {SSH_DIR}/known_hosts 2>/dev/null
[ -s {SSH_DIR}/known_hosts ]
"""


@functools.cache
def _ssh_to_localhost() -> tuple[str, ...]:
    """The ssh command line that opens a login session of this user.

    Prepares sshd, the D-Bus user bus and a key on first use. Cached, so each
    session prepares them once.
    """
    run_as_root(_LOGIN_BASELINE_SCRIPT)
    keys = subprocess.run(
        ["bash", "-c", _SSH_KEY_SCRIPT],
        capture_output=True,
        text=True,
        timeout=60,
        stdin=subprocess.DEVNULL,
        check=False,
    )
    if keys.returncode != 0:
        pytest.fail(f"could not authorise the harness's ssh key{diagnostic(keys)}")
    return (
        "ssh",
        "-i",
        str(SSH_DIR / "id_ed25519"),
        "-o",
        "BatchMode=yes",
        "-o",
        "IdentitiesOnly=yes",
        "-o",
        f"UserKnownHostsFile={SSH_DIR / 'known_hosts'}",
        "-o",
        "StrictHostKeyChecking=yes",
        "localhost",
    )


def _login_shell_command(script: str) -> list[str]:
    """The command that runs *script* with bash in a login session of this user.

    On macOS the command starts a fresh login shell with only the variables a
    Terminal shell starts from, so nothing of the job's own environment (its
    tools on PATH, the workflow's variables) reaches install.sh. On Linux SSH
    gives the session a fresh environment of its own.
    """
    if IS_MACOS:
        fresh_environment = [
            f"{name}={os.environ[name]}"
            for name in ("HOME", "USER", "LOGNAME", "SHELL", "TMPDIR")
            if name in os.environ
        ]
        return ["/usr/bin/env", "-i", *fresh_environment, "/bin/bash", "-lc", script]
    return [*_ssh_to_localhost(), f"bash -c {shlex.quote(script)}"]


def login_shell(script: str, *, timeout: int = 120) -> subprocess.CompletedProcess[str]:
    """Run a bash script in a login session of this user, as a user's shell would."""
    return subprocess.run(
        _login_shell_command(script),
        capture_output=True,
        text=True,
        timeout=timeout,
        stdin=subprocess.DEVNULL,
        check=False,
    )


def install_cmd(
    home: Path,
    *,
    extra_env: str = "",
    skip_service_install: bool = True,
) -> str:
    """The install.sh invocation that installs this platform's archive into *home*."""
    env_parts = f"PEPPY_HOME={home}"
    if skip_service_install:
        env_parts = f"{env_parts} PEPPY_NO_SERVICE_INSTALL=1"
    if extra_env:
        env_parts = f"{env_parts} {extra_env}"
    return f"{env_parts} sh {INSTALL_SCRIPT} {release_archive()}"


# Returns the machine to no peppy install: the user service a test installed,
# any daemon a test started, and every test's PEPPY_HOME. The service goes
# through the installed binary's own `service` commands, which know its unit
# on either platform. A daemon a test started by hand gets SIGTERM, so it
# stops its own children, and the script waits for it to be gone however
# long that takes; a daemon that never exits runs the script into the
# caller's timeout, which reports it. `-x` matches the process name exactly,
# so the patterns never match the bash running this script.
_REMOVE_PEPPY_SCRIPT = f"""
for bin in {HOMES_ROOT}/*/bin/peppy; do
    [ -x "$bin" ] || continue
    home="$(dirname "$(dirname "$bin")")"
    PEPPY_HOME="$home" "$bin" service stop >/dev/null 2>&1 || true
    PEPPY_HOME="$home" "$bin" service uninstall >/dev/null 2>&1 || true
done
pkill -x peppy 2>/dev/null || true
pkill -x zenohd 2>/dev/null || true
while pgrep -x peppy >/dev/null 2>&1 || pgrep -x zenohd >/dev/null 2>&1; do
    sleep 1
done
rm -rf {HOMES_ROOT}
mkdir -p {HOMES_ROOT}
"""


def container_install_cmd() -> str:
    """install.sh as a container user runs it: no service, since no systemd runs."""
    archive = f"/peppy-test/{release_archive().name}"
    return (
        f"PEPPY_HOME={CONTAINER_PEPPY_HOME} PEPPY_NO_SERVICE_INSTALL=1 "
        f"sh {CONTAINER_INSTALL_SCRIPT} {archive}"
    )


def run_in_container(
    image: str,
    script: str,
    *,
    files: Mapping[str, Path] | None = None,
    device_tree: Path | None = None,
    timeout: int = 600,
) -> subprocess.CompletedProcess[str]:
    """Run *script* with sh in a fresh container of *image*.

    The container gets install.sh and the release archive of this machine's
    platform read-only, and nothing else of the host. *files* maps a path in
    the container to a host file that replaces it there, read-only: an
    /etc/os-release presents install.sh with a distribution the tests have no
    image of.

    *device_tree* is a host directory the container sees as the device tree
    of its board, at DEVICE_TREE. Docker hides /sys/firmware from a container
    and its AppArmor profile denies reading it, so such a container runs with
    the hidden paths shown, without that profile, and with a tmpfs of its own
    at /sys/firmware for the directory to be mounted into.
    """
    archive = release_archive()
    command = [
        "docker",
        "run",
        "--rm",
        "-v",
        f"{INSTALL_SCRIPT}:{CONTAINER_INSTALL_SCRIPT}:ro",
        "-v",
        f"{archive}:/peppy-test/{archive.name}:ro",
    ]
    for container_path, host_path in (files or {}).items():
        command += ["-v", f"{host_path}:{container_path}:ro"]
    if device_tree is not None:
        command += [
            "--security-opt",
            "systempaths=unconfined",
            "--security-opt",
            "apparmor=unconfined",
            "--tmpfs",
            "/sys/firmware",
            "-v",
            f"{device_tree}:{DEVICE_TREE}:ro",
        ]
    command += [image, "sh", "-c", script]
    return subprocess.run(
        command,
        capture_output=True,
        text=True,
        timeout=timeout,
        stdin=subprocess.DEVNULL,
        check=False,
    )


def host_file(path: Path, content: str, *, mode: int = 0o644) -> Path:
    """Write a file a container gets in place of one of its own (see
    run_in_container)."""
    path.write_text(content)
    path.chmod(mode)
    return path


def binaries_load_script() -> str:
    """install.sh, then ldd on every executable and shared library it
    installed, then the version of the three commands a user runs first.

    ldd reports each library or glibc version a binary needs and the system
    does not have, including for the binaries only a running container
    executes (apptainer's starter).
    """
    bin_dir = f"{CONTAINER_PEPPY_HOME}/bin"
    return (
        f"{container_install_cmd()} >/dev/null && "
        f"find {bin_dir} -type f \\( -perm -u+x -o -name '*.so*' \\) "
        "-exec ldd {} + 2>&1; "
        f"{bin_dir}/peppy --version && "
        f"{bin_dir}/zenohd --version && "
        f"{bin_dir}/apptainer/bin/apptainer --version"
    )


def assert_binaries_load(result: subprocess.CompletedProcess[str]) -> None:
    """Assert the binaries_load_script run found every library and glibc
    version, and that peppy, zenohd and apptainer started."""
    assert "not found" not in result.stdout, (
        f"a binary needs a library or glibc version that the system does not "
        f"have{diagnostic(result)}"
    )
    assert result.returncode == 0, (
        f"peppy, zenohd and apptainer should start{diagnostic(result)}"
    )
    assert "apptainer version" in result.stdout, (
        f"apptainer should report its version{diagnostic(result)}"
    )


def refusal_script(before: str = "") -> str:
    """install.sh after *before*, then a line the tests look for when it left a
    PEPPY_HOME behind, with install.sh's own exit status passed through."""
    return (
        f"{before}{container_install_cmd()}; status=$?; "
        f'[ ! -e {CONTAINER_PEPPY_HOME} ] || echo "PEPPY_HOME was created"; '
        "exit $status"
    )


def assert_refused(result: subprocess.CompletedProcess[str], message: str) -> None:
    """Assert install.sh stopped with *message* before it changed anything."""
    assert result.returncode == 1, (
        f"install.sh should stop with status 1{diagnostic(result)}"
    )
    assert message in result.stderr, f"error should say {message!r}{diagnostic(result)}"
    assert "PEPPY_HOME was created" not in result.stdout, (
        f"install.sh should stop before it changes anything{diagnostic(result)}"
    )


def remove_peppy() -> None:
    """Return the machine to having no peppy installed, services included.

    install.sh treats a running daemon as a reinstall and asks before it
    replaces it, so each test starts from a machine without one.
    """
    result = login_shell(_REMOVE_PEPPY_SCRIPT)
    if result.returncode != 0:
        pytest.fail(f"could not remove the previous test's peppy{diagnostic(result)}")


def set_linger(enabled: bool) -> None:
    """Enable or disable systemd linger for this user."""
    action = "enable-linger" if enabled else "disable-linger"
    run_as_root(f'loginctl {action} "$SUDO_USER"')


def set_fuse2fs_installed(installed: bool) -> None:
    """Install or remove fuse2fs, the one package install.sh installs itself."""
    if installed:
        run_as_root(
            "command -v fuse2fs >/dev/null 2>&1 "
            f"|| {{ {APT_GET} update -qq && {APT_GET} install -y -qq fuse2fs; }}"
        )
        return
    run_as_root(f"! dpkg -s fuse2fs >/dev/null 2>&1 || {APT_GET} remove -y -qq fuse2fs")
