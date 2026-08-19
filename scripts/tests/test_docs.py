"""Tests for functions.docs module."""

from __future__ import annotations

import json
from pathlib import Path
from typing import Any
from unittest.mock import MagicMock, patch

import pytest

from functions.claude import CLAUDE_EFFORT, CLAUDE_MODEL
from functions.cli import ReleaseError
from functions.docs import (
    _CHECK_SCHEMA,
    _UPDATE_SCHEMA,
    CheckResult,
    RequiredChange,
    UpdateOutcome,
    UpdateResult,
    _is_code_path,
    _parse_check_response,
    _parse_update_response,
    _truncate_diff,
    check_docs,
    update_docs,
)


def _flag_value(cmd: list[str], flag: str) -> str:
    """Return the argv element following ``flag`` in ``cmd``."""
    assert flag in cmd, f"{flag} not in {cmd}"
    return cmd[cmd.index(flag) + 1]


def _blocking(file: str = "docs/x.mdx", change: str = "add flag") -> RequiredChange:
    return RequiredChange(file=file, change=change, severity="blocking")


def _minor(file: str = "docs/y.mdx", change: str = "reword") -> RequiredChange:
    return RequiredChange(file=file, change=change, severity="minor")


# --- _is_code_path ---


@pytest.mark.parametrize(
    "path, expected",
    [
        ("crates/peppy/src/main.rs", True),
        ("scripts/functions/cli.py", True),
        ("Cargo.toml", True),
        ("docs/src/content/docs/guides/installation.mdx", False),
        ("docs/astro.config.mjs", False),
        ("target/debug/foo", False),
        (".github/workflows/tests.yml", False),
        ("scripts/docs/is_doc_up_to_date.py", False),
        ("scripts/functions/docs.py", False),
        ("scripts/tests/test_docs.py", False),
    ],
)
def test_is_code_path(path: str, expected: bool) -> None:
    assert _is_code_path(path) is expected


# --- _truncate_diff ---


def test_truncate_diff_short_unchanged() -> None:
    diff = "a" * 1000
    assert _truncate_diff(diff) == diff


def test_truncate_diff_long_truncated() -> None:
    diff = "a" * 500_000
    out = _truncate_diff(diff)
    assert len(out) < len(diff)
    assert "diff truncated" in out


# --- the schemas the CLI enforces ---


def test_check_schema_pins_required_fields_and_severity_enum() -> None:
    # The Python validator and the CLI-side schema must not drift: a schema
    # that stops requiring a field would let entries through that
    # _parse_check_response rejects, turning verdicts into release-time
    # crashes.
    assert _CHECK_SCHEMA["required"] == ["required_changes"]
    item = _CHECK_SCHEMA["properties"]["required_changes"]["items"]
    assert item["required"] == ["file", "change", "severity"]
    assert item["properties"]["severity"]["enum"] == ["blocking", "minor"]


def test_update_schema_pins_required_fields_and_status_enum() -> None:
    assert _UPDATE_SCHEMA["required"] == ["results", "summary"]
    item = _UPDATE_SCHEMA["properties"]["results"]["items"]
    assert item["required"] == ["file", "change", "status"]
    assert item["properties"]["status"]["enum"] == [
        "implemented",
        "already_covered",
    ]


# --- CheckResult / UpdateResult ---


def test_check_result_splits_blocking_and_minor() -> None:
    result = CheckResult(changes=(_blocking(), _minor(), _blocking("docs/z.mdx")))
    assert result.blocking == (_blocking(), _blocking("docs/z.mdx"))
    assert result.minor == (_minor(),)


def test_update_result_all_already_covered() -> None:
    covered = UpdateOutcome(file="docs/x.mdx", change="c", status="already_covered")
    implemented = UpdateOutcome(file="docs/x.mdx", change="c", status="implemented")
    assert UpdateResult(results=(covered, covered), summary="s").all_already_covered
    assert not UpdateResult(
        results=(covered, implemented), summary="s"
    ).all_already_covered
    # An empty report is not a verdict: the updater must name what it checked.
    assert not UpdateResult(results=(), summary="s").all_already_covered


