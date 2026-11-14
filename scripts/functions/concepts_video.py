"""The concepts video of the docs, checked against the docs a release ships.

`docs/concepts_video` is the project of the video the quickstart page shows.
Its transcript is the only source of what the video says and shows, and each
segment of it names the docs sections it rests on, with the hash of the text
of each one. Three steps hold the transcript to the docs:

- ``check_video``: the project's own ``docs-check`` command. It compares text
  and hashes and calls no model. For each segment it reports the named
  sections whose text is neither the text the transcript records nor the text
  of the last release, the named sections that do not exist, and the
  technical names the segment shows that its sections do not hold.
- ``update_video``: Claude reads each reported segment against its sections
  and corrects the transcript where it states what the docs no longer state.
- ``record_sections``: the project's own ``record-sections`` command, which
  writes into the transcript the hash of the present text of every section
  it names, so the next check takes those sections as read.

The video is not built here: a render takes close to an hour. The record of
the upload (`upload.json`) holds the hash of the transcript content the video
on YouTube shows, and ``check_video`` reports whether it still agrees, which
is how the release knows a pull request needs a new render.

The video project is a pixi workspace of its own, so each of its commands is
a ``pixi run`` there. Requires ``pixi`` and ``claude`` on PATH.
"""

from __future__ import annotations

import json
import subprocess
from dataclasses import dataclass
from enum import StrEnum
from pathlib import Path

from .claude import run_claude
from .cli import ReleaseError
from .docs import (
    DOCS_DIR,
    NO_EM_DASH_PATTERN,
    USER_DOCS_PREFIX,
    em_dash_lines,
    reword_added_em_dashes,
)

# The project of the video, and the files of it the release names.
VIDEO_DIR = f"{DOCS_DIR}/concepts_video"
TRANSCRIPT_FILE = f"{VIDEO_DIR}/transcript.toml"
UPLOAD_FILE = f"{VIDEO_DIR}/upload.json"
VIDEO_README = f"{VIDEO_DIR}/README.md"

# The channel of Peppy in YouTube Studio, where a new render of the video is
# uploaded.
YOUTUBE_STUDIO_CHANNEL = "https://studio.youtube.com/channel/UCQzSqGbMBfHIKmuYo1KMAKw"


class SegmentStatus(StrEnum):
    """What the update did about one reported segment."""

    CHANGED = "changed"
    STILL_CORRECT = "still_correct"


@dataclass(frozen=True)
class ChangedSection:
    """A named section whose text the transcript does not record."""

    page: str
    section: str
    # The lines that changed since the last release, as a unified diff; None
    # when the last release has no such section.
    diff: str | None


@dataclass(frozen=True)
class MissingName:
    """A technical name a segment shows that none of its sections holds."""

    # The sentence or the screen text that shows the name.
    where: str
    name: str


@dataclass(frozen=True)
class SegmentFindings:
    """What the docs check found for one segment of the transcript."""

    segment_id: str
    changed_sections: tuple[ChangedSection, ...]
    # The reason for each source that names no section.
    missing_sections: tuple[str, ...]
    missing_names: tuple[MissingName, ...]

    @property
    def disagreements(self) -> tuple[str, ...]:
        """What only an edit can settle: a changed section may leave the
        segment correct, but a section that is gone or a name the docs do not
        hold never does."""
        return tuple(
            f"Segment `{self.segment_id}`: {reason}."
            for reason in self.missing_sections
        ) + tuple(
            f"Segment `{self.segment_id}`, {missing.where}: no section that "
            f"the segment names holds `{missing.name}`."
            for missing in self.missing_names
        )


@dataclass(frozen=True)
class VideoCheck:
    """The verdict of the video project's docs check."""

    segments: tuple[SegmentFindings, ...]
    # The hash of what the transcript makes the video say and show.
    content_sha256: str
    # Whether the video on YouTube was rendered from that content.
    upload_is_current: bool

    @property
    def disagreements(self) -> tuple[str, ...]:
        return tuple(
            line for segment in self.segments for line in segment.disagreements
        )


@dataclass(frozen=True)
class SegmentOutcome:
    segment_id: str
    status: SegmentStatus
    reason: str


