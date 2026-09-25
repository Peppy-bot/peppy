"""Tests for functions.release_install_check.

The archive is a stand-in whose `peppy` is a shell script that records how it
is called. The checkout of each hub at its commit and the ssh-agent that holds
the deploy key are stand-ins too. The wait for the daemon runs on a fake clock
and a fake sleep, and the stop of the daemon on real processes that either
stop on SIGTERM or ignore it. No case sleeps or depends on how fast the host
is.
"""

from __future__ import annotations

import io
import json
import os
import select
import signal
import subprocess
import tarfile
from collections.abc import Iterator
from contextlib import contextmanager
from dataclasses import dataclass, field
from pathlib import Path
from unittest.mock import patch

import pytest

from functions.cli import ReleaseError
from functions.hub_ci import resolve
from functions.release_install_check import (
    DAEMON_STATE_FILE,
    run_hub_set_check,
    run_install_check,
    stop_daemon,
    wait_for_daemon,
)

from .helpers import HUB_COMMITS, FakeTime, hub_set_document

FAKE_PEPPY = """#!/bin/sh
echo "$* | PEPPY_HOME=$PEPPY_HOME | PEPPY_MESSAGING_PORT=$PEPPY_MESSAGING_PORT" >> "$FAKE_PEPPY_CALLS"
case "$1 $2" in
  "container setup") exit "${FAKE_SETUP_STATUS:-0}" ;;
  "service serve") exec sleep 1000 ;;
  "repo refresh") exit "${FAKE_REFRESH_STATUS:-0}" ;;
  "repo index")
    case " ${FAKE_FAILING_INDEXES:-} " in
      *" $(basename "$3") "*) exit 1 ;;
    esac
    exit 0 ;;
esac
exit 64
"""

HUB_SET = resolve.parse_release_set(json.dumps(hub_set_document()))


def _archive(tmp_path: Path, peppy: str | None = FAKE_PEPPY) -> Path:
    """A release archive whose bin/peppy is *peppy*, as a build packs it."""
    archive = tmp_path / "peppy-x86_64-unknown-linux-gnu.tgz"
    with tarfile.open(archive, "w:gz") as tar:
        if peppy is not None:
            data = peppy.encode()
            member = tarfile.TarInfo("./bin/peppy")
            member.size = len(data)
            member.mode = 0o755
            tar.addfile(member, io.BytesIO(data))
        readme = tarfile.TarInfo("./README")
        tar.addfile(readme, io.BytesIO(b""))
    return archive


class NoSleep:
    """A sleep and a clock for a run whose daemon is up at once: any sleep
    fails the test."""

    def sleep(self, seconds: float) -> None:
        raise AssertionError(f"slept {seconds}s")

    def clock(self) -> float:
        return 0.0


@dataclass
class InstallCheck:
    """The checks of the fake archive in work_dir.

    The daemon's state file is there before the daemon starts, so the wait
    for it returns at once: the wait itself is tested on its own below. Every
    daemon a check stops is recorded, stopped for real, and so is every hub it
    checks out, at the commit it checks it out at.
    """

    archive: Path
    work_dir: Path
    calls: Path
    stopped: list[subprocess.Popen] = field(default_factory=list)
    checked_out: list[tuple[str, str, Path]] = field(default_factory=list)
    deploy_key_loaded: bool = True

    def _recording_stop(self, process: subprocess.Popen) -> None:
        stop_daemon(process)
        self.stopped.append(process)

    def _recording_check_out(
        self, resolved: resolve.ResolvedHub, destination: Path
    ) -> None:
        destination.mkdir(parents=True)
        self.checked_out.append((resolved.hub.name, resolved.commit, destination))

    def __call__(self, archive: Path | None = None) -> None:
        """Check that the install reads the default hubs."""
        with self._stand_ins():
            run_install_check(
                archive or self.archive,
                self.work_dir,
                sleep=NoSleep().sleep,
                clock=NoSleep().clock,
            )

    def check_hub_set(self) -> None:
        """Check every hub of HUB_SET with the install."""
        with self._stand_ins():
            run_hub_set_check(
                HUB_SET,
                self.archive,
                self.work_dir,
                sleep=NoSleep().sleep,
                clock=NoSleep().clock,
            )

    @contextmanager
    def _stand_ins(self) -> Iterator[None]:
        with (
            patch(
                "functions.release_install_check.stop_daemon",
                side_effect=self._recording_stop,
            ),
            patch.object(
                resolve, "check_out_commit", side_effect=self._recording_check_out
            ),
            patch.object(
                resolve, "agent_holds_a_key", return_value=self.deploy_key_loaded
            ),
        ):
            yield


