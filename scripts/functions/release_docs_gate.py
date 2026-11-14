"""The docs freshness gate a release passes before anything is built.

The docs check (`docs.check_docs`) reads the code changed since the last
shipped commit and reports where `docs/` no longer matches it. A release must
not ship documentation for behaviour that changed, so the gate runs in the
prepare stage, ahead of the drafted notes: everything past it is slow or
publishes something.

Only blocking gaps stop the release: docs that now state something false, or a
user-facing change with no documentation at all. Minor suggestions (wording,
clarity, nice-to-haves) never block, so a release is never held hostage by how
the model would phrase a sentence today; the release can open an optional pull
request applying them on the side.

The gate also holds the concepts video (`concepts_video.py`) to the docs about
to ship, once the docs themselves are settled in the working tree: the video
rests on docs sections, so it is read against them as the release will ship
them. A video that states what the docs no longer state stops the release like
a blocking gap does, and its corrected transcript goes into the same pull
request as the docs, never into a second one. A docs section that changed and
left the video correct stops nothing.
"""

from __future__ import annotations

import sys
from dataclasses import dataclass
from pathlib import Path

import httpx

from .cli import ReleaseError, console
from .concepts_video import (
    TRANSCRIPT_FILE,
    UPLOAD_FILE,
    VIDEO_DIR,
    VIDEO_README,
    SegmentFindings,
    SegmentStatus,
    VideoCheck,
    VideoUpdate,
    check_video,
    record_sections,
    update_video,
)
from .docs import (
    DOCS_DIR,
    USER_DOCS_PREFIX,
    RequiredChange,
    check_docs,
    print_minor_changes,
    update_docs,
)
from .github import RepoSlug, github_api
from .release_line import (
    ALIGNED_BRANCH,
    GIT_REMOTE,
    RELEASE_BRANCH,
    latest_release_tag,
)
from .repo import (
    commit_paths,
    detached_worktree,
    find_commit,
    has_changes_in_paths,
    is_ancestor,
    push_branch,
    switch_branch,
    switch_to_new_branch,
)

# Prefix of the throwaway branch carrying a regenerated docs tree. The release
# commit is appended, so re-running the release for the same commit reuses the
# branch (and its open pull request) rather than piling up new ones.
DOCS_SYNC_BRANCH_PREFIX = "auto/docs-update-"

# Prefix of the same branch when the release skipped the docs check, so that
# it carries a corrected concepts video alone. A merged docs-sync pull request
# vouches that the docs check judged the code up to its commit
# (`_judged_commit_of`), which a branch cut without that check must not do.
VIDEO_SYNC_BRANCH_PREFIX = "auto/video-update-"

# Prefix of the branch carrying the optional minor doc polish a release asked
# for. Separate from the sync prefix so a rerun that finds a blocking gap never
# mistakes an open wording-level pull request for the fix it has to wait on.
DOCS_POLISH_BRANCH_PREFIX = "auto/docs-polish-"


# --- the commit the docs check diffs from ---


def _last_shipped_commit(
    client: httpx.Client, slug: RepoSlug, release_commit: str
) -> str:
    """The last shipped commit, which a release's docs are checked from.

    `origin/main` names it whenever the previous release ran to the end: the
    last step of a release fast-forwards `main` to the commit carrying the
    notes, whose only change is under `docs/`, so the code diff since `main`
    is the code diff since the release tag. A release published but never
    aligned (a publish that stopped after the GitHub release went out) leaves
    `main` behind its tag. Diffing from `main` would then re-audit a release
    the docs already went through, several times slower, and since the check
    caps the diff it reads, the changes of the release at hand can fall past
    the cap and go unjudged. The latest published release's tag is the base
    then: the same commit the release notes are drafted from.

    `origin/main` stays the base without a published release, and when the
    tag is not on the release line (a tag cut from elsewhere): the branch
    check has already proved `origin/main` is.
    """
    aligned = f"{GIT_REMOTE}/{ALIGNED_BRANCH}"
    tag = latest_release_tag(client, slug)
    if not tag:
        return aligned
    if is_ancestor(tag, aligned):
        return aligned
    if not (is_ancestor(aligned, tag) and is_ancestor(tag, release_commit)):
        return aligned
    console.print(
        f"[yellow]{aligned} is behind the latest release {tag}, so it no "
        f"longer names the last shipped commit: '{DOCS_DIR}/' is checked "
        f"against {tag} instead. '{ALIGNED_BRANCH}' catches up when this "
        f"release publishes.[/yellow]"
    )
    return tag