@dataclass(frozen=True)
class VideoUpdate:
    """What the update did about each reported segment."""

    outcomes: tuple[SegmentOutcome, ...]
    # What a person has to do that the update could not; empty when nothing.
    for_the_reviewer: str

    @property
    def changed(self) -> tuple[SegmentOutcome, ...]:
        return tuple(o for o in self.outcomes if o.status is SegmentStatus.CHANGED)


# Schema for the update report: one entry per reported segment, so a segment
# the run never looked at cannot pass as read. The reasons and the note are
# quoted in a pull request, so they hold no em-dash.
_UPDATE_SCHEMA: dict = {
    "type": "object",
    "properties": {
        "segments": {
            "type": "array",
            "items": {
                "type": "object",
                "properties": {
                    "id": {"type": "string"},
                    "status": {"enum": list(SegmentStatus)},
                    "reason": {"type": "string", "pattern": NO_EM_DASH_PATTERN},
                },
                "required": ["id", "status", "reason"],
                "additionalProperties": False,
            },
        },
        "for_the_reviewer": {"type": "string", "pattern": NO_EM_DASH_PATTERN},
    },
    "required": ["segments", "for_the_reviewer"],
    "additionalProperties": False,
}


_UPDATE_PROMPT = """\
You are holding the transcript of a video to the documentation it rests on.
The project is "peppy". Its documentation is the pages under
`{docs}`, and `{transcript}` is the transcript of the video that explains the
concepts of peppy, which the quickstart page shows.

The transcript is the only source of what the video says and shows. Each
`[[segment]]` holds:
- `[[segment.voice]]`: the sentences of the voice, each with an `id`.
- `[segment.screen]`: the texts the screen shows.
- `[[segment.source]]`: the sections of the documentation the segment rests
  on. A section is the text below one heading of a page, up to the next
  heading of any level, with the snippet files it shows through
  `<Code code={{name}} />`. The title of a page names the text before its
  first heading.
A text between two backticks is a technical name: a command, a flag, a file
name or an API name. In a technical name, a word in angle brackets is a
placeholder.

A check that compares text only reported the segments below: a section they
name changed since the last release, a section they name no longer exists, or
a technical name they show is in none of their sections. Establish, for each
reported segment, whether it still agrees with the documentation, and correct
it where it does not.

Your task, for each reported segment:
1. Read the segment in the transcript, and Read each of its sections as the
   page holds it now. The documentation is right: the transcript follows it.
2. Edit the segment only when a sentence of the voice or a text of the screen
   states what its sections no longer state, or shows a technical name that
   its sections no longer hold. Then make the smallest edit that makes the
   segment agree with its sections again.
3. Edit nothing when the segment still agrees: a section that changed its
   wording, gained a detail the video does not mention, or changed an example
   the video does not show leaves the segment correct. When you are not sure
   that the segment states something false, edit nothing and say so in the
   reason.

Rules for an edit:
- Edit `{transcript}` only: no page of the documentation and no other file of
  the video.
- Keep the `id` of each segment and of each sentence, and each key of
  `[segment.screen]`: the compositions of the video name them. Change the
  texts only.
- Never change a `sha256`: a tool sets them after you. Change the `page` or
  the `section` of a `[[segment.source]]` only when the section moved or its
  heading changed.
- A sentence of the voice follows ASD-STE100 (Simplified Technical English),
  as the sentences around it do: 25 words at most, active voice, one idea.
- Keep a sentence close to its present length: the video has a limit of
  duration and is near it.
- Write a technical name between backticks and as its section writes it. Each
  technical name of a segment must be in a section the segment names.
- Never write an em-dash (U+2014): reword with a comma, a colon, parentheses
  or a separate sentence.

Report one `segments` entry per reported segment: its `id`, a `status` of
"changed" when you edited it or "still_correct" when you did not, and a
`reason` of one or two sentences that names the statement you corrected, or
says why the segment still agrees.

Give in `for_the_reviewer` only the work on another file that your edits make
necessary and that you could not do: for example a composition of the video
that shows a text which has to go, or a page of the documentation that
contradicts another one. Leave it empty when there is none. After your run, a
tool sets every `sha256` and the reviewer gets the steps to render the video
again, so mention neither.

Reported segments:
{segments}
"""


