"""Tests for functions.claude (shared claude CLI helper)."""

from __future__ import annotations

import json
from pathlib import Path
from typing import Any, Callable
from unittest.mock import MagicMock, patch

import pytest

from functions.claude import (
    CLAUDE_EFFORT,
    CLAUDE_MODEL,
    HEARTBEAT_SECONDS,
    _heartbeat,
    run_claude,
)
from functions.cli import ReleaseError

_SCHEMA: dict = {
    "type": "object",
    "properties": {"ok": {"type": "boolean"}},
    "required": ["ok"],
}


def _flag_value(cmd: list[str], flag: str) -> str:
    """Return the argv element following ``flag`` in ``cmd``."""
    assert flag in cmd, f"{flag} not in {cmd}"
    return cmd[cmd.index(flag) + 1]


def _mock_run(
    structured: object,
    returncode: int = 0,
    *,
    capture: dict[str, Any] | None = None,
) -> Callable[..., MagicMock]:
    """Build a subprocess.run replacement mimicking ``claude --json-schema``."""

    def _run(cmd: list[str], *args: object, **kwargs: object) -> MagicMock:
        if capture is not None:
            capture["cmd"] = cmd
            capture["input"] = kwargs.get("input")
        mock = MagicMock()
        mock.returncode = returncode
        mock.stdout = json.dumps(
            {
                "type": "result",
                "result": json.dumps(structured),
                "structured_output": structured,
            }
        )
        mock.stderr = ""
        return mock

    return _run


# --- run_claude ---


def test_run_claude_returns_structured_output(tmp_path: Path) -> None:
    with patch(
        "functions.claude.subprocess.run", side_effect=_mock_run({"ok": True})
    ):
        out = run_claude(
            "prompt",
            allowed_tools="Read",
            permission_mode="bypassPermissions",
            cwd=tmp_path,
            json_schema=_SCHEMA,
            activity="judging",
        )
    assert out == {"ok": True}


def test_run_claude_pins_model_effort_and_pipes_prompt_via_stdin(
    tmp_path: Path,
) -> None:
    capture: dict[str, Any] = {}
    with patch(
        "functions.claude.subprocess.run",
        side_effect=_mock_run({"ok": True}, capture=capture),
    ):
        run_claude(
            "the-prompt",
            allowed_tools="Read Grep Glob",
            permission_mode="bypassPermissions",
            cwd=tmp_path,
            json_schema=_SCHEMA,
            activity="judging",
        )
    cmd = capture["cmd"]
    assert cmd[0] == "claude"
    assert "-p" in cmd
    assert _flag_value(cmd, "--model") == CLAUDE_MODEL
    assert _flag_value(cmd, "--effort") == CLAUDE_EFFORT
    assert _flag_value(cmd, "--output-format") == "json"
    assert _flag_value(cmd, "--permission-mode") == "bypassPermissions"
    assert _flag_value(cmd, "--allowed-tools") == "Read Grep Glob"
    # The prompt is piped via stdin, never placed on the command line.
    assert capture["input"] == "the-prompt"
    assert "the-prompt" not in cmd


def test_run_claude_passes_schema_as_json(tmp_path: Path) -> None:
    capture: dict[str, Any] = {}
    with patch(
        "functions.claude.subprocess.run",
        side_effect=_mock_run({"ok": True}, capture=capture),
    ):
        run_claude(
            "p",
            allowed_tools="Read",
            permission_mode="default",
            cwd=tmp_path,
            json_schema=_SCHEMA,
            activity="judging",
        )
    # The schema rides argv as serialized JSON the CLI can validate against.
    assert json.loads(_flag_value(capture["cmd"], "--json-schema")) == _SCHEMA


def test_run_claude_disables_tools_when_tools_empty(tmp_path: Path) -> None:
    capture: dict[str, Any] = {}
    with patch(
        "functions.claude.subprocess.run",
        side_effect=_mock_run({"ok": True}, capture=capture),
    ):
        run_claude(
            "p",
            allowed_tools="",
            permission_mode="bypassPermissions",
            cwd=tmp_path,
            json_schema=_SCHEMA,
            activity="judging",
            tools="",
        )
    cmd = capture["cmd"]
    # `--tools ""` disables all tools; an empty allowlist is omitted entirely.
    assert _flag_value(cmd, "--tools") == ""
    assert "--allowed-tools" not in cmd