# How many closed pull requests `_last_judged_commit` reads, most recently
# updated first. A docs pull request merged since the last release is among
# them; one that is not only costs the check it would have saved.
_JUDGED_LOOKUP_PAGE_SIZE = 100


@dataclass(frozen=True)
class JudgedCommit:
    """A commit the docs check judged, whose gaps a merged pull request closed."""

    commit: str
    pr_url: str


def _judged_commit_of(
    pull: object, shipped: str, release_commit: str
) -> JudgedCommit | None:
    """The commit *pull* vouches for on the way to *release_commit*, or None.

    A docs-sync pull request vouches for the commit its branch is named after
    once it is merged and that merge is part of the release: the check judged
    the code up to that commit and the merge closed every gap it found.
    """
    if not isinstance(pull, dict) or not pull.get("merged_at"):
        return None
    head = pull.get("head")
    branch = head.get("ref") if isinstance(head, dict) else None
    if not isinstance(branch, str) or not branch.startswith(DOCS_SYNC_BRANCH_PREFIX):
        return None
    merge_commit = pull.get("merge_commit_sha")
    pr_url = pull.get("html_url")
    if not isinstance(merge_commit, str) or not isinstance(pr_url, str):
        return None
    commit = find_commit(branch.removeprefix(DOCS_SYNC_BRANCH_PREFIX))
    if commit is None or find_commit(merge_commit) is None:
        return None
    if not is_ancestor(shipped, commit):
        return None
    if not is_ancestor(commit, release_commit):
        return None
    if not is_ancestor(merge_commit, release_commit):
        return None
    return JudgedCommit(commit=commit, pr_url=pr_url)


def _last_judged_commit(
    client: httpx.Client, slug: RepoSlug, shipped: str, release_commit: str
) -> JudgedCommit | None:
    """The latest commit since *shipped* the docs check already judged, or None.

    The check is not reproducible: two runs over the same code report
    different gaps. Judging from the last shipped commit again once a docs
    pull request is merged would spend the same runs on the same code only to
    draw again, and a release would stop on a fresh pull request for as long
    as the draws differ. A merged docs-sync pull request settles the code up
    to the commit it was cut for (`_judged_commit_of`).
    """
    response = github_api(
        client,
        "GET",
        f"{slug.api_url}/pulls"
        f"?base={RELEASE_BRANCH}&state=closed&sort=updated&direction=desc"
        f"&per_page={_JUDGED_LOOKUP_PAGE_SIZE}",
    )
    if not isinstance(response, list):
        return None
    latest: JudgedCommit | None = None
    for pull in response:
        judged = _judged_commit_of(pull, shipped, release_commit)
        if judged is None:
            continue
        if latest is None or is_ancestor(latest.commit, judged.commit):
            latest = judged
    return latest


def _docs_check_base(
    client: httpx.Client, slug: RepoSlug, shipped: str, release_commit: str
) -> str:
    """The commit the docs check diffs the release from.

    *shipped*, the last shipped commit (`_last_shipped_commit`), or past it
    the latest commit the check already judged and whose gaps a merged pull
    request closed (`_last_judged_commit`): only the code changed since is
    left to judge, which is none at all on the rerun that follows such a
    merge.
    """
    judged = _last_judged_commit(client, slug, shipped, release_commit)
    if judged is None:
        return shipped
    console.print(
        f"[yellow]'{DOCS_DIR}/' was already judged up to {judged.commit[:12]} "
        f"and its gaps closed by {judged.pr_url}: only the code changed since "
        f"is checked.[/yellow]"
    )
    return judged.commit


# --- the docs pull requests ---

# Subject of the commit and of the pull request that close blocking docs gaps,
# with or without a corrected concepts video, and of those that carry a
# corrected concepts video alone.
_DOCS_SYNC_SUBJECT = "docs: sync with the code being released"
_VIDEO_SYNC_SUBJECT = "docs: sync the concepts video with the docs being released"

# How a pull request names what the update did about a segment of the video.
_STATUS_WORDS = {
    SegmentStatus.CHANGED: "changed",
    SegmentStatus.STILL_CORRECT: "still correct",
}


@dataclass(frozen=True)
class DocsPull:
    """A docs pull request the release opened."""

    number: int
    url: str


