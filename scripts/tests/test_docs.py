"""Tests for functions.docs module."""

from __future__ import annotations

import json
import re
import subprocess
import threading
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
    _CONFIRM_PROMPT,
    _CONFIRM_SCHEMA,
    _PART_SCOPE,
    _REWORD_PROMPT,
    _UPDATE_PROMPT,
    _UPDATE_SCHEMA,
    EM_DASH,
    CheckResult,
    DiffPart,
    EmDashLine,
    GapReview,
    RequiredChange,
    UpdateOutcome,
    UpdateResult,
    _em_dash_lines,
    _is_code_path,
    _parse_check_response,
    _parse_confirm_response,
    _parse_update_response,
    check_docs,
    get_code_diff,
    get_docs_diff,
    split_diff,
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


def _gap(file: str, change: str, severity: str) -> dict:
    """One entry of a check verdict, as claude returns it."""
    return {"file": file, "change": change, "severity": severity}


# The second opinions on a blocking gap, as claude returns them.
_CONFIRMED: dict = {"verdict": "confirmed", "evidence": "the code agrees"}
_REFUTED: dict = {"verdict": "refuted", "evidence": "foo.rs:12 accepts any key"}


def _is_gap_review(cmd: list[str]) -> bool:
    """True when *cmd* is the claude run confirming a gap, not one judging a diff."""
    return json.loads(_flag_value(cmd, "--json-schema")) == _CONFIRM_SCHEMA


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
        # Lock files, wherever they sit.
        ("Cargo.lock", False),
        ("public-peppy-libs/peppy-shared/Cargo.lock", False),
        ("public-peppy-libs/so101_description/uv.lock", False),
        ("scripts/pixi.lock", False),
        ("tools/package-lock.json", False),
        # peppy's own test suite.
        ("scripts/tests/test_docs.py", False),
        ("crates/peppy/tests/cli.rs", False),
        ("crates/generator-internal/src/generator/python/tests/golden.rs", False),
        ("public-peppy-libs/srs_model/tests/fixtures/parity_v10_left.txt", False),
        ("crates/core-node-internal/src/services/tests.rs", False),
        (
            "public-peppy-libs/peppy-shared/peppylib-rs/src/messaging/deadline_tests.rs",
            False,
        ),
        # Templates land in users' projects, their tests included.
        (
            "crates/core-node-internal/templates/node_init/python/tests/test_smoke.py.j2",
            True,
        ),
        ("crates/core-node-internal/templates/node_init/rust/tests/smoke.rs.j2", True),
        # The test surfaces users write their tests against.
        ("public-peppy-libs/peppy-shared/peppylib-rs/src/testing.rs", True),
        (
            "public-peppy-libs/peppy-shared/peppy-messaging-interface/src/adapters/mock.rs",
            True,
        ),
        ("crates/generator-internal/src/generator/rust/testing.rs", True),
        # Only whole names count.
        ("crates/peppy/src/contests.rs", True),
        ("crates/peppy/src/latests/mod.rs", True),
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
    "crates/peppy/tests/cli.rs\n"
    "crates/generator-internal/templates/peppygen/python/peppygen/clock.py\n"
    "public-peppy-libs/peppy-shared/Cargo.lock\n"
    "docs/astro.config.mjs\n"
    "docs/src/content/docs/advanced_guides/testing.mdx\n"
    "docs/src/content/docs/guides/snippets/python/hello_world/uv.lock\n"
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
    # Embedded snippets are part of the rendered pages, but not their lock
    # files; release notes and the site configuration are not user
    # documentation.
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


def test_confirm_schema_pins_required_fields_and_verdict_enum() -> None:
    assert _CONFIRM_SCHEMA["required"] == ["verdict", "evidence"]
    assert _CONFIRM_SCHEMA["properties"]["verdict"]["enum"] == [
        "confirmed",
        "refuted",
    ]