def _run_video_task(repo_root: Path, arguments: list[str], activity: str) -> str:
    """Run a pixi task of the video project and return what it printed.

    The task runs with the lockfile of the project as it is, so a release
    never re-resolves the tools of the video.
    """
    video_dir = repo_root / VIDEO_DIR
    result = subprocess.run(
        [
            "pixi",
            "run",
            "--locked",
            "--manifest-path",
            str(video_dir / "pixi.toml"),
            *arguments,
        ],
        cwd=video_dir,
        capture_output=True,
        text=True,
        check=False,
    )
    if result.returncode != 0:
        raise ReleaseError(
            f"{activity} failed (exit {result.returncode}): "
            f"{result.stderr.strip() or result.stdout.strip()}"
        )
    return result.stdout


def _parse_changed_section(item: object) -> ChangedSection:
    if not isinstance(item, dict):
        raise ReleaseError(f"video check changed section must be an object: {item!r}")
    page = item.get("page")
    section = item.get("section")
    diff = item.get("diff")
    if not isinstance(page, str) or not isinstance(section, str):
        raise ReleaseError(
            f"video check changed section missing page/section: {item!r}"
        )
    if diff is not None and not isinstance(diff, str):
        raise ReleaseError(f"video check changed section has a bad diff: {item!r}")
    return ChangedSection(page=page, section=section, diff=diff)


def _parse_missing_name(item: object) -> MissingName:
    if not isinstance(item, dict):
        raise ReleaseError(f"video check missing name must be an object: {item!r}")
    where = item.get("where")
    name = item.get("name")
    if not isinstance(where, str) or not isinstance(name, str):
        raise ReleaseError(f"video check missing name missing where/name: {item!r}")
    return MissingName(where=where, name=name)


def _parse_segment_findings(item: object) -> SegmentFindings:
    if not isinstance(item, dict):
        raise ReleaseError(f"video check segment must be an object: {item!r}")
    segment_id = item.get("segment_id")
    changed = item.get("changed_sections")
    missing_sections = item.get("missing_sections")
    missing_names = item.get("missing_names")
    if not isinstance(segment_id, str):
        raise ReleaseError(f"video check segment missing 'segment_id': {item!r}")
    if (
        not isinstance(changed, list)
        or not isinstance(missing_sections, list)
        or not isinstance(missing_names, list)
        or not all(isinstance(reason, str) for reason in missing_sections)
    ):
        raise ReleaseError(f"video check segment has a bad list: {item!r}")
    return SegmentFindings(
        segment_id=segment_id,
        changed_sections=tuple(_parse_changed_section(c) for c in changed),
        missing_sections=tuple(missing_sections),
        missing_names=tuple(_parse_missing_name(m) for m in missing_names),
    )


def _parse_check_report(payload: object) -> VideoCheck:
    """Validate the document `docs-check --json` prints into a VideoCheck.

    The command is the project's own, but a release reads it across a process
    boundary: a report in another shape surfaces as a ReleaseError, not as a
    stray KeyError.
    """
    if not isinstance(payload, dict):
        raise ReleaseError(f"video check report is not an object: {payload!r}")
    segments = payload.get("segments")
    content_sha256 = payload.get("content_sha256")
    upload_is_current = payload.get("upload_is_current")
    if not isinstance(segments, list):
        raise ReleaseError(f"video check report 'segments' must be a list: {payload!r}")
    if not isinstance(content_sha256, str) or not isinstance(upload_is_current, bool):
        raise ReleaseError(
            f"video check report missing 'content_sha256' or "
            f"'upload_is_current': {payload!r}"
        )
    return VideoCheck(
        segments=tuple(_parse_segment_findings(s) for s in segments),
        content_sha256=content_sha256,
        upload_is_current=upload_is_current,
    )


def check_video(repo_root: Path, baseline_docs: Path | None) -> VideoCheck:
    """Check the transcript of the video against the docs in the working tree.

    *baseline_docs* is the directory of the docs pages of the last release. A
    named section with the text it had there is not reported: the release
    that shipped that text read the segment against it. Without a baseline,
    every named section whose text the transcript does not record is.
    """
    arguments = ["docs-check", "--json"]
    if baseline_docs is not None:
        arguments += ["--baseline", str(baseline_docs)]
    stdout = _run_video_task(
        repo_root, arguments, "the docs check of the concepts video"
    )
    try:
        payload = json.loads(stdout)
    except json.JSONDecodeError as e:
        raise ReleaseError(
            f"the docs check of the concepts video did not print valid JSON "
            f"({e}); stdout={stdout[:500]!r}"
        ) from e
    return _parse_check_report(payload)