@dataclass(frozen=True)
class VideoSync:
    """What the gate did about the concepts video in the working tree."""

    # What the check reported, before the update.
    findings: tuple[SegmentFindings, ...]
    update: VideoUpdate


def _sync_branch(release_commit: str, *, docs_checked: bool) -> str:
    """The branch of the pull request that stops the release at
    *release_commit* (see `DOCS_SYNC_BRANCH_PREFIX` and
    `VIDEO_SYNC_BRANCH_PREFIX`)."""
    prefix = DOCS_SYNC_BRANCH_PREFIX if docs_checked else VIDEO_SYNC_BRANCH_PREFIX
    return f"{prefix}{release_commit[:12]}"


def _video_pr_lines(video: VideoSync, remaining: VideoCheck) -> list[str]:
    """The part of the pull request body about the concepts video.

    *remaining* is the check of the transcript as the pull request holds it:
    what the update left for the reviewer, and whether the video on YouTube
    still shows that transcript.
    """
    lines = [
        (
            f"The concepts video (`{VIDEO_DIR}`), read against the docs "
            + "sections it rests on (`scripts/functions/concepts_video.py`):"
        ),
        "",
    ]
    lines += [
        f"- Segment `{outcome.segment_id}` ({_STATUS_WORDS[outcome.status]}): "
        + outcome.reason
        for outcome in video.update.outcomes
    ]
    lines += [
        "",
        f"`{TRANSCRIPT_FILE}` records the present text of every docs section "
        + "it names.",
    ]
    left = list(remaining.disagreements)
    if video.update.for_the_reviewer:
        left.append(video.update.for_the_reviewer)
    if left:
        lines += ["", "Left for the reviewer:", ""]
        lines += [f"- {item}" for item in left]
    if not remaining.upload_is_current:
        lines += [
            "",
            "The video on YouTube is not the video of this transcript. The "
            + "comment below gives the steps to render and upload it, and the "
            + "`concepts-video` check fails until they are done.",
        ]
    return lines


def _docs_sync_pr_body(
    release_commit: str,
    changes: tuple[RequiredChange, ...],
    video: VideoSync | None = None,
    remaining: VideoCheck | None = None,
) -> str:
    """Render the body of the pull request that stops the release: the docs
    gaps it closes, and what it did about the concepts video."""
    if changes:
        lines = [
            (
                "Automated docs update produced by the release script "
                + "(`scripts/functions/docs.py`), which found `docs/` lagging the code "
                + "at the commit the release was being cut from."
            )
        ]
    else:
        lines = [
            (
                "Automated update of the concepts video produced by the release "
                + "script (`scripts/functions/concepts_video.py`), which found the "
                + "transcript of the video lagging the docs at the commit the "
                + "release was being cut from."
            )
        ]
    lines += ["", f"Release commit: {release_commit}"]
    if changes:
        lines += ["", "Gaps the check reported:", ""]
        lines += [f"- `{c.file}`: {c.change}" for c in changes]
    if video is not None and remaining is not None:
        lines += ["", *_video_pr_lines(video, remaining)]
    lines += [
        "",
        f"Merge this into `{RELEASE_BRANCH}`, then re-run the release.",
    ]
    return "\n".join(lines)