@pytest.fixture
def install(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> InstallCheck:
    calls = tmp_path / "calls"
    monkeypatch.setenv("FAKE_PEPPY_CALLS", str(calls))
    work_dir = tmp_path / "work"
    (work_dir / "home").mkdir(parents=True)
    (work_dir / "home" / DAEMON_STATE_FILE).write_text("{}")
    return InstallCheck(archive=_archive(tmp_path), work_dir=work_dir, calls=calls)


def _calls(path: Path) -> list[list[str]]:
    if not path.exists():
        return []
    return [line.split(" | ") for line in path.read_text().splitlines()]


# --- the install check of publish ---


def test_the_check_readies_apptainer_serves_and_refreshes_strictly(
    install: InstallCheck,
) -> None:
    install()

    calls = _calls(install.calls)
    assert [call[0].split()[:3] for call in calls] == [
        ["container", "setup"],
        ["service", "serve", "--core-node-name"],
        ["repo", "refresh", "--strict"],
    ]
    assert calls[1][0].split()[3].startswith("release-install-check-")
    # One PEPPY_HOME of the check's own, and one messaging port, for all.
    assert {call[1] for call in calls} == {f"PEPPY_HOME={install.work_dir / 'home'}"}
    [port] = {call[2] for call in calls}
    assert port.removeprefix("PEPPY_MESSAGING_PORT=").isdigit()
    [daemon] = install.stopped
    assert daemon.args[1:3] == ["service", "serve"]
    assert daemon.returncode is not None


def test_a_failed_refresh_fails_the_check_and_stops_the_daemon(
    install: InstallCheck,
    monkeypatch: pytest.MonkeyPatch,
    capfd: pytest.CaptureFixture[str],
) -> None:
    monkeypatch.setenv("FAKE_REFRESH_STATUS", "1")

    with pytest.raises(
        ReleaseError, match=r"`peppy repo refresh --strict` failed \(exit 1\)"
    ):
        install()

    [daemon] = install.stopped
    assert daemon.returncode is not None
    assert "The daemon's log:" in capfd.readouterr().err


def test_a_failed_container_setup_starts_no_daemon(
    install: InstallCheck, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.setenv("FAKE_SETUP_STATUS", "3")

    with pytest.raises(
        ReleaseError, match=r"`peppy container setup` failed \(exit 3\)"
    ):
        install()

    assert [call[0] for call in _calls(install.calls)] == ["container setup"]
    assert install.stopped == []


def test_an_archive_without_peppy_is_refused(
    install: InstallCheck, tmp_path: Path
) -> None:
    with pytest.raises(ReleaseError, match="holds no bin/peppy"):
        install(_archive(tmp_path, peppy=None))

    assert _calls(install.calls) == []


def test_an_archive_that_is_no_archive_is_refused(
    install: InstallCheck, tmp_path: Path
) -> None:
    broken = tmp_path / "broken.tgz"
    broken.write_bytes(b"not a tarball")

    with pytest.raises(ReleaseError, match="unpacking .*broken.tgz failed"):
        install(broken)


# --- the hub-check stage ---


def test_the_hub_check_pins_the_set_refreshes_and_checks_every_index(
    install: InstallCheck,
) -> None:
    install.check_hub_set()

    home = install.work_dir / "home"
    assert (home / "conf" / "repositories.json5").read_text() == (
        resolve.release_repositories_file(HUB_SET)
    )
    checkouts = install.work_dir / "hubs"
    assert install.checked_out == [
        (name, commit, checkouts / name) for name, commit in HUB_COMMITS.items()
    ]
    calls = _calls(install.calls)
    assert [call[0].split()[:2] for call in calls[:2]] == [
        ["container", "setup"],
        ["service", "serve"],
    ]
    assert [call[0].split() for call in calls[2:]] == [
        ["repo", "refresh", "--strict"],
        *(
            resolve.index_check_arguments(resolved.hub, checkouts / resolved.hub.name)
            for resolved in HUB_SET.hubs
        ),
    ]
    # Every command runs with the daemon's PEPPY_HOME and messaging port.
    assert {call[1] for call in calls} == {f"PEPPY_HOME={home}"}
    assert len({call[2] for call in calls}) == 1
    [daemon] = install.stopped
    assert daemon.returncode is not None


def test_a_failed_index_check_names_every_hub_that_failed(
    install: InstallCheck,
    monkeypatch: pytest.MonkeyPatch,
    capfd: pytest.CaptureFixture[str],
) -> None:
    monkeypatch.setenv("FAKE_FAILING_INDEXES", "launchers-hub private-nodes-hub")

    with pytest.raises(ReleaseError) as excinfo:
        install.check_hub_set()

    assert str(excinfo.value).startswith(
        "the repository index check of launchers-hub, private-nodes-hub failed"
    )
    # Every hub was checked, those after a failure included.
    assert [name for name, _, _ in install.checked_out] == list(HUB_COMMITS)
    [daemon] = install.stopped
    assert daemon.returncode is not None
    assert "The daemon's log:" in capfd.readouterr().err


def test_a_hub_the_daemon_cannot_read_fails_before_any_index_check(
    install: InstallCheck, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.setenv("FAKE_REFRESH_STATUS", "1")

    with pytest.raises(
        ReleaseError, match=r"`peppy repo refresh --strict` failed \(exit 1\)"
    ):
        install.check_hub_set()

    assert install.checked_out == []


def test_the_hub_check_needs_the_deploy_key(install: InstallCheck) -> None:
    install.deploy_key_loaded = False

    with pytest.raises(ReleaseError, match="no deploy key is loaded"):
        install.check_hub_set()

    assert _calls(install.calls) == []
    assert not (install.work_dir / "dist").exists()


# --- the wait for the daemon ---


class FakeDaemon:
    """A daemon process as the wait sees it: running until it writes
    *state_file* at its *serves_at_poll*-th poll, or until it exits with
    *exit_status* after *polls_before_exit* polls, or running for ever."""

    def __init__(
        self,
        state_file: Path,
        *,
        serves_at_poll: int | None = None,
        polls_before_exit: int | None = None,
        exit_status: int = 1,
    ) -> None:
        self.polls = 0
        self.state_file = state_file
        self.serves_at_poll = serves_at_poll
        self.polls_before_exit = polls_before_exit
        self.exit_status = exit_status

    def poll(self) -> int | None:
        self.polls += 1
        if self.polls == self.serves_at_poll:
            self.state_file.write_text("{}")
        if self.polls_before_exit is not None and self.polls > self.polls_before_exit:
            return self.exit_status
        return None


def test_the_wait_ends_once_the_daemon_writes_its_state(tmp_path: Path) -> None:
    state_file = tmp_path / DAEMON_STATE_FILE
    time = FakeTime()

    wait_for_daemon(
        FakeDaemon(state_file, serves_at_poll=3),
        state_file,
        sleep=time.sleep,
        clock=time.clock,
    )

    assert time.sleeps == [1.0, 1.0, 1.0]


def test_a_daemon_that_exits_before_it_serves_fails_the_wait(tmp_path: Path) -> None:
    state_file = tmp_path / DAEMON_STATE_FILE
    time = FakeTime()

    with pytest.raises(
        ReleaseError, match="the daemon exited with status 2 before it served"
    ):
        wait_for_daemon(
            FakeDaemon(state_file, polls_before_exit=2, exit_status=2),
            state_file,
            sleep=time.sleep,
            clock=time.clock,
        )


def test_a_daemon_that_never_serves_fails_the_wait_at_its_timeout(
    tmp_path: Path,
) -> None:
    state_file = tmp_path / DAEMON_STATE_FILE
    time = FakeTime()

    with pytest.raises(
        ReleaseError, match="the daemon did not serve within 60 seconds"
    ):
        wait_for_daemon(
            FakeDaemon(state_file), state_file, sleep=time.sleep, clock=time.clock
        )

    assert time.now == 60.0


# --- the stop of the daemon ---


def _start_session(script: str) -> tuple[subprocess.Popen, int]:
    """Start *script* in a session of its own, as the daemon runs, and return
    once it wrote `ready` to the pipe it is given as the descriptor $READY.

    Every process of the session holds the write end of that pipe, whose read
    end is returned: it reads end of file once all of them are gone.
    """
    read_end, write_end = os.pipe()
    process = subprocess.Popen(
        ["bash", "-c", script],
        pass_fds=(write_end,),
        start_new_session=True,
        env={**os.environ, "READY": str(write_end)},
    )
    os.close(write_end)
    assert os.read(read_end, len(b"ready\n")) == b"ready\n"
    return process, read_end


def _session_ended(read_end: int) -> bool:
    """Whether every process of the session is gone. The bound only turns a
    session that outlives its stop into a failure instead of a hang."""
    ready, _, _ = select.select([read_end], [], [], 60)
    ended = bool(ready) and os.read(read_end, 1) == b""
    os.close(read_end)
    return ended


def test_the_daemon_and_its_session_stop_on_sigterm() -> None:
    process, read_end = _start_session(
        "sleep 1000 & echo ready >&$READY; exec sleep 1000"
    )

    stop_daemon(process)

    assert process.returncode == -signal.SIGTERM
    assert _session_ended(read_end)


def test_a_process_the_daemon_leaves_behind_is_killed() -> None:
    # The daemon stops on SIGTERM; a process it started ignores it.
    process, read_end = _start_session(
        "(trap '' TERM; echo ready >&$READY; exec sleep 1000) & exec sleep 1000"
    )

    stop_daemon(process)

    assert process.returncode == -signal.SIGTERM
    assert _session_ended(read_end)


def test_a_daemon_that_ignores_sigterm_is_killed() -> None:
    process, read_end = _start_session(
        "trap '' TERM; echo ready >&$READY; exec sleep 1000"
    )

    with patch("functions.release_install_check.DAEMON_STOP_TIMEOUT_SECONDS", 0.0):
        stop_daemon(process)

    assert process.returncode == -signal.SIGKILL
    assert _session_ended(read_end)


def test_stopping_a_daemon_that_already_exited_is_harmless() -> None:
    process, read_end = _start_session("echo ready >&$READY; exit 0")
    process.wait()

    stop_daemon(process)

    assert process.returncode == 0
    assert _session_ended(read_end)
