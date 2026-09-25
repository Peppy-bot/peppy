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
"""

from __future__ import annotations

import sys
from dataclasses import dataclass
from pathlib import Path

import httpx

from .cli import ReleaseError, console
from .docs import (
    DOCS_DIR,
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
    client: httpx.Client, slug: RepoSlug, release_commit: str
) -> str:
    """The commit the docs check diffs the release from.

    The last shipped commit (`_last_shipped_commit`), or past it the latest
    commit the check already judged and whose gaps a merged pull request
    closed (`_last_judged_commit`): only the code changed since is left to
    judge, which is none at all on the rerun that follows such a merge.
    """
    shipped = _last_shipped_commit(client, slug, release_commit)
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


def _docs_sync_pr_body(
    release_commit: str,
    changes: tuple[RequiredChange, ...],
) -> str:
    """Render the body of the docs-sync pull request."""
    lines = [
        (
            "Automated docs update produced by the release script "
            + "(`scripts/functions/docs.py`), which found `docs/` lagging the code "
            + "at the commit the release was being cut from."
        ),
        "",
        f"Release commit: {release_commit}",
    ]
    if changes:
        lines += ["", "Gaps the check reported:", ""]
        lines += [f"- `{c.file}`: {c.change}" for c in changes]
    lines += [
        "",
        f"Merge this into `{RELEASE_BRANCH}`, then re-run the release.",
    ]
    return "\n".join(lines)


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


def _find_open_docs_sync_pr(
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


def _push_docs_sync_branch(branch: str, docs_dir: Path, message: str) -> None:
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
) -> str:
    """Open a docs pull request from *branch* into the release branch.

    Returns the pull request URL.
    """
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
    if not isinstance(response, dict) or not response.get("html_url"):
        raise ReleaseError(
            f"unexpected GitHub API response when opening the docs pull "
            f"request: {response!r}"
        )
    return str(response["html_url"])


def _stop_for_docs_pr(pr_url: str) -> None:
    """Point at the docs pull request and end the release."""
    console.print(
        f"\n[yellow]Release stopped: the docs have to be updated first.[/yellow]\n"
        f"Review and merge {pr_url}\n"
        f"then start a new run of the release from '{RELEASE_BRANCH}'."
    )
    sys.exit(1)


# --- the gate ---


def _close_blocking_docs_gaps(
    client: httpx.Client,
    slug: RepoSlug,
    release_commit: str,
    docs_dir: Path,
    blocking: tuple[RequiredChange, ...],
    base: str,
) -> None:
    """Open the pull request closing *blocking* and stop the release.

    Claude closes exactly the reported gaps, the result is pushed to a branch
    named after the release commit, and a pull request against `dev` is
    opened: the docs land through review like any other change rather than
    being folded into a release nobody reads. An earlier attempt on this same
    commit that already opened that pull request is reported instead, since
    re-deriving the docs would only spend another Claude run to arrive at a
    branch that has to replace the one under review.

    Returns only when the updater verifies that every reported gap is already
    documented: the check's findings were noise, and the release continues.
    """
    console.print(f"[yellow]'{DOCS_DIR}/' is out of date:[/yellow]")
    for change in blocking:
        console.print(f"  [bold]{change.file}[/bold]: {change.change}")

    branch = f"{DOCS_SYNC_BRANCH_PREFIX}{release_commit[:12]}"
    pr_url = _find_open_docs_sync_pr(client, slug, branch)
    if pr_url:
        console.print(
            "[yellow]A docs pull request is already open for this commit.[/yellow]"
        )
        _stop_for_docs_pr(pr_url)

    console.print("Asking Claude to update the docs...")
    update = update_docs(base, release_commit, blocking)
    console.print(update.summary)

    if not has_changes_in_paths([docs_dir]):
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
        return

    _push_docs_sync_branch(branch, docs_dir, "docs: sync with the code being released")
    pr_url = _open_docs_pr(
        client,
        slug,
        branch,
        f"docs: sync with the code being released ({release_commit[:12]})",
        _docs_sync_pr_body(release_commit, blocking),
    )
    _stop_for_docs_pr(pr_url)


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
    pr_url = _find_open_docs_sync_pr(client, slug, branch)
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

    _push_docs_sync_branch(branch, docs_dir, "docs: minor polish")
    pr_url = _open_docs_pr(
        client,
        slug,
        branch,
        f"docs: minor polish ({release_commit[:12]})",
        _docs_polish_pr_body(release_commit, minor),
    )
    console.print(f"Opened {pr_url}; it does not block this release.")


def verify_docs_gate(
    client: httpx.Client,
    slug: RepoSlug,
    release_commit: str,
    repo_root: Path,
    *,
    open_minor_docs_pr: bool,
) -> None:
    """Stop the release while `docs/` lags the code about to ship.

    The diff base is the last shipped commit (`origin/main` as a rule, or the
    latest release's tag when `main` was left behind it), or past it the
    commit a merged docs pull request already settled (`_docs_check_base`).

    Blocking gaps get a pull request against `dev` and stop the release
    (`_close_blocking_docs_gaps`). A rerun on the same commit finds that pull
    request and stops on it, so nothing published is ever rewritten; once it
    is merged, the rerun checks from the commit it was cut for, so the code it
    settled is not judged a second time. Minor suggestions are printed, and
    *open_minor_docs_pr* decides whether a pull request applying them is
    opened on the side (`_open_minor_docs_pr`).
    """
    docs_dir = repo_root / DOCS_DIR
    if has_changes_in_paths([docs_dir]):
        raise ReleaseError(
            f"'{DOCS_DIR}/' has uncommitted changes. The docs check commits "
            f"that directory onto a branch of its own, which would sweep those "
            f"edits into the pull request."
        )

    base = _docs_check_base(client, slug, release_commit)
    console.print(f"Checking '{DOCS_DIR}/' covers the code changes since {base}...")
    result = check_docs(base, release_commit)
    print_minor_changes(result.minor)

    if result.blocking:
        _close_blocking_docs_gaps(
            client, slug, release_commit, docs_dir, result.blocking, base
        )
        return

    if result.minor and open_minor_docs_pr:
        _open_minor_docs_pr(
            client, slug, release_commit, docs_dir, result.minor, base
        )
    console.print(f"[green]'{DOCS_DIR}/' is up to date.[/green]")
