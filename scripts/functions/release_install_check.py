"""Check that the archive of a release, installed, reads every default hub.

The publish stage runs this once it tagged the hubs and before it publishes
the release. The archive is unpacked whole, as install.sh unpacks it, and
runs with a PEPPY_HOME of its own, empty, so its daemon writes the default
repositories on start as it does on a user's machine. A release build reads
each default hub at its tag of the release (`peppy-release/<tag>`), and
`peppy repo refresh --strict` fails when one of them cannot be read: this is
the install users get the moment the release is published.
"""

from __future__ import annotations

import os
import signal
import socket
import subprocess
import tempfile
import time
import uuid
from collections.abc import Callable, Iterator, Mapping, Sequence
from contextlib import contextmanager
from pathlib import Path

from .cli import ReleaseError, console

# The file the daemon writes under its PEPPY_HOME once it serves, and which the
# CLI finds it through.
DAEMON_STATE_FILE = "daemon_state.json5"
DAEMON_START_TIMEOUT_SECONDS = 60.0
DAEMON_START_POLL_SECONDS = 1.0
# How long the daemon has to stop on SIGTERM before it is killed.
DAEMON_STOP_TIMEOUT_SECONDS = 30.0


def check_release_install(archive: Path) -> None:
    """Install *archive* in a scratch directory and check that its daemon reads
    every default hub at its tag of the release."""
    with tempfile.TemporaryDirectory(
        prefix="peppy-install-check-", ignore_cleanup_errors=True
    ) as scratch:
        run_install_check(
            archive, Path(scratch), sleep=time.sleep, clock=time.monotonic
        )


def run_install_check(
    archive: Path,
    work_dir: Path,
    *,
    sleep: Callable[[float], None],
    clock: Callable[[], float],
) -> None:
    """Unpack *archive* under *work_dir*, ready its apptainer, start its
    daemon with a PEPPY_HOME of its own under *work_dir*, and refresh every
    repository strictly. The daemon is stopped whatever happens."""
    console.print(
        f"Checking that the install of {archive.name} reads every default hub..."
    )
    peppy = _unpack(archive, work_dir / "dist")
    peppy_home = work_dir / "home"
    peppy_home.mkdir(parents=True, exist_ok=True)
    env = {
        **os.environ,
        "PEPPY_HOME": str(peppy_home),
        "PEPPY_MESSAGING_PORT": str(_free_port()),
    }
    # The daemon refuses to start where its apptainer cannot run unprivileged,
    # and this writes the AppArmor profile that lets it, keyed to this
    # install's path (it needs passwordless sudo).
    _run_peppy(peppy, ("container", "setup"), env)
    with _running_daemon(
        peppy, env, peppy_home, work_dir / "serve.log", sleep=sleep, clock=clock
    ):
        _run_peppy(peppy, ("repo", "refresh", "--strict"), env)
    console.print("[green]The install reads every default hub.[/green]")


def _unpack(archive: Path, destination: Path) -> Path:
    """Unpack the whole archive, which the daemon needs around its binary (the
    bundled router and apptainer), and return the `peppy` binary."""
    destination.mkdir(parents=True)
    result = subprocess.run(
        ["tar", "-xzf", str(archive), "-C", str(destination)],
        capture_output=True,
        text=True,
    )
    if result.returncode != 0:
        raise ReleaseError(f"unpacking {archive} failed: {result.stderr.strip()}")
    peppy = destination / "bin" / "peppy"
    if not peppy.is_file():
        raise ReleaseError(f"the archive {archive} holds no bin/peppy")
    return peppy


def _free_port() -> int:
    """A port no other process listens on, for the daemon's messaging."""
    with socket.socket() as listener:
        listener.bind(("127.0.0.1", 0))
        return listener.getsockname()[1]


def _run_peppy(peppy: Path, arguments: Sequence[str], env: Mapping[str, str]) -> None:
    command = " ".join(("peppy", *arguments))
    console.print(f"Running `{command}`...")
    result = subprocess.run([str(peppy), *arguments], stdin=subprocess.DEVNULL, env=env)
    if result.returncode != 0:
        raise ReleaseError(f"`{command}` failed (exit {result.returncode})")


@contextmanager
def _running_daemon(
    peppy: Path,
    env: Mapping[str, str],
    peppy_home: Path,
    log_path: Path,
    *,
    sleep: Callable[[float], None],
    clock: Callable[[], float],
) -> Iterator[None]:
    """Run the daemon of *peppy* for the block, and stop it afterwards.

    The daemon runs in a session of its own, so stopping it reaches the
    processes it started too. On a failure, its log is printed once it has
    stopped.
    """
    # Core-node names must be unique among daemons that can reach each other.
    core_node_name = f"release-install-check-{uuid.uuid4().hex[:12]}"
    with log_path.open("w") as log:
        process = subprocess.Popen(
            [str(peppy), "service", "serve", "--core-node-name", core_node_name],
            stdin=subprocess.DEVNULL,
            stdout=log,
            stderr=subprocess.STDOUT,
            start_new_session=True,
            env=env,
        )
    failed = True
    try:
        wait_for_daemon(
            process, peppy_home / DAEMON_STATE_FILE, sleep=sleep, clock=clock
        )
        yield
        failed = False
    finally:
        stop_daemon(process)
        if failed:
            console.print("[red]The daemon's log:[/red]")
            console.print(
                log_path.read_text(errors="replace"), markup=False, highlight=False
            )


def wait_for_daemon(
    process: subprocess.Popen,
    state_file: Path,
    *,
    sleep: Callable[[float], None],
    clock: Callable[[], float],
) -> None:
    """Wait until the daemon writes *state_file*, for at most
    DAEMON_START_TIMEOUT_SECONDS. A daemon that exits first fails the wait."""
    deadline = clock() + DAEMON_START_TIMEOUT_SECONDS
    while not state_file.is_file():
        status = process.poll()
        if status is not None:
            raise ReleaseError(
                f"the daemon exited with status {status} before it served"
            )
        if clock() >= deadline:
            raise ReleaseError(
                f"the daemon did not serve within {DAEMON_START_TIMEOUT_SECONDS:.0f} "
                f"seconds: it wrote no {state_file}"
            )
        sleep(DAEMON_START_POLL_SECONDS)


def stop_daemon(process: subprocess.Popen) -> None:
    """Stop the daemon and every process of its session.

    SIGTERM is the stop the daemon handles; a daemon still running after
    DAEMON_STOP_TIMEOUT_SECONDS is killed. SIGKILL then reaches whatever its
    session still holds, a process it started and left behind included.
    """
    _signal_session(process, signal.SIGTERM)
    try:
        process.wait(timeout=DAEMON_STOP_TIMEOUT_SECONDS)
    except subprocess.TimeoutExpired:
        console.print(
            f"[yellow]The daemon did not stop within "
            f"{DAEMON_STOP_TIMEOUT_SECONDS:.0f} seconds; killing it.[/yellow]"
        )
    _signal_session(process, signal.SIGKILL)
    process.wait()


def _signal_session(process: subprocess.Popen, signal_number: int) -> None:
    """Send *signal_number* to every process of the daemon's session, whose
    process group id is the daemon's pid."""
    try:
        os.killpg(process.pid, signal_number)
    except ProcessLookupError:
        pass