# --- _parse_check_response ---


def test_parse_check_response_empty_is_clean() -> None:
    assert _parse_check_response({"required_changes": []}) == CheckResult(changes=())


def test_parse_check_response_keeps_severities() -> None:
    payload = {
        "required_changes": [
            {
                "file": "docs/src/content/docs/guides/installation.mdx",
                "change": "mention new --verbose flag",
                "severity": "blocking",
            },
            {"file": "docs/a.mdx", "change": "reword intro", "severity": "minor"},
        ]
    }
    result = _parse_check_response(payload)
    assert result.blocking == (
        RequiredChange(
            file="docs/src/content/docs/guides/installation.mdx",
            change="mention new --verbose flag",
            severity="blocking",
        ),
    )
    assert result.minor == (
        RequiredChange(file="docs/a.mdx", change="reword intro", severity="minor"),
    )


def test_parse_check_response_missing_changes() -> None:
    with pytest.raises(ReleaseError, match="required_changes"):
        _parse_check_response({})


def test_parse_check_response_changes_not_list() -> None:
    with pytest.raises(ReleaseError, match="required_changes"):
        _parse_check_response({"required_changes": "foo"})


def test_parse_check_response_change_entry_not_object() -> None:
    with pytest.raises(ReleaseError, match="must be an object"):
        _parse_check_response({"required_changes": ["docs/a.mdx"]})


def test_parse_check_response_change_entry_missing_fields() -> None:
    with pytest.raises(ReleaseError, match="file/change"):
        _parse_check_response(
            {"required_changes": [{"file": "a.md", "severity": "minor"}]}
        )


def test_parse_check_response_unknown_severity() -> None:
    with pytest.raises(ReleaseError, match="unknown severity"):
        _parse_check_response(
            {
                "required_changes": [
                    {"file": "a.md", "change": "c", "severity": "critical"}
                ]
            }
        )


# --- _parse_update_response ---


def test_parse_update_response_valid() -> None:
    payload = {
        "results": [
            {"file": "docs/x.mdx", "change": "add flag", "status": "implemented"},
            {"file": "docs/y.mdx", "change": "fix", "status": "already_covered"},
        ],
        "summary": "closed one gap",
    }
    result = _parse_update_response(payload)
    assert result == UpdateResult(
        results=(
            UpdateOutcome(file="docs/x.mdx", change="add flag", status="implemented"),
            UpdateOutcome(file="docs/y.mdx", change="fix", status="already_covered"),
        ),
        summary="closed one gap",
    )


def test_parse_update_response_results_not_list() -> None:
    with pytest.raises(ReleaseError, match="'results' must be a list"):
        _parse_update_response({"results": {}, "summary": "s"})


def test_parse_update_response_missing_summary() -> None:
    with pytest.raises(ReleaseError, match="summary"):
        _parse_update_response({"results": []})


def test_parse_update_response_entry_not_object() -> None:
    with pytest.raises(ReleaseError, match="must be an object"):
        _parse_update_response({"results": ["x"], "summary": "s"})


def test_parse_update_response_unknown_status() -> None:
    with pytest.raises(ReleaseError, match="unknown status"):
        _parse_update_response(
            {
                "results": [{"file": "a", "change": "c", "status": "skipped"}],
                "summary": "s",
            }
        )


# --- check_docs / update_docs (mocked claude) ---


def _mock_subprocess_run_for_claude(
    structured: object,
    *,
    capture: dict[str, Any] | None = None,
) -> MagicMock:
    """Build a MagicMock replacement for subprocess.run.

    Returns the provided object as the envelope's structured output for
    claude calls; git calls are not expected (get_code_diff is patched).
    """

    def _run(cmd: list[str], *args: object, **kwargs: object) -> MagicMock:
        if cmd and cmd[0] == "claude":
            if capture is not None:
                capture["cmd"] = cmd
                capture["input"] = kwargs.get("input")
            mock = MagicMock()
            mock.returncode = 0
            mock.stdout = json.dumps(
                {
                    "type": "result",
                    "result": json.dumps(structured),
                    "structured_output": structured,
                }
            )
            mock.stderr = ""
            return mock
        raise AssertionError(f"unexpected command: {cmd}")

    return MagicMock(side_effect=_run)