def record_sections(repo_root: Path) -> None:
    """Write into the transcript the hash of the present text of every docs
    section it names. Nothing else of the transcript changes."""
    _run_video_task(
        repo_root, ["record-sections"], "recording the docs sections of the video"
    )


def _render_segment(segment: SegmentFindings) -> str:
    """One reported segment, as the update prompt lists it."""
    lines = [f"### Segment `{segment.segment_id}`"]
    for changed in segment.changed_sections:
        lines.append(
            f"\nThe section `{changed.section}` of "
            f"`{USER_DOCS_PREFIX}{changed.page}` changed."
        )
        if changed.diff is None:
            lines.append("The last release has no such section, so all of it is new.")
            continue
        lines.append(f"Its lines since the last release:\n```diff\n{changed.diff}\n```")
    for reason in segment.missing_sections:
        lines.append(f"\nA source names a section that does not exist: {reason}.")
    for missing in segment.missing_names:
        lines.append(
            f"\nNo section of the segment holds the technical name "
            f"`{missing.name}`, which {missing.where} shows."
        )
    return "\n".join(lines)


def _parse_update_response(
    payload: dict, segments: tuple[SegmentFindings, ...]
) -> VideoUpdate:
    """Validate the update report into a VideoUpdate with one outcome per
    reported segment, in the order they were reported."""
    raw_segments = payload.get("segments")
    for_the_reviewer = payload.get("for_the_reviewer")
    if not isinstance(raw_segments, list):
        raise ReleaseError(
            f"claude video report 'segments' must be a list: {payload!r}"
        )
    if not isinstance(for_the_reviewer, str):
        raise ReleaseError(
            f"claude video report missing string 'for_the_reviewer': {payload!r}"
        )
    outcomes: dict[str, SegmentOutcome] = {}
    for item in raw_segments:
        if not isinstance(item, dict):
            raise ReleaseError(f"claude video report entry must be an object: {item!r}")
        segment_id = item.get("id")
        reason = item.get("reason")
        if not isinstance(segment_id, str) or not isinstance(reason, str):
            raise ReleaseError(f"claude video report entry missing id/reason: {item!r}")
        try:
            status = SegmentStatus(item.get("status"))
        except ValueError as err:
            raise ReleaseError(
                f"claude video report entry has unknown status: {item!r}"
            ) from err
        if segment_id in outcomes:
            raise ReleaseError(
                f"claude video report names the segment '{segment_id}' twice"
            )
        outcomes[segment_id] = SegmentOutcome(
            segment_id=segment_id, status=status, reason=reason
        )
    reported = [segment.segment_id for segment in segments]
    if sorted(outcomes) != sorted(reported):
        raise ReleaseError(
            f"claude video report covers the segments {sorted(outcomes)}, but "
            f"the check reported {sorted(reported)}"
        )
    return VideoUpdate(
        outcomes=tuple(outcomes[segment_id] for segment_id in reported),
        for_the_reviewer=for_the_reviewer.strip(),
    )


def update_video(repo_root: Path, segments: tuple[SegmentFindings, ...]) -> VideoUpdate:
    """Have Claude read each of *segments* against its docs sections and
    correct the transcript where it no longer agrees with them.

    The findings scope the run: Claude reads those segments and no other, and
    edits the transcript alone. It sets no hash (`record_sections` does), and
    its edits add no em-dash.
    """
    if not segments:
        raise ReleaseError("update_video called with no segment to read")
    em_dashes_before = em_dash_lines(repo_root)
    prompt = _UPDATE_PROMPT.format(
        docs=USER_DOCS_PREFIX,
        transcript=TRANSCRIPT_FILE,
        segments="\n\n".join(_render_segment(segment) for segment in segments),
    )
    payload = run_claude(
        prompt,
        allowed_tools="Read Edit Grep Glob",
        permission_mode="acceptEdits",
        cwd=repo_root,
        json_schema=_UPDATE_SCHEMA,
        activity="reading the concepts video against the docs",
        tools="Read Edit Grep Glob",
    )
    update = _parse_update_response(payload, segments)
    reword_added_em_dashes(em_dashes_before, repo_root)
    return update