def _render_steps_comment(slug: RepoSlug, branch: str, content_sha256: str) -> str:
    """Render the comment that tells the reviewer of a pull request how to
    render the concepts video again, upload it, and give its link.

    GitHub has no form field on a pull request. The link to its editor of
    `upload.json` on *branch* is the text box the comment offers: the
    reviewer types the id of the upload there and commits from the browser.
    *content_sha256* is the hash that record has to hold for the transcript
    of the pull request.
    """
    readme = f"https://github.com/{slug.full}/blob/{branch}/{VIDEO_README}"
    editor = f"https://github.com/{slug.full}/edit/{branch}/{UPLOAD_FILE}"
    upload_name = Path(UPLOAD_FILE).name
    transcript_name = Path(TRANSCRIPT_FILE).name
    return "\n".join(
        [
            "## The concepts video needs a new render",
            "",
            (
                "This pull request changes what the concepts video says or shows, "
                + "so the video on the quickstart page is not the video of this "
                + "transcript. The `concepts-video` check fails until the steps "
                + f"below are done. [The README of the video project]({readme}) "
                + "has the details of each step."
            ),
            "",
            "1. On a Linux x86_64 machine with [pixi](https://pixi.sh), get this branch:",
            "",
            "   ```sh",
            f"   git fetch origin {branch}",
            f"   git switch {branch}",
            f"   cd {VIDEO_DIR}",
            "   ```",
            "",
            (
                f"2. Read the changes of `{transcript_name}` in this pull request, "
                + "and correct them where necessary."
            ),
            "",
            (
                "3. Render the video, which takes about one hour on a machine "
                + "with 16 processors, then run the checks:"
            ),
            "",
            "   ```sh",
            "   pixi run build",
            "   pixi run test",
            "   pixi run layout",
            "   pixi run motion",
            "   ```",
            "",
            (
                "   The build writes `voice_durations.json` when a sentence of the "
                + "voice changed. Until step 6, `pixi run test` fails on the "
                + "upload only."
            ),
            "",
            "4. Watch `build/concepts_video.mp4`.",
            "",
            (
                "5. Upload it to YouTube with the captions "
                + "`build/concepts_video.en.srt`, and paste "
                + "`build/concepts_video.chapters.txt` into the description."
            ),
            "",
            "6. Give the link of the upload, in one of two ways:",
            "",
            "   - In the terminal: `pixi run record-upload <link>`.",
            (
                f"   - In the browser: [edit `{upload_name}` on this branch]({editor}). "
                + "Put the id of the link (the 11 characters after `youtu.be/`) in "
                + "`youtube_id`, put the hash below in `content_sha256`, and commit "
                + "the change to this branch."
            ),
            "",
            "     ```",
            f"     {content_sha256}",
            "     ```",
            "",
            (
                "     This is the hash of the transcript as this pull request opened. "
                + "If the transcript changed in step 2, `pixi run test` gives the "
                + "hash to put."
            ),
            "",
            (
                "7. Commit `voice_durations.json`, and "
                + f"`{upload_name}` if the terminal wrote it, then push."
            ),
        ]
    )


def _docs_polish_pr_body(
    release_commit: str,
    changes: tuple[RequiredChange, ...],
) -> str:
    """Render the body of the optional docs-polish pull request."""
    lines = [
        (
            "Optional docs polish produced by the release script "
            + "(`scripts/functions/docs.py`): the docs check reported only minor "
            + "suggestions, and the user chose to apply them anyway."
        ),
        "",
        f"Release commit: {release_commit}",
        "",
        "Suggestions applied:",
        "",
    ]
    lines += [f"- `{c.file}`: {c.change}" for c in changes]
    lines += [
        "",
        "This pull request does not block the release it was cut from; merge "
        + "or close it on its own schedule.",
    ]
    return "\n".join(lines)


def _find_open_docs_pr(
    client: httpx.Client,
    slug: RepoSlug,
    branch: str,
) -> str | None:
    """Return the URL of an open pull request from *branch*, or None.

    The branch name pins the release commit, so an open pull request from it
    means an earlier attempt on this same commit already produced the docs
    update. That one is reported as-is: it is the fix, and re-deriving it would
    only overwrite work a reviewer may have already added to it.
    """
    response = github_api(
        client,
        "GET",
        f"{slug.api_url}/pulls"
        f"?head={slug.owner}:{branch}&base={RELEASE_BRANCH}&state=open",
    )
    if not isinstance(response, list):
        return None
    for pull in response:
        if isinstance(pull, dict) and pull.get("html_url"):
            return str(pull["html_url"])
    return None


def _push_docs_branch(branch: str, docs_dir: Path, message: str) -> None:
    """Commit the regenerated docs onto *branch* as *message* and push it.

    The branch is cut from the release commit and carries the working-tree edits
    over, so its single commit holds the docs changes and nothing else. The
    checkout returns to `dev` in a `finally`, so a failed commit or push never
    strands the working tree on the throwaway branch, and the docs edits leave
    the tree with it: they live on the branch from here on.

    The push is a plain one. An open pull request for this release commit has
    already been ruled out by the caller, so a rejection here means a branch was
    left behind by an attempt whose pull request is gone, and the fix is to say
    so rather than to overwrite whatever is on it.
    """
    switch_to_new_branch(branch)
    try:
        commit_paths([docs_dir], message)
        console.print(f"Pushing '{branch}' to {GIT_REMOTE}...")
        try:
            push_branch(GIT_REMOTE, branch, branch)
        except ReleaseError as e:
            raise ReleaseError(
                f"{e}\n"
                f"'{GIT_REMOTE}/{branch}' already holds different commits and no "
                f"pull request is open for it, so it is left over from an earlier "
                f"attempt. Inspect it, then delete it with "
                f"`git push {GIT_REMOTE} --delete {branch}` and retry."
            ) from e
    finally:
        switch_branch(RELEASE_BRANCH)