def test_check_docs_short_circuits_on_no_code_changes(tmp_path: Path) -> None:
    with patch("functions.docs.get_repo_root", return_value=tmp_path), \
         patch("functions.docs.get_code_diff", return_value=("", [])), \
         patch("functions.claude.subprocess.run") as mock_run:
        result = check_docs("BASE", "HEAD")
    assert result == CheckResult(changes=())
    mock_run.assert_not_called()


def test_check_docs_parses_claude_verdict(tmp_path: Path) -> None:
    verdict = {
        "required_changes": [
            {"file": "docs/x.mdx", "change": "add flag", "severity": "blocking"}
        ]
    }
    with patch("functions.docs.get_repo_root", return_value=tmp_path), \
         patch(
             "functions.docs.get_code_diff",
             return_value=("diff", ["crates/foo.rs"]),
         ), \
         patch(
             "functions.claude.subprocess.run",
             _mock_subprocess_run_for_claude(verdict),
         ):
        result = check_docs("BASE", "HEAD")
    assert result.blocking == (_blocking(),)
    assert result.minor == ()


def test_check_docs_enforces_schema_and_readonly_tools(tmp_path: Path) -> None:
    capture: dict[str, Any] = {}
    verdict: dict = {"required_changes": []}
    with patch("functions.docs.get_repo_root", return_value=tmp_path), \
         patch(
             "functions.docs.get_code_diff",
             return_value=("diff", ["crates/foo.rs"]),
         ), \
         patch(
             "functions.claude.subprocess.run",
             _mock_subprocess_run_for_claude(verdict, capture=capture),
         ):
        check_docs("BASE", "HEAD")
    cmd = capture["cmd"]
    # The verdict shape is enforced CLI-side, so prose answers cannot crash
    # the release.
    assert json.loads(_flag_value(cmd, "--json-schema")) == _CHECK_SCHEMA
    # tools (not just the allowlist) is restricted: under bypassPermissions
    # the allowlist approves rather than limits, and the check must stay
    # read-only.
    assert _flag_value(cmd, "--tools") == "Read Grep Glob"
    assert _flag_value(cmd, "--allowed-tools") == "Read Grep Glob"


def test_check_docs_raises_on_claude_nonzero(tmp_path: Path) -> None:
    mock = MagicMock()
    mock.returncode = 2
    mock.stdout = ""
    mock.stderr = "boom"
    with patch("functions.docs.get_repo_root", return_value=tmp_path), \
         patch(
             "functions.docs.get_code_diff",
             return_value=("diff", ["crates/foo.rs"]),
         ), \
         patch("functions.claude.subprocess.run", return_value=mock):
        with pytest.raises(ReleaseError, match="claude CLI failed"):
            check_docs("BASE", "HEAD")


def test_check_docs_raises_on_missing_structured_output(tmp_path: Path) -> None:
    # The regression that motivated --json-schema: a prose answer must fail
    # with a diagnosable error, never a JSON traceback.
    mock = MagicMock()
    mock.returncode = 0
    mock.stdout = json.dumps({"type": "result", "result": "The docs are fine."})
    mock.stderr = ""
    with patch("functions.docs.get_repo_root", return_value=tmp_path), \
         patch(
             "functions.docs.get_code_diff",
             return_value=("diff", ["crates/foo.rs"]),
         ), \
         patch("functions.claude.subprocess.run", return_value=mock):
        with pytest.raises(ReleaseError, match="missing 'structured_output'"):
            check_docs("BASE", "HEAD")


