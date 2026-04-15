"""Tests for functions.docs module."""

from __future__ import annotations

import json
import subprocess
from pathlib import Path
from unittest.mock import MagicMock, patch

import pytest

from functions.cli import ReleaseError
from functions.docs import (
    CheckResult,
    RequiredChange,
    _is_code_path,
    _parse_check_response,
    _strip_code_fence,
    _truncate_diff,
    check_docs,
    get_code_diff,
    update_docs,
)


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


# --- _strip_code_fence ---


def test_strip_code_fence_no_fence() -> None:
    assert _strip_code_fence('{"up_to_date": true}') == '{"up_to_date": true}'


def test_strip_code_fence_with_lang() -> None:
    text = '```json\n{"up_to_date": true}\n```'
    assert _strip_code_fence(text) == '{"up_to_date": true}'


def test_strip_code_fence_bare() -> None:
    text = '```\n{"up_to_date": true}\n```'
    assert _strip_code_fence(text) == '{"up_to_date": true}'


def test_strip_code_fence_with_surrounding_whitespace() -> None:
    text = '  \n```json\n{"up_to_date": true}\n```\n  '
    assert _strip_code_fence(text) == '{"up_to_date": true}'


# --- _parse_check_response ---


def test_parse_check_response_up_to_date() -> None:
    result = _parse_check_response('{"up_to_date": true, "required_changes": []}')
    assert result == CheckResult(up_to_date=True, required_changes=())


def test_parse_check_response_stale() -> None:
    text = json.dumps(
        {
            "up_to_date": False,
            "required_changes": [
                {"file": "docs/src/content/docs/guides/installation.mdx",
                 "change": "mention new --verbose flag"},
            ],
        }
    )
    result = _parse_check_response(text)
    assert result.up_to_date is False
    assert result.required_changes == (
        RequiredChange(
            file="docs/src/content/docs/guides/installation.mdx",
            change="mention new --verbose flag",
        ),
    )


def test_parse_check_response_with_code_fence() -> None:
    text = '```json\n{"up_to_date": true, "required_changes": []}\n```'
    result = _parse_check_response(text)
    assert result.up_to_date is True


def test_parse_check_response_invalid_json() -> None:
    with pytest.raises(ReleaseError, match="not valid JSON"):
        _parse_check_response("not json at all")


def test_parse_check_response_missing_up_to_date() -> None:
    with pytest.raises(ReleaseError, match="up_to_date"):
        _parse_check_response('{"required_changes": []}')


def test_parse_check_response_up_to_date_not_bool() -> None:
    with pytest.raises(ReleaseError, match="up_to_date"):
        _parse_check_response('{"up_to_date": "yes", "required_changes": []}')


def test_parse_check_response_changes_not_list() -> None:
    with pytest.raises(ReleaseError, match="required_changes"):
        _parse_check_response('{"up_to_date": false, "required_changes": "foo"}')


def test_parse_check_response_change_entry_missing_fields() -> None:
    text = '{"up_to_date": false, "required_changes": [{"file": "a.md"}]}'
    with pytest.raises(ReleaseError, match="file/change"):
        _parse_check_response(text)


def test_parse_check_response_not_object() -> None:
    with pytest.raises(ReleaseError, match="must be an object"):
        _parse_check_response("[]")


# --- get_code_diff (with real git repo) ---


def _commit_file(repo: Path, path: str, content: str, message: str) -> str:
    target = repo / path
    target.parent.mkdir(parents=True, exist_ok=True)
    target.write_text(content)
    subprocess.run(["git", "add", path], cwd=repo, check=True, capture_output=True)
    subprocess.run(
        ["git", "commit", "-m", message],
        cwd=repo,
        check=True,
        capture_output=True,
    )
    result = subprocess.run(
        ["git", "rev-parse", "HEAD"],
        cwd=repo,
        check=True,
        capture_output=True,
        text=True,
    )
    return result.stdout.strip()


def test_get_code_diff_code_change(tmp_repo: Path) -> None:
    base = subprocess.run(
        ["git", "rev-parse", "HEAD"],
        cwd=tmp_repo,
        capture_output=True,
        text=True,
        check=True,
    ).stdout.strip()
    head = _commit_file(tmp_repo, "crates/foo/src/lib.rs", "fn main() {}\n", "add lib")

    diff, paths = get_code_diff(base, head, tmp_repo)
    assert paths == ["crates/foo/src/lib.rs"]
    assert "fn main" in diff


def test_get_code_diff_docs_only_excluded(tmp_repo: Path) -> None:
    base = subprocess.run(
        ["git", "rev-parse", "HEAD"],
        cwd=tmp_repo,
        capture_output=True,
        text=True,
        check=True,
    ).stdout.strip()
    head = _commit_file(
        tmp_repo,
        "docs/src/content/docs/guides/new.mdx",
        "# new guide\n",
        "docs only",
    )

    diff, paths = get_code_diff(base, head, tmp_repo)
    assert paths == []
    assert diff == ""