def _open_docs_pr(
    client: httpx.Client,
    slug: RepoSlug,
    branch: str,
    title: str,
    body: str,
) -> DocsPull:
    """Open a docs pull request from *branch* into the release branch."""
    response = github_api(
        client,
        "POST",
        f"{slug.api_url}/pulls",
        json_data={
            "title": title,
            "head": branch,
            "base": RELEASE_BRANCH,
            "body": body,
        },
    )
    if (
        not isinstance(response, dict)
        or not response.get("html_url")
        or not isinstance(response.get("number"), int)
    ):
        raise ReleaseError(
            f"unexpected GitHub API response when opening the docs pull "
            f"request: {response!r}"
        )
    return DocsPull(number=response["number"], url=str(response["html_url"]))


def _comment_on_docs_pr(
    client: httpx.Client, slug: RepoSlug, pull: DocsPull, body: str
) -> None:
    """Leave *body* as a comment on *pull*."""
    github_api(
        client,
        "POST",
        f"{slug.api_url}/issues/{pull.number}/comments",
        json_data={"body": body},
    )


def _stop_for_docs_pr(pr_url: str) -> None:
    """Point at the docs pull request and end the release."""
    console.print(
        f"\n[yellow]Release stopped: the docs have to be updated first.[/yellow]\n"
        f"Review and merge {pr_url}\n"
        f"then start a new run of the release from '{RELEASE_BRANCH}'."
    )
    sys.exit(1)


def _stop_on_open_docs_pr(client: httpx.Client, slug: RepoSlug, branch: str) -> None:
    """End the release on the pull request an earlier attempt on this same
    commit opened from *branch*; return when there is none.

    Called before each Claude run that edits `docs/`: deriving the update
    again would only spend that run to arrive at a branch that has to replace
    the one under review.
    """
    pr_url = _find_open_docs_pr(client, slug, branch)
    if not pr_url:
        return
    console.print(
        "[yellow]A docs pull request is already open for this commit.[/yellow]"
    )
    _stop_for_docs_pr(pr_url)


# --- the gate ---


def _close_blocking_docs_gaps(
    client: httpx.Client,
    slug: RepoSlug,
    branch: str,
    release_commit: str,
    docs_dir: Path,
    blocking: tuple[RequiredChange, ...],
    base: str,
) -> tuple[RequiredChange, ...]:
    """Have Claude close *blocking* in the working tree; return the gaps the
    edits closed.

    Claude closes exactly the reported gaps. The edits stay in the working
    tree, where the concepts video is then read against them, and the gate
    commits both onto *branch* (`_open_sync_pr_and_stop`). An earlier attempt
    on this same commit that already opened that pull request ends the release
    on it instead.

    Returns no gap when the updater verifies that every reported one is
    already documented: the check's findings were noise.
    """
    console.print(f"[yellow]'{DOCS_DIR}/' is out of date:[/yellow]")
    for change in blocking:
        console.print(f"  [bold]{change.file}[/bold]: {change.change}")

    _stop_on_open_docs_pr(client, slug, branch)

    console.print("Asking Claude to update the docs...")
    update = update_docs(base, release_commit, blocking)
    console.print(update.summary)

    if has_changes_in_paths([docs_dir]):
        return blocking
    if not update.all_already_covered:
        raise ReleaseError(
            f"the check reported '{DOCS_DIR}/' as out of date and the update "
            f"claimed to close gaps, but nothing changed there, so there is "
            f"no pull request to open. Update the docs by hand and push them "
            f"to '{RELEASE_BRANCH}', or run the release again with "
            f"--skip-docs-check if the report is wrong."
        )
    console.print(
        f"[green]The updater verified every reported gap is already "
        f"documented; '{DOCS_DIR}/' covers the release.[/green]"
    )
    return ()


def _print_video_findings(findings: tuple[SegmentFindings, ...]) -> None:
    console.print(
        "[yellow]The concepts video has to be read against the docs:[/yellow]"
    )
    for segment in findings:
        for changed in segment.changed_sections:
            console.print(
                f"  [bold]{segment.segment_id}[/bold]: the section "
                f"'{changed.section}' of {changed.page} changed."
            )
        for line in segment.disagreements:
            console.print(f"  {line}")


