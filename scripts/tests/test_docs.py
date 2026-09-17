"""Tests for functions.docs module."""

from __future__ import annotations

import json
import re
import subprocess
from collections.abc import Callable
from pathlib import Path
from typing import Any
from unittest.mock import MagicMock, patch

import pytest

from functions.claude import CLAUDE_EFFORT, CLAUDE_MODEL
from functions.cli import ReleaseError
from functions.docs import (
    _CHECK_PROMPT,
    _CHECK_SCHEMA,
    _MAX_DIFF_BYTES,
    _REWORD_PROMPT,
    _UPDATE_PROMPT,
    _UPDATE_SCHEMA,
    EM_DASH,
    CheckResult,
    EmDashLine,
    RequiredChange,
    UpdateOutcome,
    UpdateResult,
    _em_dash_lines,
    _is_code_path,
    _parse_check_response,
    _parse_update_response,
    check_docs,
    get_code_diff,
    get_docs_diff,
    truncate_diff,
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
        ("Cargo.lock", False),
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


# --- truncate_diff ---


def test_truncate_diff_short_unchanged() -> None:
    diff = "a" * 1000
    assert truncate_diff(diff) == diff


def test_truncate_diff_long_truncated() -> None:
    diff = "a" * 500_000
    out = truncate_diff(diff)
    assert len(out) < len(diff)
    assert "diff truncated" in out


# --- get_code_diff / get_docs_diff ---


def _mock_git_diff(names: str, diff: str = "DIFF", returncode: int = 0) -> MagicMock:
    """Build a subprocess.run replacement answering the two git diff calls.

    ``--name-only`` returns ``names``; the path-scoped diff returns ``diff``.
    """

    def _run(cmd: list[str], *args: object, **kwargs: object) -> MagicMock:
        assert cmd[:2] == ["git", "diff"], cmd
        mock = MagicMock()
        mock.returncode = returncode
        mock.stdout = names if "--name-only" in cmd else diff
        mock.stderr = "boom" if returncode else ""
        return mock

    return MagicMock(side_effect=_run)


_CHANGED = (
    "Cargo.lock\n"
    "crates/peppy/src/main.rs\n"
    "crates/generator-internal/templates/peppygen/python/peppygen/clock.py\n"
    "docs/astro.config.mjs\n"
    "docs/src/content/docs/advanced_guides/testing.mdx\n"
    "docs/src/content/docs/guides/snippets/rust/hello_receiver/src/lib.rs\n"
    "docs/src/content/releases/v0.25.1.html\n"
    ".github/workflows/ci.yml\n"
)


def test_get_code_diff_keeps_code_paths_only(tmp_path: Path) -> None:
    run = _mock_git_diff(_CHANGED)
    with patch("functions.docs.subprocess.run", run):
        diff, paths = get_code_diff("v0.1.0", "HEAD", tmp_path)
    assert diff == "DIFF"
    assert paths == [
        "crates/peppy/src/main.rs",
        "crates/generator-internal/templates/peppygen/python/peppygen/clock.py",
    ]
    # The diff itself is scoped to the kept paths, so excluded files never
    # reach the prompt even when they changed.
    assert run.call_args.args[0] == [
        "git",
        "diff",
        "v0.1.0..HEAD",
        "--",
        *paths,
    ]
    assert run.call_args.kwargs["cwd"] == tmp_path


def test_get_docs_diff_keeps_user_documentation_only(tmp_path: Path) -> None:
    run = _mock_git_diff(_CHANGED)
    with patch("functions.docs.subprocess.run", run):
        diff, paths = get_docs_diff("v0.1.0", "HEAD", tmp_path)
    assert diff == "DIFF"
    # Embedded snippets are part of the rendered pages; release notes and the
    # site configuration are not user documentation.
    assert paths == [
        "docs/src/content/docs/advanced_guides/testing.mdx",
        "docs/src/content/docs/guides/snippets/rust/hello_receiver/src/lib.rs",
    ]
    assert run.call_args.args[0] == ["git", "diff", "v0.1.0..HEAD", "--", *paths]


@pytest.mark.parametrize(
    "getter, names",
    [
        (get_code_diff, "Cargo.lock\ndocs/src/content/docs/x.mdx\n"),
        (get_docs_diff, "crates/peppy/src/main.rs\ndocs/astro.config.mjs\n"),
    ],
)
def test_diff_getters_skip_the_diff_when_nothing_matches(
    getter: object, names: str, tmp_path: Path
) -> None:
    run = _mock_git_diff(names)
    with patch("functions.docs.subprocess.run", run):
        assert getter("v0.1.0", "HEAD", tmp_path) == ("", [])  # type: ignore[operator]
    # Only the name listing ran: there is nothing to diff.
    assert run.call_count == 1
    assert "--name-only" in run.call_args.args[0]


def test_get_code_diff_raises_when_git_fails(tmp_path: Path) -> None:
    with patch("functions.docs.subprocess.run", _mock_git_diff("", returncode=128)):
        with pytest.raises(ReleaseError, match="git diff --name-only v9.9.9..HEAD failed"):
            get_code_diff("v9.9.9", "HEAD", tmp_path)


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


def test_check_schema_forbids_an_em_dash_in_a_gap_description() -> None:
    # The description is quoted in the docs pull request body and handed to
    # the updater as its instruction, so the CLI must reject an em-dash in it.
    item = _CHECK_SCHEMA["properties"]["required_changes"]["items"]
    pattern = item["properties"]["change"]["pattern"]
    assert re.fullmatch(pattern, "add the flag, then its default")
    assert re.fullmatch(pattern, f"add the flag {EM_DASH} then its default") is None


@pytest.mark.parametrize(
    "prompt", [_CHECK_PROMPT, _UPDATE_PROMPT, _REWORD_PROMPT]
)
def test_prompts_hold_no_em_dash(prompt: str) -> None:
    # Claude mirrors the style of what it reads.
    assert EM_DASH not in prompt


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


def test_parse_check_response_rejects_an_em_dash_in_a_change() -> None:
    payload = {
        "required_changes": [
            {
                "file": "docs/x.mdx",
                "change": f"add the flag {EM_DASH} and its default",
                "severity": "blocking",
            }
        ]
    }
    with pytest.raises(ReleaseError, match="em-dash"):
        _parse_check_response(payload)


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
    claude calls. get_code_diff is patched, so the only git call is the
    em-dash scan of ``docs/``, which finds nothing.
    """

    def _run(cmd: list[str], *args: object, **kwargs: object) -> MagicMock:
        if cmd[:2] == ["git", "grep"]:
            mock = MagicMock()
            mock.returncode = 1
            mock.stdout = ""
            mock.stderr = ""
            return mock
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


def _check_docs_with_diff(
    tmp_path: Path, diff: str, paths: list[str]
) -> CheckResult:
    """Run the check on a canned diff with Claude answering that all is covered."""
    with patch("functions.docs.get_repo_root", return_value=tmp_path), \
         patch("functions.docs.get_code_diff", return_value=(diff, paths)), \
         patch(
             "functions.claude.subprocess.run",
             _mock_subprocess_run_for_claude({"required_changes": []}),
         ):
        return check_docs("BASE", "HEAD")


def test_check_docs_says_how_much_claude_reads(
    tmp_path: Path, capfd: pytest.CaptureFixture[str]
) -> None:
    _check_docs_with_diff(tmp_path, "x" * (3 * 1024), ["crates/a.rs", "crates/b.rs"])

    err = " ".join(capfd.readouterr().err.split())
    assert "2 changed code path(s), 3 KB of diff" in err
    assert "not judged" not in err


def test_check_docs_warns_when_the_diff_is_cut_short(
    tmp_path: Path, capfd: pytest.CaptureFixture[str]
) -> None:
    _check_docs_with_diff(tmp_path, "x" * (_MAX_DIFF_BYTES + 1), ["crates/a.rs"])

    err = " ".join(capfd.readouterr().err.split())
    assert "the changes past that point are not judged" in err


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


# --- em-dashes added under docs/ (real git, mocked claude) ---


def _git(repo: Path, *args: str) -> None:
    subprocess.run(
        [
            "git",
            "-c",
            "user.name=test",
            "-c",
            "user.email=test@example.com",
            "-c",
            "commit.gpgsign=false",
            *args,
        ],
        cwd=repo,
        check=True,
        capture_output=True,
    )


def _docs_repo(tmp_path: Path) -> Path:
    """A git repository whose committed page already holds one em-dash line."""
    repo = tmp_path / "repo"
    page = repo / "docs" / "src" / "content" / "docs" / "page.mdx"
    page.parent.mkdir(parents=True)
    page.write_text(f"# Page\n\nAn old line {EM_DASH} kept as is.\n")
    (repo / ".gitignore").write_text("docs/node_modules/\n")
    _git(repo, "init", "-q")
    _git(repo, "add", ".")
    _git(repo, "commit", "-q", "-m", "init")
    return repo


_PAGE = "docs/src/content/docs/page.mdx"

_REAL_RUN = subprocess.run


def _claude_edits(
    repo: Path,
    edits: list[Callable[[Path], None]],
    prompts: list[str],
) -> MagicMock:
    """Answer each claude call by applying the next of *edits* to *repo*.

    Every other command runs for real, so the em-dash scan reads the edits
    from disk. Each prompt is recorded in *prompts*.
    """

    def _run(cmd: list[str], *args: object, **kwargs: object) -> Any:
        if cmd[0] != "claude":
            return _REAL_RUN(cmd, *args, **kwargs)
        prompts.append(str(kwargs.get("input")))
        edits.pop(0)(repo)
        # Answers both the update report and the rewording report: the
        # rewording caller reads nothing from it.
        structured = {"results": [], "summary": "done"}
        mock = MagicMock()
        mock.returncode = 0
        mock.stdout = json.dumps(
            {"type": "result", "result": "", "structured_output": structured}
        )
        mock.stderr = ""
        return mock

    return MagicMock(side_effect=_run)


def _append(relative: str, text: str) -> Callable[[Path], None]:
    def _edit(repo: Path) -> None:
        path = repo / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        with path.open("a") as f:
            f.write(text)

    return _edit


def _replace(relative: str, old: str, new: str) -> Callable[[Path], None]:
    def _edit(repo: Path) -> None:
        path = repo / relative
        path.write_text(path.read_text().replace(old, new))

    return _edit


def _update(repo: Path, edits: list[Callable[[Path], None]], prompts: list[str]):
    with patch("functions.docs.get_repo_root", return_value=repo), \
         patch(
             "functions.docs.get_code_diff",
             return_value=("diff", ["crates/foo.rs"]),
         ), \
         patch("subprocess.run", _claude_edits(repo, edits, prompts)):
        return update_docs("BASE", "HEAD", (_blocking(),))


def test_em_dash_lines_cover_tracked_and_untracked_docs_only(
    tmp_path: Path,
) -> None:
    repo = _docs_repo(tmp_path)
    (repo / "docs" / "new.mdx").write_text(f"plain\nnew {EM_DASH} line\n")
    ignored = repo / "docs" / "node_modules" / "dep.js"
    ignored.parent.mkdir()
    ignored.write_text(f"// ignored {EM_DASH} line\n")
    (repo / "docs" / "image.bin").write_bytes(
        b"\x00binary " + EM_DASH.encode() + b"\n"
    )
    (repo / "README.md").write_text(f"outside {EM_DASH} docs\n")

    assert set(_em_dash_lines(repo)) == {
        EmDashLine(file="docs/new.mdx", line=2, text=f"new {EM_DASH} line"),
        EmDashLine(file=_PAGE, line=3, text=f"An old line {EM_DASH} kept as is."),
    }


def test_em_dash_lines_is_empty_without_a_match(tmp_path: Path) -> None:
    repo = _docs_repo(tmp_path)
    (repo / _PAGE).write_text("# Page\n")
    assert _em_dash_lines(repo) == ()


def test_update_docs_without_an_added_em_dash_asks_claude_once(
    tmp_path: Path,
) -> None:
    repo = _docs_repo(tmp_path)
    prompts: list[str] = []
    result = _update(repo, [_append(_PAGE, "A new line, no dash.\n")], prompts)
    assert result.summary == "done"
    assert len(prompts) == 1


def test_update_docs_rewords_only_the_em_dashes_it_added(tmp_path: Path) -> None:
    repo = _docs_repo(tmp_path)
    prompts: list[str] = []
    added = f"The copy never starts {EM_DASH} loosen a constraint."
    _update(
        repo,
        [
            _append(_PAGE, f"{added}\n"),
            _replace(_PAGE, added, "The copy never starts: loosen a constraint."),
        ],
        prompts,
    )

    assert len(prompts) == 2
    reword = prompts[1]
    assert f"- `{_PAGE}` line 4: {added}" in reword
    # The committed em-dash is not the update's to reword.
    assert "An old line" not in reword
    assert "The copy never starts: loosen a constraint." in (repo / _PAGE).read_text()


def test_update_docs_does_not_count_a_moved_em_dash_line_as_added(
    tmp_path: Path,
) -> None:
    repo = _docs_repo(tmp_path)
    prompts: list[str] = []
    old = f"An old line {EM_DASH} kept as is."
    _update(
        repo,
        [_replace(_PAGE, old, f"A new first line.\n\n{old}")],
        prompts,
    )
    assert len(prompts) == 1


def test_update_docs_catches_an_em_dash_in_a_new_page(tmp_path: Path) -> None:
    repo = _docs_repo(tmp_path)
    prompts: list[str] = []
    new_page = "docs/src/content/docs/new.mdx"
    with pytest.raises(ReleaseError, match="em-dash") as raised:
        _update(
            repo,
            [
                _append(new_page, f"# New {EM_DASH} page\n"),
                lambda _repo: None,
            ],
            prompts,
        )
    assert len(prompts) == 2
    assert f"- `{new_page}` line 1: # New {EM_DASH} page" in prompts[1]
    assert f"`{new_page}` line 1" in str(raised.value)