@pytest.mark.parametrize(
    "prompt",
    [_CHECK_PROMPT, _CONFIRM_PROMPT, _UPDATE_PROMPT, _REWORD_PROMPT, _PART_SCOPE],
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


def test_parse_check_response_stamps_the_diff_part() -> None:
    result = _parse_check_response(
        {"required_changes": [_gap("docs/x.mdx", "add flag", "blocking")]},
        diff_part=3,
    )
    assert result.changes[0].diff_part == 3


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


# --- _parse_confirm_response ---


def test_parse_confirm_response_reads_both_verdicts() -> None:
    assert _parse_confirm_response(_CONFIRMED) == GapReview(
        confirmed=True, evidence="the code agrees"
    )
    assert _parse_confirm_response(_REFUTED) == GapReview(
        confirmed=False, evidence="foo.rs:12 accepts any key"
    )


def test_parse_confirm_response_unknown_verdict() -> None:
    with pytest.raises(ReleaseError, match="unknown verdict"):
        _parse_confirm_response({"verdict": "maybe", "evidence": "x"})


def test_parse_confirm_response_missing_evidence() -> None:
    with pytest.raises(ReleaseError, match="missing string 'evidence'"):
        _parse_confirm_response({"verdict": "confirmed"})


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


# --- split_diff ---


def _diff_of(*paths: str, body: str = "-old\n+new\n") -> str:
    """A unified diff changing each of *paths*, *body* being the hunk lines."""
    return "".join(
        f"diff --git a/{path} b/{path}\n--- a/{path}\n+++ b/{path}\n"
        f"@@ -1 +1 @@\n{body}"
        for path in paths
    )


def _header_of(path: str) -> str:
    """The lines of a file's section before its first hunk."""
    return f"diff --git a/{path} b/{path}\n--- a/{path}\n+++ b/{path}\n"


def test_split_diff_is_one_part_when_the_diff_fits() -> None:
    diff = _diff_of("crates/a.rs", "crates/b.rs")

    assert split_diff(diff, cap=len(diff)) == (
        DiffPart(text=diff, paths=("crates/a.rs", "crates/b.rs")),
    )


def test_split_diff_keeps_whole_files_together_in_order() -> None:
    diff = _diff_of("crates/a.rs", "crates/b.rs", "crates/c.rs")
    section = _diff_of("crates/a.rs")

    parts = split_diff(diff, cap=2 * len(section))

    assert parts == (
        DiffPart(
            text=_diff_of("crates/a.rs", "crates/b.rs"),
            paths=("crates/a.rs", "crates/b.rs"),
        ),
        DiffPart(text=_diff_of("crates/c.rs"), paths=("crates/c.rs",)),
    )


def test_split_diff_cuts_an_oversized_file_between_lines_under_its_header() -> None:
    lines = [f"+line {i:02d} of the file\n" for i in range(6)]
    body = "@@ -1 +1 @@\n" + "".join(lines)
    header = _header_of("crates/big.rs")
    cap = len(header) + 45

    parts = split_diff(header + body, cap=cap)

    # Two lines per piece fit the budget, the hunk line counting as one, and
    # every piece is led by the header so its lines stay attributed.
    assert len(parts) == 4
    assert all(part.paths == ("crates/big.rs",) for part in parts)
    assert all(part.text.startswith(header) for part in parts)
    assert all(len(part.text) <= cap for part in parts)
    assert "".join(part.text[len(header) :] for part in parts) == body


def test_split_diff_cuts_a_line_past_the_cap_within_the_line() -> None:
    body = "@@ -1 +1 @@\n+" + "x" * 99 + "\n"
    header = _header_of("crates/blob.rs")
    cap = len(header) + 40

    parts = split_diff(header + body, cap=cap)

    # The hunk line, then the long line in slices of the budget: nothing lost.
    assert len(parts) == 4
    assert all(len(part.text) <= cap for part in parts)
    assert "".join(part.text[len(header) :] for part in parts) == body


def test_split_diff_reads_a_quoted_path() -> None:
    diff = (
        'diff --git "a/crates/sp ace.rs" "b/crates/sp ace.rs"\n'
        '--- "a/crates/sp ace.rs"\n+++ "b/crates/sp ace.rs"\n'
        "@@ -1 +1 @@\n-old\n+new\n"
    )

    assert split_diff(diff, cap=len(diff))[0].paths == ("crates/sp ace.rs",)


def test_split_diff_of_nothing_is_no_part() -> None:
    assert split_diff("", cap=10) == ()


def test_split_diff_refuses_text_outside_a_file_section() -> None:
    with pytest.raises(ReleaseError, match="does not open with a 'diff --git'"):
        split_diff("not a diff\n" + _diff_of("crates/a.rs"), cap=1000)


# --- check_docs / update_docs (mocked claude) ---


def _claude_answer(structured: object) -> MagicMock:
    """A successful claude run whose envelope carries *structured*."""
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


class _ClaudeCalls:
    """A subprocess.run stand-in answering each claude call in turn.

    For runs made one after another (the updater's, or a check of a single
    part); the check's parts are answered by `_ClaudeByPath`. The prompt of
    every call is appended to ``prompts``; the em-dash scan of ``docs/`` (the
    one git call left once get_code_diff is patched) finds nothing.
    """

    def __init__(self, *answers: object) -> None:
        self._answers = iter(answers)
        self.prompts: list[str] = []

    def __call__(self, cmd: list[str], *args: object, **kwargs: object) -> MagicMock:
        if cmd[:2] == ["git", "grep"]:
            mock = MagicMock()
            mock.returncode = 1
            mock.stdout = ""
            mock.stderr = ""
            return mock
        if cmd and cmd[0] == "claude":
            self.prompts.append(str(kwargs.get("input")))
            structured = next(self._answers, None)
            assert structured is not None, "claude called more often than answered"
            return _claude_answer(structured)
        raise AssertionError(f"unexpected command: {cmd}")


# The longest a fake claude run waits at a rendezvous for the other runs. Only
# a check that stops running parts at once ever waits this long: the guard
# turns that hang into a failure and never decides a passing run.
_HANG_GUARD_SECONDS = 30.0


class _ClaudeByPath:
    """A subprocess.run stand-in answering each check run by the file it judges.

    The check judges parts concurrently, so its runs arrive in no set order;
    answering by the file a part shows keeps every verdict with its part.
    Every part must show exactly one file of *answers*. A run judging a file
    in *failing* exits non-zero, naming the file. When *rendezvous* is given,
    every run waits there before answering, so none answers until that many
    runs are in flight at once.

    ``prompts`` maps each judged file to its prompt; ``judged`` lists the
    files in the order their runs started.

    The run confirming a blocking gap is answered by the doc file the gap
    names: *reviews* maps a doc file to its second opinion, and a gap in any
    other doc file is confirmed. ``review_prompts`` lists the prompts of those
    runs.
    """

    def __init__(
        self,
        answers: dict[str, object],
        *,
        failing: frozenset[str] = frozenset(),
        rendezvous: threading.Barrier | None = None,
        reviews: dict[str, object] | None = None,
    ) -> None:
        self._answers = answers
        self._failing = failing
        self._rendezvous = rendezvous
        self._reviews = reviews or {}
        self.prompts: dict[str, str] = {}
        self.judged: list[str] = []
        self.review_prompts: list[str] = []

    def _review(self, prompt: str) -> MagicMock:
        self.review_prompts.append(prompt)
        named = [file for file in self._reviews if f"`{file}`" in prompt]
        assert len(named) <= 1, f"a review must name one doc file: {named}"
        return _claude_answer(self._reviews[named[0]] if named else _CONFIRMED)

    def __call__(self, cmd: list[str], *args: object, **kwargs: object) -> MagicMock:
        assert cmd and cmd[0] == "claude", f"unexpected command: {cmd}"
        prompt = str(kwargs.get("input"))
        if _is_gap_review(cmd):
            return self._review(prompt)
        files = [f for f in self._answers if f"diff --git a/{f} b/{f}\n" in prompt]
        assert len(files) == 1, f"a part must show one answered file: {files}"
        (file,) = files
        self.prompts[file] = prompt
        self.judged.append(file)
        if self._rendezvous is not None:
            self._rendezvous.wait(timeout=_HANG_GUARD_SECONDS)
        if file in self._failing:
            mock = MagicMock()
            mock.returncode = 2
            mock.stdout = ""
            mock.stderr = f"{file} broke"
            return mock
        return _claude_answer(self._answers[file])


def _mock_subprocess_run_for_claude(
    structured: object,
    *,
    capture: dict[str, Any] | None = None,
) -> MagicMock:
    """Build a MagicMock replacement for subprocess.run.

    Returns the provided object as the envelope's structured output for
    claude calls, and confirms every blocking gap the check asks a second
    opinion on. get_code_diff is patched, so the only git call is the em-dash
    scan of ``docs/``, which finds nothing.
    """

    def _run(cmd: list[str], *args: object, **kwargs: object) -> MagicMock:
        if cmd[:2] == ["git", "grep"]:
            mock = MagicMock()
            mock.returncode = 1
            mock.stdout = ""
            mock.stderr = ""
            return mock
        if cmd and cmd[0] == "claude":
            if _is_gap_review(cmd):
                return _claude_answer(_CONFIRMED)
            if capture is not None:
                capture["cmd"] = cmd
                capture["input"] = kwargs.get("input")
            return _claude_answer(structured)
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
             return_value=(_diff_of("crates/foo.rs"), ["crates/foo.rs"]),
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
    diff = _diff_of("crates/a.rs", "crates/b.rs", body="+" + "x" * 3000 + "\n")

    _check_docs_with_diff(tmp_path, diff, ["crates/a.rs", "crates/b.rs"])

    err = " ".join(capfd.readouterr().err.split())
    assert (
        f"2 changed code path(s), {len(diff) // 1024} KB of diff, judged in 1 part(s)."
        in err
    )
    assert "Asking Claude to judge the diff (2 path(s)," in err


def test_check_docs_keeps_the_whole_diff_in_one_run_when_it_fits(
    tmp_path: Path,
) -> None:
    calls = _ClaudeCalls({"required_changes": []})
    diff = _diff_of("crates/a.rs", "crates/b.rs")
    with patch("functions.docs.get_repo_root", return_value=tmp_path), \
         patch(
             "functions.docs.get_code_diff",
             return_value=(diff, ["crates/a.rs", "crates/b.rs"]),
         ), \
         patch("functions.claude.subprocess.run", calls):
        check_docs("BASE", "HEAD")

    (prompt,) = calls.prompts
    assert diff in prompt
    assert "part 1 of" not in prompt


def _patched_parts(
    tmp_path: Path,
    claude: Callable[..., MagicMock],
    paths: tuple[str, ...] = ("crates/a.rs", "crates/b.rs"),
):
    """Patch the repo, the diff and claude so each of *paths* is a part of its own.

    The paths are the same length, so their diff sections are too, and a cap
    of one section cuts the diff into one part per path, in order.
    """
    return (
        patch("functions.docs.get_repo_root", return_value=tmp_path),
        patch(
            "functions.docs.get_code_diff",
            return_value=(_diff_of(*paths), list(paths)),
        ),
        patch("functions.docs._MAX_DIFF_CHARS", len(_diff_of(paths[0]))),
        patch("functions.claude.subprocess.run", claude),
    )


_NO_GAP: dict = {"required_changes": []}


def test_check_docs_judges_a_large_diff_in_parts(
    tmp_path: Path, capfd: pytest.CaptureFixture[str]
) -> None:
    calls = _ClaudeByPath(
        {
            "crates/a.rs": {"required_changes": [_gap("docs/a.mdx", "say a", "blocking")]},
            "crates/b.rs": {"required_changes": [_gap("docs/b.mdx", "say b", "minor")]},
        }
    )
    repo, diff, cap, claude = _patched_parts(tmp_path, calls)
    with repo, diff, cap, claude:
        result = check_docs("BASE", "HEAD")

    # Every verdict comes back in part order, stamped with the part it was
    # judged on.
    assert result.changes == (
        RequiredChange("docs/a.mdx", "say a", "blocking", diff_part=1),
        RequiredChange("docs/b.mdx", "say b", "minor", diff_part=2),
    )
    first = calls.prompts["crates/a.rs"]
    second = calls.prompts["crates/b.rs"]
    # Each run sees one part, told it is one, listing that part's paths only.
    assert "part 1 of 2 of the whole change set" in first
    assert _diff_of("crates/a.rs") in first
    assert "crates/b.rs" not in first
    assert "part 2 of 2 of the whole change set" in second
    assert _diff_of("crates/b.rs") in second
    assert "crates/a.rs" not in second
    err = " ".join(capfd.readouterr().err.split())
    assert "judged in 2 part(s)." in err
    assert "Asking Claude to judge part 1 of 2 of the diff (1 path(s)," in err
    assert "Asking Claude to judge part 2 of 2 of the diff (1 path(s)," in err
    assert "Judged part 1 of 2 of the diff: 1 gap(s)." in err
    assert "Judged part 2 of 2 of the diff: 1 gap(s)." in err


def test_check_docs_judges_the_parts_at_once(tmp_path: Path) -> None:
    # Neither run answers until both are in flight, so the check passes only
    # when it runs them at the same time.
    calls = _ClaudeByPath(
        {"crates/a.rs": _NO_GAP, "crates/b.rs": _NO_GAP},
        rendezvous=threading.Barrier(2),
    )
    repo, diff, cap, claude = _patched_parts(tmp_path, calls)
    with repo, diff, cap, claude:
        result = check_docs("BASE", "HEAD")

    assert result == CheckResult(changes=())
    assert sorted(calls.judged) == ["crates/a.rs", "crates/b.rs"]


def test_check_docs_starts_no_part_once_one_fails(tmp_path: Path) -> None:
    paths = ("crates/a.rs", "crates/b.rs", "crates/c.rs")
    calls = _ClaudeByPath(
        {path: _NO_GAP for path in paths}, failing=frozenset({"crates/a.rs"})
    )
    repo, diff, cap, claude = _patched_parts(tmp_path, calls, paths)
    # One run at a time: the run judging part 1 fails before any other starts.
    one_at_a_time = patch("functions.docs._CHECK_CONCURRENCY", 1)
    with repo, diff, cap, claude, one_at_a_time:
        with pytest.raises(ReleaseError, match="crates/a.rs broke"):
            check_docs("BASE", "HEAD")

    assert calls.judged == ["crates/a.rs"]


def test_check_docs_raises_the_failure_of_the_first_failing_part(
    tmp_path: Path,
) -> None:
    # Both runs are in flight before either fails, so both failures are in
    # hand whichever ends first; the one of the lower part is raised.
    calls = _ClaudeByPath(
        {"crates/a.rs": _NO_GAP, "crates/b.rs": _NO_GAP},
        failing=frozenset({"crates/a.rs", "crates/b.rs"}),
        rendezvous=threading.Barrier(2),
    )
    repo, diff, cap, claude = _patched_parts(tmp_path, calls)
    with repo, diff, cap, claude:
        with pytest.raises(ReleaseError, match="crates/a.rs broke"):
            check_docs("BASE", "HEAD")

    assert sorted(calls.judged) == ["crates/a.rs", "crates/b.rs"]


def test_check_docs_drops_a_blocking_gap_the_code_refutes(
    tmp_path: Path, capfd: pytest.CaptureFixture[str]
) -> None:
    # Two parts disagree about one behaviour, as when a stale comment sits in
    # one and the code deciding it in the other: only the gap the code
    # confirms survives, so the docs are never rewritten back and forth.
    calls = _ClaudeByPath(
        {
            "crates/a.rs": {
                "required_changes": [
                    _gap("docs/a.mdx", "keys are restricted", "blocking"),
                    _gap("docs/a.mdx", "reword the intro", "minor"),
                ]
            },
            "crates/b.rs": {
                "required_changes": [_gap("docs/b.mdx", "say b", "blocking")]
            },
        },
        reviews={"docs/a.mdx": _REFUTED},
    )
    repo, diff, cap, claude = _patched_parts(tmp_path, calls)
    with repo, diff, cap, claude:
        result = check_docs("BASE", "HEAD")

    assert result.blocking == (
        RequiredChange("docs/b.mdx", "say b", "blocking", diff_part=2),
    )
    # A refuted gap is dropped, not demoted: as a minor suggestion it would
    # still reach the updater of the polish pull request.
    assert result.minor == (
        RequiredChange("docs/a.mdx", "reword the intro", "minor", diff_part=1),
    )
    err = " ".join(capfd.readouterr().err.split())
    assert "Dropped a gap the code does not confirm:" in err
    assert "docs/a.mdx: keys are restricted" in err
    assert "foo.rs:12 accepts any key" in err
    assert "Confirmed the gap in docs/b.mdx." in err


def test_check_docs_confirms_a_gap_from_the_repository_not_the_diff(
    tmp_path: Path,
) -> None:
    calls = _ClaudeByPath(
        {
            "crates/a.rs": _NO_GAP,
            "crates/b.rs": {
                "required_changes": [_gap("docs/b.mdx", "say b", "blocking")]
            },
        }
    )
    repo, diff, cap, claude = _patched_parts(tmp_path, calls)
    with repo, diff, cap, claude:
        check_docs("BASE", "HEAD")

    (prompt,) = calls.review_prompts
    assert "- doc file: `docs/b.mdx`" in prompt
    assert "- claim: say b" in prompt
    # The run is pointed at the files of the part that showed the gap, and
    # reads them as they stand: a diff would hand it the judge's blind spot.
    assert "crates/b.rs" in prompt
    assert "crates/a.rs" not in prompt
    assert "diff --git" not in prompt


def test_check_docs_asks_no_second_opinion_on_minor_gaps(tmp_path: Path) -> None:
    calls = _ClaudeByPath(
        {
            "crates/a.rs": {"required_changes": [_gap("docs/a.mdx", "reword", "minor")]},
            "crates/b.rs": _NO_GAP,
        }
    )
    repo, diff, cap, claude = _patched_parts(tmp_path, calls)
    with repo, diff, cap, claude:
        result = check_docs("BASE", "HEAD")

    assert result.minor == (RequiredChange("docs/a.mdx", "reword", "minor", diff_part=1),)
    assert calls.review_prompts == []


def test_check_docs_confirms_a_gap_reported_twice_once(tmp_path: Path) -> None:
    twice = [_gap("docs/a.mdx", "say a", "blocking")] * 2
    calls = _ClaudeByPath(
        {"crates/a.rs": {"required_changes": twice}, "crates/b.rs": _NO_GAP},
        reviews={"docs/a.mdx": _REFUTED},
    )
    repo, diff, cap, claude = _patched_parts(tmp_path, calls)
    with repo, diff, cap, claude:
        result = check_docs("BASE", "HEAD")

    # Equal gaps are one gap: one second opinion, which decides both.
    assert len(calls.review_prompts) == 1
    assert result == CheckResult(changes=())


def test_check_docs_confirms_gaps_with_the_schema_and_readonly_tools(
    tmp_path: Path,
) -> None:
    reviews: list[list[str]] = []

    def _run(cmd: list[str], *args: object, **kwargs: object) -> MagicMock:
        if not _is_gap_review(cmd):
            return _claude_answer(
                {"required_changes": [_gap("docs/x.mdx", "add flag", "blocking")]}
            )
        reviews.append(cmd)
        return _claude_answer(_CONFIRMED)

    with patch("functions.docs.get_repo_root", return_value=tmp_path), \
         patch(
             "functions.docs.get_code_diff",
             return_value=(_diff_of("crates/foo.rs"), ["crates/foo.rs"]),
         ), \
         patch("functions.claude.subprocess.run", MagicMock(side_effect=_run)):
        check_docs("BASE", "HEAD")

    (cmd,) = reviews
    # Read-only for the same reason the judging runs are.
    assert _flag_value(cmd, "--tools") == "Read Grep Glob"
    assert _flag_value(cmd, "--allowed-tools") == "Read Grep Glob"
    assert _flag_value(cmd, "--model") == CLAUDE_MODEL
    assert _flag_value(cmd, "--effort") == CLAUDE_EFFORT


def test_update_docs_hands_each_part_its_own_gaps(tmp_path: Path) -> None:
    calls = _ClaudeCalls(
        {
            "results": [
                {"file": "docs/a.mdx", "change": "say a", "status": "implemented"}
            ],
            "summary": "did a",
        },
        {
            "results": [
                {"file": "docs/b.mdx", "change": "say b", "status": "already_covered"}
            ],
            "summary": "b was there",
        },
    )
    changes = (
        RequiredChange("docs/a.mdx", "say a", "blocking", diff_part=1),
        RequiredChange("docs/b.mdx", "say b", "blocking", diff_part=2),
    )
    repo, diff, cap, claude = _patched_parts(tmp_path, calls)
    with repo, diff, cap, claude:
        result = update_docs("BASE", "HEAD", changes)

    assert result.results == (
        UpdateOutcome("docs/a.mdx", "say a", "implemented"),
        UpdateOutcome("docs/b.mdx", "say b", "already_covered"),
    )
    assert result.summary == "did a\nb was there"
    first, second = calls.prompts
    # A run gets the gaps its part shows, with that part as the facts.
    assert "- `docs/a.mdx`: say a" in first
    assert "docs/b.mdx" not in first
    assert _diff_of("crates/a.rs") in first
    assert "crates/b.rs" not in first
    assert "part 1 of 2 of the whole change set" in first
    assert "- `docs/b.mdx`: say b" in second
    assert "docs/a.mdx" not in second
    assert _diff_of("crates/b.rs") in second
    assert "part 2 of 2 of the whole change set" in second


def test_update_docs_skips_the_parts_with_no_gap(tmp_path: Path) -> None:
    calls = _ClaudeCalls(
        {
            "results": [
                {"file": "docs/b.mdx", "change": "say b", "status": "implemented"}
            ],
            "summary": "did b",
        }
    )
    changes = (RequiredChange("docs/b.mdx", "say b", "blocking", diff_part=2),)
    repo, diff, cap, claude = _patched_parts(tmp_path, calls)
    with repo, diff, cap, claude:
        result = update_docs("BASE", "HEAD", changes)

    assert result.summary == "did b"
    (prompt,) = calls.prompts
    assert "part 2 of 2 of the whole change set" in prompt


def test_update_docs_refuses_a_gap_naming_a_part_the_diff_lacks(
    tmp_path: Path,
) -> None:
    calls = _ClaudeCalls()
    changes = (RequiredChange("docs/b.mdx", "say b", "blocking", diff_part=3),)
    repo, diff, cap, claude = _patched_parts(tmp_path, calls)
    with repo, diff, cap, claude:
        with pytest.raises(ReleaseError, match="has 2 part\\(s\\), but these changes"):
            update_docs("BASE", "HEAD", changes)

    assert calls.prompts == []


def test_check_docs_enforces_schema_and_readonly_tools(tmp_path: Path) -> None:
    capture: dict[str, Any] = {}
    verdict: dict = {"required_changes": []}
    with patch("functions.docs.get_repo_root", return_value=tmp_path), \
         patch(
             "functions.docs.get_code_diff",
             return_value=(_diff_of("crates/foo.rs"), ["crates/foo.rs"]),
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
             return_value=(_diff_of("crates/foo.rs"), ["crates/foo.rs"]),
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
             return_value=(_diff_of("crates/foo.rs"), ["crates/foo.rs"]),
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
             return_value=(
                 _diff_of("crates/foo.rs", body="+THE-CODE-DIFF\n"),
                 ["crates/foo.rs"],
             ),
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
             return_value=(_diff_of("crates/foo.rs"), ["crates/foo.rs"]),
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
             return_value=(_diff_of("crates/foo.rs"), ["crates/foo.rs"]),
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
             return_value=(_diff_of("crates/foo.rs"), ["crates/foo.rs"]),
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