def _sync_video(
    client: httpx.Client,
    slug: RepoSlug,
    branch: str,
    shipped: str,
    repo_root: Path,
) -> VideoSync | None:
    """Correct the concepts video in the working tree where the docs about to
    ship no longer agree with it; return what was done, or None when the
    check reports nothing.

    The check reads the docs of the working tree, so it sees the edits that
    closed the docs gaps of this release, against the docs of *shipped*, the
    last shipped commit: a section a release already shipped was read then.
    Claude then reads each reported segment against its sections and edits
    the transcript where it states what they no longer state. An earlier
    attempt on this same commit that already opened the pull request ends the
    release on it instead.
    """
    console.print(
        f"Checking the concepts video against the docs sections changed "
        f"since {shipped}..."
    )
    with detached_worktree(shipped) as last_release:
        check = check_video(repo_root, last_release / USER_DOCS_PREFIX)
    if not check.segments:
        console.print("[green]The concepts video agrees with the docs.[/green]")
        return None
    _print_video_findings(check.segments)

    _stop_on_open_docs_pr(client, slug, branch)

    console.print("Asking Claude to read the concepts video against the docs...")
    update = update_video(repo_root, check.segments)
    for outcome in update.outcomes:
        console.print(
            f"  [bold]{outcome.segment_id}[/bold] "
            f"({_STATUS_WORDS[outcome.status]}): {outcome.reason}"
        )
    if update.changed and not has_changes_in_paths([repo_root / TRANSCRIPT_FILE]):
        raise ReleaseError(
            f"the update claimed to correct the concepts video, but "
            f"'{TRANSCRIPT_FILE}' did not change, so there is no pull request "
            f"to open. Correct the transcript by hand and push it to "
            f"'{RELEASE_BRANCH}', or run the release again with "
            f"--skip-video-check if the report is wrong."
        )
    return VideoSync(findings=check.segments, update=update)


def _require_video_agrees(video: VideoSync) -> None:
    """Stop when the concepts video, left as it was, disagrees with the docs.

    A section that changed can leave its segments correct, and then nothing
    is left to do. A section that is gone, or a technical name the docs no
    longer hold, cannot.
    """
    left = [line for segment in video.findings for line in segment.disagreements]
    if left:
        raise ReleaseError(
            "the concepts video names what the docs do not hold, and the "
            "update changed nothing:\n"
            + "\n".join(left)
            + f"\nCorrect '{TRANSCRIPT_FILE}' or the docs by hand and push "
            f"them to '{RELEASE_BRANCH}', or run the release again with "
            f"--skip-video-check."
        )
    console.print(
        "[green]The docs sections that changed leave the concepts video "
        "correct.[/green]"
    )


def _open_sync_pr_and_stop(
    client: httpx.Client,
    slug: RepoSlug,
    branch: str,
    release_commit: str,
    repo_root: Path,
    gaps: tuple[RequiredChange, ...],
    video: VideoSync | None,
) -> None:
    """Open the one pull request carrying every edit of the gate under
    `docs/`, and stop the release.

    The edits are pushed to *branch*, named after the release commit, and a
    pull request against `dev` is opened: the docs and the video land through
    review like any other change rather than being folded into a release
    nobody reads.

    When the video was read, the transcript first records the present text of
    every docs section it names, so the rerun that follows the merge does not
    read the same sections again. When the transcript then says or shows
    something the video on YouTube does not, a comment on the pull request
    gives the reviewer the steps to render and upload the video again.
    """
    remaining: VideoCheck | None = None
    if video is not None:
        record_sections(repo_root)
        remaining = check_video(repo_root, None)

    subject = _DOCS_SYNC_SUBJECT if gaps else _VIDEO_SYNC_SUBJECT
    _push_docs_branch(branch, repo_root / DOCS_DIR, subject)
    pull = _open_docs_pr(
        client,
        slug,
        branch,
        f"{subject} ({release_commit[:12]})",
        _docs_sync_pr_body(release_commit, gaps, video, remaining),
    )
    if remaining is not None and not remaining.upload_is_current:
        _comment_on_docs_pr(
            client,
            slug,
            pull,
            _render_steps_comment(slug, branch, remaining.content_sha256),
        )
    _stop_for_docs_pr(pull.url)


