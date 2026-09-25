"""Install an archive of the release and check that its daemon reads the hubs.

The archive is unpacked whole, as install.sh unpacks it, and runs with a
PEPPY_HOME of its own, a messaging port of its own and its apptainer ready.
Two stages check the x86_64 archive of the release this way:

- `hub-check`, in the hub-set job: the PEPPY_HOME pins every hub of the hub
  set at its recorded commit, and `peppy repo refresh --strict` fails when
  the daemon cannot read one of them. The repository index of each hub is
  then checked at its commit, as the hub's own CI checks it.
- `publish`, once it tagged the hubs and before it publishes the release: the
  PEPPY_HOME starts empty, so the daemon writes the default repositories on
  start as it does on a user's machine. A release build reads each default
  hub at its tag of the release (`peppy-release/<tag>`), and `peppy repo
  refresh --strict` fails when one of them cannot be read: this is the
  install users get the moment the release is published.
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
from dataclasses import dataclass
from pathlib import Path

from .cli import ReleaseError, console
from .hub_ci import check_out_commit, require_private_hub_key, resolve, unpack_peppy

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
    with _scratch_directory() as work_dir:
        run_install_check(archive, work_dir, sleep=time.sleep, clock=time.monotonic)


def check_hub_set(hub_set: resolve.HubSet, archive: Path) -> None:
    """Install *archive* in a scratch directory and check every hub of
    *hub_set* at its commit with it."""
    with _scratch_directory() as work_dir:
        run_hub_set_check(
            hub_set, archive, work_dir, sleep=time.sleep, clock=time.monotonic
        )


@contextmanager
def _scratch_directory() -> Iterator[Path]:
    with tempfile.TemporaryDirectory(
        prefix="peppy-install-check-", ignore_cleanup_errors=True
    ) as scratch:
        yield Path(scratch)


def run_install_check(
    archive: Path,
    work_dir: Path,
    *,
    sleep: Callable[[float], None],
    clock: Callable[[], float],
) -> None:
    """Install *archive* under *work_dir* with an empty PEPPY_HOME, and refresh
    every repository its daemon writes on start strictly."""
    console.print(
        f"Checking that the install of {archive.name} reads every default hub..."
    )
    with _serving_install(archive, work_dir, sleep=sleep, clock=clock) as peppy:
        peppy.run(("repo", "refresh", "--strict"))
    console.print("[green]The install reads every default hub.[/green]")


def run_hub_set_check(
    hub_set: resolve.HubSet,
    archive: Path,
    work_dir: Path,
    *,
    sleep: Callable[[float], None],
    clock: Callable[[], float],
) -> None:
    """Install *archive* under *work_dir* with a PEPPY_HOME that pins every hub
    of *hub_set* at its commit, refresh every hub strictly, and check the
    repository index of each hub at its commit. Every hub is checked, and the
    failure names each one that failed."""
    # The daemon and the checkouts read private-nodes-hub over ssh.
    require_private_hub_key()
    console.print(f"Checking every hub of the hub set with {archive.name}...")
    with _serving_install(
        archive,
        work_dir,
        repositories=resolve.release_repositories_file(hub_set),
        sleep=sleep,
        clock=clock,
    ) as peppy:
        # A hub the daemon cannot read at its recorded commit fails the
        # refresh, which also fills the caches mcp-hub's index check reads the
        # contracts from.
        peppy.run(("repo", "refresh", "--strict"))
        failed = [
            resolved.hub.name
            for resolved in hub_set.hubs
            if not _index_check_passes(peppy, resolved, work_dir / "hubs")
        ]
        if failed:
            raise ReleaseError(
                f"the repository index check of {', '.join(failed)} failed at "
                "the commit of the hub set; the log above says why"
            )
    console.print(
        "[green]The install reads every hub of the hub set, and every hub's "
        "repository index checks.[/green]"
    )


def _index_check_passes(
    peppy: PeppyInstall, resolved: resolve.ResolvedHub, checkouts: Path
) -> bool:
    """Check out *resolved* at its commit under *checkouts* and check its
    repository index there, as the hub's own CI checks it."""
    checkout = checkouts / resolved.hub.name
    check_out_commit(resolved, checkout)
    console.print(
        f"Checking the repository index of {resolved.hub.name} at {resolved.commit}..."
    )
    arguments = resolve.index_check_arguments(resolved.hub, checkout)
    return peppy.exit_status(arguments) == 0


@dataclass(frozen=True)
class PeppyInstall:
    """The `peppy` binary of an unpacked archive, and the environment every
    command of it runs with: its own PEPPY_HOME and messaging port."""

    binary: Path
    env: Mapping[str, str]

    def exit_status(self, arguments: Sequence[str]) -> int:
        """Run `peppy <arguments>`, its output going to the log, and return
        its exit status."""
        console.print(f"Running `{_command_line(arguments)}`...")
        result = subprocess.run(
            [str(self.binary), *arguments], stdin=subprocess.DEVNULL, env=self.env
        )
        return result.returncode

    def run(self, arguments: Sequence[str]) -> None:
        """Run `peppy <arguments>`, and stop the check when it fails."""
        status = self.exit_status(arguments)
        if status != 0:
            raise ReleaseError(f"`{_command_line(arguments)}` failed (exit {status})")


def _command_line(arguments: Sequence[str]) -> str:
    return " ".join(("peppy", *arguments))


@contextmanager
def _serving_install(
    archive: Path,
    work_dir: Path,
    *,
    repositories: str | None = None,
    sleep: Callable[[float], None],
    clock: Callable[[], float],
) -> Iterator[PeppyInstall]:
    """Unpack *archive* under *work_dir*, ready its apptainer, and run its
    daemon for the block, with a PEPPY_HOME of its own under *work_dir*.

    The PEPPY_HOME holds *repositories* as its repositories.json5 when it is
    given. Without it, the PEPPY_HOME starts empty and the daemon writes the
    default repositories on start. The daemon is stopped whatever happens.
    """
    binary = unpack_peppy(archive, work_dir / "dist")
    peppy_home = work_dir / "home"
    peppy_home.mkdir(parents=True, exist_ok=True)
    if repositories is not None:
        resolve.write_repositories_file(peppy_home, repositories)
    peppy = PeppyInstall(
        binary=binary,
        env={
            **os.environ,
            "PEPPY_HOME": str(peppy_home),
            "PEPPY_MESSAGING_PORT": str(_free_port()),
        },
    )
    # The daemon refuses to start where its apptainer cannot run unprivileged,
    # and this writes the AppArmor profile that lets it, keyed to this
    # install's path (it needs passwordless sudo).
    peppy.run(("container", "setup"))
    with _running_daemon(
        peppy, peppy_home, work_dir / "serve.log", sleep=sleep, clock=clock
    ):
        yield peppy


def _free_port() -> int:
    """A port no other process listens on, for the daemon's messaging."""
    with socket.socket() as listener:
        listener.bind(("127.0.0.1", 0))
        return listener.getsockname()[1]


@contextmanager
def _running_daemon(
    peppy: PeppyInstall,
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
            [str(peppy.binary), "service", "serve", "--core-node-name", core_node_name],
            stdin=subprocess.DEVNULL,
            stdout=log,
            stderr=subprocess.STDOUT,
            start_new_session=True,
            env=peppy.env,
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