def test_update_docs_rejects_an_empty_change_list(tmp_path: Path) -> None:
    with patch("functions.docs.get_repo_root", return_value=tmp_path), \
         patch("functions.claude.subprocess.run") as mock_run:
        with pytest.raises(ReleaseError, match="no changes to implement"):
            update_docs("BASE", "HEAD", ())
    mock_run.assert_not_called()


def test_update_docs_scopes_the_prompt_to_the_requested_changes(
    tmp_path: Path,
) -> None:
    capture: dict[str, Any] = {}
    report = {
        "results": [
            {"file": "docs/x.mdx", "change": "add flag", "status": "implemented"}
        ],
        "summary": "edited 1 file",
    }
    changes = (_blocking(), _blocking("docs/z.mdx", "document subcommand"))
    with patch("functions.docs.get_repo_root", return_value=tmp_path), \
         patch(
             "functions.docs.get_code_diff",
             return_value=("THE-CODE-DIFF", ["crates/foo.rs"]),
         ), \
         patch(
             "functions.claude.subprocess.run",
             _mock_subprocess_run_for_claude(report, capture=capture),
         ):
        result = update_docs("BASE", "HEAD", changes)

    assert result.summary == "edited 1 file"
    prompt = capture["input"]
    # The updater implements the check's verdict, nothing else: every
    # requested change is enumerated, and the diff stays in as the source of
    # truth for the facts.
    assert "- `docs/x.mdx`: add flag" in prompt
    assert "- `docs/z.mdx`: document subcommand" in prompt
    assert "THE-CODE-DIFF" in prompt
    assert json.loads(_flag_value(capture["cmd"], "--json-schema")) == _UPDATE_SCHEMA


def test_update_docs_invokes_claude_with_edit_permissions(tmp_path: Path) -> None:
    capture: dict[str, Any] = {}
    report: dict = {"results": [], "summary": "edited 3 files"}
    with patch("functions.docs.get_repo_root", return_value=tmp_path), \
         patch(
             "functions.docs.get_code_diff",
             return_value=("diff", ["crates/foo.rs"]),
         ), \
         patch(
             "functions.claude.subprocess.run",
             _mock_subprocess_run_for_claude(report, capture=capture),
         ):
        result = update_docs("BASE", "HEAD", (_blocking(),))

    assert result.summary == "edited 3 files"
    cmd = capture["cmd"]
    assert cmd[0] == "claude"
    assert _flag_value(cmd, "--permission-mode") == "acceptEdits"
    assert _flag_value(cmd, "--tools") == "Read Edit Write Grep Glob"
    allowed = _flag_value(cmd, "--allowed-tools")
    assert "Edit" in allowed
    assert "Write" in allowed


# --- pinned model / effort (reproducibility) ---


def _capture_claude_cmd(
    fn, tmp_path: Path, structured: object
) -> list[str]:
    """Invoke ``fn()`` with claude mocked and return its argv."""
    capture: dict[str, Any] = {}
    with patch("functions.docs.get_repo_root", return_value=tmp_path), \
         patch(
             "functions.docs.get_code_diff",
             return_value=("diff", ["crates/foo.rs"]),
         ), \
         patch(
             "functions.claude.subprocess.run",
             _mock_subprocess_run_for_claude(structured, capture=capture),
         ):
        fn()
    return capture["cmd"]


def test_check_docs_pins_model_and_effort(tmp_path: Path) -> None:
    cmd = _capture_claude_cmd(
        lambda: check_docs("BASE", "HEAD"),
        tmp_path,
        {"required_changes": []},
    )
    assert _flag_value(cmd, "--model") == CLAUDE_MODEL
    assert _flag_value(cmd, "--effort") == CLAUDE_EFFORT


def test_update_docs_pins_model_and_effort(tmp_path: Path) -> None:
    cmd = _capture_claude_cmd(
        lambda: update_docs("BASE", "HEAD", (_blocking(),)),
        tmp_path,
        {"results": [], "summary": "edited 1 file"},
    )
    assert _flag_value(cmd, "--model") == CLAUDE_MODEL
    assert _flag_value(cmd, "--effort") == CLAUDE_EFFORT