def _open_minor_docs_pr(
    client: httpx.Client,
    slug: RepoSlug,
    release_commit: str,
    docs_dir: Path,
    minor: tuple[RequiredChange, ...],
    base: str,
) -> None:
    """Open the optional pull request applying *minor*; the release goes on.

    An earlier polish pull request for this commit is reported instead of
    re-derived, and an update that ends up changing nothing simply leaves
    nothing to open.
    """
    branch = f"{DOCS_POLISH_BRANCH_PREFIX}{release_commit[:12]}"
    pr_url = _find_open_docs_pr(client, slug, branch)
    if pr_url:
        console.print(
            f"[yellow]A docs-polish pull request is already open for this "
            f"commit: {pr_url}[/yellow]"
        )
        return

    console.print("Asking Claude to apply the minor suggestions...")
    update = update_docs(base, release_commit, minor)
    console.print(update.summary)

    if not has_changes_in_paths([docs_dir]):
        console.print(
            f"[yellow]The update changed nothing under '{DOCS_DIR}/'; there is "
            f"no pull request to open.[/yellow]"
        )
        return

    _push_docs_branch(branch, docs_dir, "docs: minor polish")
    pull = _open_docs_pr(
        client,
        slug,
        branch,
        f"docs: minor polish ({release_commit[:12]})",
        _docs_polish_pr_body(release_commit, minor),
    )
    console.print(f"Opened {pull.url}; it does not block this release.")


def verify_docs_gate(
    client: httpx.Client,
    slug: RepoSlug,
    release_commit: str,
    repo_root: Path,
    *,
    open_minor_docs_pr: bool,
    skip_docs_check: bool,
    skip_video_check: bool,
) -> None:
    """Stop the release while `docs/` lags the code about to ship, or the
    concepts video lags those docs.

    The docs check diffs the code from the last shipped commit (`origin/main`
    as a rule, or the latest release's tag when `main` was left behind it),
    or past it from the commit a merged docs pull request already settled
    (`_docs_check_base`). Claude closes its blocking gaps in the working tree
    (`_close_blocking_docs_gaps`). The concepts video is then read against
    the docs of that tree, and Claude corrects its transcript there
    (`_sync_video`).

    Whatever those two steps changed under `docs/` goes into one pull request
    against `dev`, which stops the release (`_open_sync_pr_and_stop`). A
    rerun on the same commit finds that pull request and stops on it, so
    nothing published is ever rewritten; once it is merged, the rerun checks
    the code from the commit it was cut for and finds the docs sections of
    the video recorded, so neither is judged a second time.

    Minor docs suggestions are printed, and *open_minor_docs_pr* decides
    whether a pull request applying them is opened on the side
    (`_open_minor_docs_pr`). *skip_docs_check* and *skip_video_check* each
    leave one of the two checks out.
    """
    docs_dir = repo_root / DOCS_DIR
    if has_changes_in_paths([docs_dir]):
        raise ReleaseError(
            f"'{DOCS_DIR}/' has uncommitted changes. The docs check commits "
            f"that directory onto a branch of its own, which would sweep those "
            f"edits into the pull request."
        )

    shipped = _last_shipped_commit(client, slug, release_commit)
    branch = _sync_branch(release_commit, docs_checked=not skip_docs_check)

    base = shipped
    gaps: tuple[RequiredChange, ...] = ()
    minor: tuple[RequiredChange, ...] = ()
    if not skip_docs_check:
        base = _docs_check_base(client, slug, shipped, release_commit)
        console.print(f"Checking '{DOCS_DIR}/' covers the code changes since {base}...")
        result = check_docs(base, release_commit)
        print_minor_changes(result.minor)
        if result.blocking:
            gaps = _close_blocking_docs_gaps(
                client, slug, branch, release_commit, docs_dir, result.blocking, base
            )
        else:
            minor = result.minor

    video = (
        None
        if skip_video_check
        else _sync_video(client, slug, branch, shipped, repo_root)
    )

    if has_changes_in_paths([docs_dir]):
        _open_sync_pr_and_stop(
            client, slug, branch, release_commit, repo_root, gaps, video
        )
        return
    if video is not None:
        _require_video_agrees(video)

    if skip_docs_check:
        return
    if minor and open_minor_docs_pr:
        _open_minor_docs_pr(client, slug, release_commit, docs_dir, minor, base)
    console.print(f"[green]'{DOCS_DIR}/' is up to date.[/green]")