def test_get_code_diff_mixed_filters_docs(tmp_repo: Path) -> None:
    base = subprocess.run(
        ["git", "rev-parse", "HEAD"],
        cwd=tmp_repo,
        capture_output=True,
        text=True,
        check=True,
    ).stdout.strip()
    (tmp_repo / "crates").mkdir(parents=True, exist_ok=True)
    (tmp_repo / "crates" / "foo.rs").write_text("fn f() {}\n")
    (tmp_repo / "docs" / "src").mkdir(parents=True, exist_ok=True)
    (tmp_repo / "docs" / "src" / "a.md").write_text("hi\n")
    subprocess.run(["git", "add", "."], cwd=tmp_repo, check=True, capture_output=True)
    subprocess.run(
        ["git", "commit", "-m", "mixed"],
        cwd=tmp_repo,
        check=True,
        capture_output=True,
    )
    head = subprocess.run(
        ["git", "rev-parse", "HEAD"],
        cwd=tmp_repo,
        capture_output=True,
        text=True,
        check=True,
    ).stdout.strip()

    diff, paths = get_code_diff(base, head, tmp_repo)
    assert paths == ["crates/foo.rs"]
    assert "fn f" in diff
    assert "hi" not in diff


def test_get_code_diff_bad_ref_raises(tmp_repo: Path) -> None:
    with pytest.raises(ReleaseError, match="git diff"):
        get_code_diff("does-not-exist", "HEAD", tmp_repo)


# --- check_docs / update_docs (mocked claude) ---


def _mock_subprocess_run_for_claude(
    result_text: str, returncode: int = 0
) -> MagicMock:
    """Build a MagicMock replacement for subprocess.run.

    Returns a dummy for git calls (empty stdout, rc=0) and the provided
    JSON payload for claude calls.
    """

    def _run(cmd: list[str], *args: object, **kwargs: object) -> MagicMock:
        if cmd and cmd[0] == "claude":
            payload = json.dumps({"type": "result", "result": result_text})
            mock = MagicMock()
            mock.returncode = returncode
            mock.stdout = payload
            mock.stderr = ""
            return mock
        raise AssertionError(f"unexpected command: {cmd}")

    return MagicMock(side_effect=_run)


def test_check_docs_short_circuits_on_no_code_changes(tmp_repo: Path) -> None:
    with patch("functions.docs.get_repo_root", return_value=tmp_repo), \
         patch("functions.docs.get_code_diff", return_value=("", [])), \
         patch("functions.docs.subprocess.run") as mock_run:
        result = check_docs("BASE", "HEAD")
    assert result.up_to_date is True
    assert result.required_changes == ()
    mock_run.assert_not_called()


def test_check_docs_parses_claude_verdict(tmp_repo: Path) -> None:
    verdict = '{"up_to_date": false, "required_changes": [' \
              '{"file": "docs/x.mdx", "change": "add flag"}]}'
    with patch("functions.docs.get_repo_root", return_value=tmp_repo), \
         patch(
             "functions.docs.get_code_diff",
             return_value=("diff", ["crates/foo.rs"]),
         ), \
         patch(
             "functions.docs.subprocess.run",
             _mock_subprocess_run_for_claude(verdict),
         ):
        result = check_docs("BASE", "HEAD")
    assert result.up_to_date is False
    assert result.required_changes == (
        RequiredChange(file="docs/x.mdx", change="add flag"),
    )


def test_check_docs_raises_on_claude_nonzero(tmp_repo: Path) -> None:
    mock = MagicMock()
    mock.returncode = 2
    mock.stdout = ""
    mock.stderr = "boom"
    with patch("functions.docs.get_repo_root", return_value=tmp_repo), \
         patch(
             "functions.docs.get_code_diff",
             return_value=("diff", ["crates/foo.rs"]),
         ), \
         patch("functions.docs.subprocess.run", return_value=mock):
        with pytest.raises(ReleaseError, match="claude CLI failed"):
            check_docs("BASE", "HEAD")


def test_check_docs_raises_on_invalid_outer_json(tmp_repo: Path) -> None:
    mock = MagicMock()
    mock.returncode = 0
    mock.stdout = "not json"
    mock.stderr = ""
    with patch("functions.docs.get_repo_root", return_value=tmp_repo), \
         patch(
             "functions.docs.get_code_diff",
             return_value=("diff", ["crates/foo.rs"]),
         ), \
         patch("functions.docs.subprocess.run", return_value=mock):
        with pytest.raises(ReleaseError, match="not return valid JSON"):
            check_docs("BASE", "HEAD")


def test_update_docs_short_circuits_on_no_code_changes(tmp_repo: Path) -> None:
    with patch("functions.docs.get_repo_root", return_value=tmp_repo), \
         patch("functions.docs.get_code_diff", return_value=("", [])), \
         patch("functions.docs.subprocess.run") as mock_run:
        summary = update_docs("BASE", "HEAD")
    assert "nothing to update" in summary
    mock_run.assert_not_called()


def test_update_docs_invokes_claude_with_edit_permissions(tmp_repo: Path) -> None:
    captured: dict[str, list[str]] = {}

    def _run(cmd: list[str], *args: object, **kwargs: object) -> MagicMock:
        captured["cmd"] = cmd
        mock = MagicMock()
        mock.returncode = 0
        mock.stdout = json.dumps({"type": "result", "result": "edited 3 files"})
        mock.stderr = ""
        return mock

    with patch("functions.docs.get_repo_root", return_value=tmp_repo), \
         patch(
             "functions.docs.get_code_diff",
             return_value=("diff", ["crates/foo.rs"]),
         ), \
         patch("functions.docs.subprocess.run", side_effect=_run):
        summary = update_docs("BASE", "HEAD")

    assert summary == "edited 3 files"
    cmd = captured["cmd"]
    assert cmd[0] == "claude"
    assert "--permission-mode" in cmd
    assert cmd[cmd.index("--permission-mode") + 1] == "acceptEdits"
    assert "--allowed-tools" in cmd
    allowed = cmd[cmd.index("--allowed-tools") + 1]
    assert "Edit" in allowed
    assert "Write" in allowed