def test_run_claude_effort_override(tmp_path: Path) -> None:
    capture: dict[str, Any] = {}
    with patch(
        "functions.claude.subprocess.run",
        side_effect=_mock_run({"ok": True}, capture=capture),
    ):
        run_claude(
            "p",
            allowed_tools="Read",
            permission_mode="default",
            cwd=tmp_path,
            json_schema=_SCHEMA,
            activity="judging",
            effort="low",
        )
    assert _flag_value(capture["cmd"], "--effort") == "low"


def test_run_claude_defaults_to_pinned_effort(tmp_path: Path) -> None:
    capture: dict[str, Any] = {}
    with patch(
        "functions.claude.subprocess.run",
        side_effect=_mock_run({"ok": True}, capture=capture),
    ):
        run_claude(
            "p",
            allowed_tools="Read",
            permission_mode="default",
            cwd=tmp_path,
            json_schema=_SCHEMA,
            activity="judging",
        )
    assert _flag_value(capture["cmd"], "--effort") == CLAUDE_EFFORT


def test_run_claude_omits_tools_flag_by_default(tmp_path: Path) -> None:
    capture: dict[str, Any] = {}
    with patch(
        "functions.claude.subprocess.run",
        side_effect=_mock_run({"ok": True}, capture=capture),
    ):
        run_claude(
            "p",
            allowed_tools="Read",
            permission_mode="default",
            cwd=tmp_path,
            json_schema=_SCHEMA,
            activity="judging",
        )
    cmd = capture["cmd"]
    assert "--tools" not in cmd
    assert _flag_value(cmd, "--allowed-tools") == "Read"


def test_run_claude_raises_on_nonzero_exit(tmp_path: Path) -> None:
    mock = MagicMock()
    mock.returncode = 2
    mock.stdout = ""
    mock.stderr = "boom"
    with patch("functions.claude.subprocess.run", return_value=mock):
        with pytest.raises(ReleaseError, match="claude CLI failed"):
            run_claude(
                "p",
                allowed_tools="Read",
                permission_mode="bypassPermissions",
                cwd=tmp_path,
                json_schema=_SCHEMA,
                activity="judging",
            )


def test_run_claude_raises_on_invalid_outer_json(tmp_path: Path) -> None:
    mock = MagicMock()
    mock.returncode = 0
    mock.stdout = "not json"
    mock.stderr = ""
    with patch("functions.claude.subprocess.run", return_value=mock):
        with pytest.raises(ReleaseError, match="did not return valid JSON"):
            run_claude(
                "p",
                allowed_tools="Read",
                permission_mode="bypassPermissions",
                cwd=tmp_path,
                json_schema=_SCHEMA,
                activity="judging",
            )


def test_run_claude_raises_on_non_object_envelope(tmp_path: Path) -> None:
    mock = MagicMock()
    mock.returncode = 0
    mock.stdout = "[]"
    mock.stderr = ""
    with patch("functions.claude.subprocess.run", return_value=mock):
        with pytest.raises(ReleaseError, match="not an object"):
            run_claude(
                "p",
                allowed_tools="Read",
                permission_mode="bypassPermissions",
                cwd=tmp_path,
                json_schema=_SCHEMA,
                activity="judging",
            )


def test_run_claude_raises_on_missing_structured_output(tmp_path: Path) -> None:
    # An envelope without structured_output means the CLI never forced the
    # schema (an older CLI, or a run that died before the final answer); the
    # prose result is surfaced so the failure is diagnosable.
    mock = MagicMock()
    mock.returncode = 0
    mock.stdout = json.dumps({"type": "result", "result": "The docs are fine."})
    mock.stderr = ""
    with patch("functions.claude.subprocess.run", return_value=mock):
        with pytest.raises(
            ReleaseError, match="missing 'structured_output'"
        ) as excinfo:
            run_claude(
                "p",
                allowed_tools="Read",
                permission_mode="bypassPermissions",
                cwd=tmp_path,
                json_schema=_SCHEMA,
                activity="judging",
            )
    assert "The docs are fine." in str(excinfo.value)


def test_run_claude_raises_on_non_object_structured_output(
    tmp_path: Path,
) -> None:
    mock = MagicMock()
    mock.returncode = 0
    mock.stdout = json.dumps(
        {"type": "result", "result": "[]", "structured_output": []}
    )
    mock.stderr = ""
    with patch("functions.claude.subprocess.run", return_value=mock):
        with pytest.raises(ReleaseError, match="missing 'structured_output'"):
            run_claude(
                "p",
                allowed_tools="Read",
                permission_mode="bypassPermissions",
                cwd=tmp_path,
                json_schema=_SCHEMA,
                activity="judging",
            )


# --- the heartbeat ---


def test_heartbeat_says_the_minutes_so_far_until_stopped(
    capfd: pytest.CaptureFixture[str],
) -> None:
    stop = MagicMock()
    stop.wait.side_effect = [False, False, True]
    with patch("functions.claude.time.monotonic", side_effect=[70.0, 130.0]):
        _heartbeat("judging the diff", stop, 10.0)

    err = " ".join(capfd.readouterr().err.split())
    assert "Still judging the diff (1 min so far)..." in err
    assert "Still judging the diff (2 min so far)..." in err
    stop.wait.assert_called_with(HEARTBEAT_SECONDS)


class _RecordedThread:
    """A stand-in for threading.Thread that records how it was driven."""

    started: list[_RecordedThread] = []

    def __init__(self, *, target: Any, args: tuple, daemon: bool) -> None:
        self.target = target
        self.args = args
        self.daemon = daemon
        self.running = False
        self.joined = False

    def start(self) -> None:
        self.running = True
        _RecordedThread.started.append(self)

    def join(self) -> None:
        self.joined = True


@pytest.fixture
def recorded_threads() -> list[_RecordedThread]:
    _RecordedThread.started = []
    with patch("functions.claude.threading.Thread", _RecordedThread):
        yield _RecordedThread.started


def test_run_claude_keeps_a_heartbeat_only_while_the_cli_runs(
    tmp_path: Path, recorded_threads: list[_RecordedThread]
) -> None:
    with patch(
        "functions.claude.subprocess.run", side_effect=_mock_run({"ok": True})
    ):
        run_claude(
            "p",
            allowed_tools="Read",
            permission_mode="default",
            cwd=tmp_path,
            json_schema=_SCHEMA,
            activity="judging the diff",
        )

    (thread,) = recorded_threads
    assert thread.target is _heartbeat
    assert thread.daemon
    activity, stop, _started = thread.args
    assert activity == "judging the diff"
    # Stopped and joined once the CLI answered, so no line prints afterwards.
    assert stop.is_set()
    assert thread.joined


def test_run_claude_stops_the_heartbeat_when_the_cli_fails(
    tmp_path: Path, recorded_threads: list[_RecordedThread]
) -> None:
    with patch(
        "functions.claude.subprocess.run",
        side_effect=_mock_run({"ok": True}, returncode=2),
    ):
        with pytest.raises(ReleaseError, match="claude CLI failed"):
            run_claude(
                "p",
                allowed_tools="Read",
                permission_mode="default",
                cwd=tmp_path,
                json_schema=_SCHEMA,
                activity="judging the diff",
            )

    (thread,) = recorded_threads
    assert thread.args[1].is_set()
    assert thread.joined


# --- pinned model / effort (reproducibility) ---


def test_claude_model_is_pinned_to_exact_id() -> None:
    # A bare alias ("opus") would follow the moving "latest" pointer and
    # defeat the reproducibility pin; require a full versioned id.
    assert CLAUDE_MODEL.startswith("claude-")
    assert CLAUDE_MODEL not in ("opus", "sonnet", "haiku")
    assert CLAUDE_EFFORT == "xhigh"
