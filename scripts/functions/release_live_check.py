"""Check that users get a published release from the sites they read.

publish makes the release the latest one on GitHub and fast-forwards `main`
to its notes commit. Users reach the release through two sites that this
repository does not serve, and the `live-check` stage reads them until each
URL below serves the release:

- peppy.bot redirects `install.sh` to the install.sh of `main`, and
  `latest/<archive>` and `<tag>/<archive>` to the archives of the GitHub
  release. install.sh downloads the archive from those two URLs.
- docs.peppy.bot, which Railway deploys on each push to `main`, lists the
  notes of every release on its changelog page (see release_notes.py).

A URL that does not serve the release yet is read again at the next poll: a
docs deploy ends a few minutes after the push, and raw.githubusercontent.com
caches install.sh for up to 5 minutes. Once LIVE_WAIT_TIMEOUT_SECONDS have
passed, the stage fails and names each URL that still does not serve the
release, with what it serves.
"""

from __future__ import annotations

from collections.abc import Callable, Sequence
from dataclasses import dataclass
from functools import partial

import httpx

from .build import release_archive_name
from .cli import RELEASE_TRIPLES, ReleaseError, console
from .github import RepoSlug
from .release_notes import CHANGELOG_URL, release_page_path

DOWNLOAD_SITE = "https://peppy.bot"

# A docs deploy takes about 2 minutes, and a cached install.sh lives 5
# minutes, so 20 minutes leaves room for a slow deploy and still fails the
# same hour the release is published.
LIVE_POLL_INTERVAL_SECONDS = 30.0
LIVE_WAIT_TIMEOUT_SECONDS = 20 * 60.0

# How the stage reads a URL: the problem it finds there, or None when the URL
# serves the release.
Probe = Callable[[httpx.Client, str], str | None]


@dataclass(frozen=True)
class LiveCheck:
    """A URL users read, what it serves once the release is live (for the log
    and the error), and the probe that tells it does."""

    url: str
    content: str
    probe: Probe


def archive_download_problem(
    client: httpx.Client, url: str, *, asset_url: str
) -> str | None:
    """None when *url* redirects through *asset_url*, the GitHub download of
    a release archive, to a file it can download.

    The body is never read: the status of the last response tells that the
    file is there.
    """
    with client.stream("GET", url) as response:
        hops = [hop.url for hop in response.history] + [response.url]
        status = response.status_code
    # GitHub reads owner and repository names in any case.
    if asset_url.casefold() not in (str(hop).casefold() for hop in hops):
        return (
            f"its redirect chain {_redirect_chain(hops)} (status {status}) does "
            f"not pass through {asset_url}"
        )
    if status != 200:
        return f"its redirect chain {_redirect_chain(hops)} ends with status {status}"
    return None


def _redirect_chain(hops: Sequence[httpx.URL]) -> str:
    """*hops* for the log, without their queries: the signed URL of the file on
    GitHub's storage carries a long one that tells nothing here."""
    return " -> ".join(str(hop.copy_with(query=None, fragment=None)) for hop in hops)


def install_script_problem(
    client: httpx.Client, url: str, *, install_script: bytes
) -> str | None:
    """None when *url* serves *install_script*, the install.sh of the
    release."""
    response = client.get(url)
    if response.status_code != 200:
        return f"it leads to {response.url}, which answers {response.status_code}"
    if response.content != install_script:
        return f"it leads to {response.url}, which serves another install.sh"
    return None


def changelog_problem(
    client: httpx.Client, url: str, *, release_path: str
) -> str | None:
    """None when the changelog page at *url* links to *release_path*, the
    docs page of the release."""
    response = client.get(url)
    if response.status_code != 200:
        return f"it answers {response.status_code}"
    if release_path not in response.text:
        return f"it has no link to {release_path}"
    return None


def release_live_checks(
    tag: str, repository: RepoSlug, install_script: bytes
) -> list[LiveCheck]:
    """Every URL users read the release *tag* of *repository* at, with the
    probe that tells it serves the release. *install_script* is the install.sh
    of the release."""
    checks = [
        LiveCheck(
            url=f"{DOWNLOAD_SITE}/install.sh",
            content=f"the install.sh of {tag}",
            probe=partial(install_script_problem, install_script=install_script),
        )
    ]
    for triple in RELEASE_TRIPLES:
        archive = release_archive_name(triple)
        asset_url = (
            f"https://github.com/{repository.full}/releases/download/{tag}/{archive}"
        )
        for channel in ("latest", tag):
            checks.append(
                LiveCheck(
                    url=f"{DOWNLOAD_SITE}/{channel}/{archive}",
                    content=f"the {tag} archive",
                    probe=partial(archive_download_problem, asset_url=asset_url),
                )
            )
    checks.append(
        LiveCheck(
            url=CHANGELOG_URL,
            content=f"the notes of {tag}",
            probe=partial(changelog_problem, release_path=release_page_path(tag)),
        )
    )
    return checks


def _read_problem(check: LiveCheck, client: httpx.Client) -> str | None:
    try:
        return check.probe(client, check.url)
    except httpx.HTTPError as e:
        return f"reading it failed: {e}"


def wait_until_live(
    checks: Sequence[LiveCheck],
    client: httpx.Client,
    *,
    sleep: Callable[[float], None],
    clock: Callable[[], float],
    poll_interval: float = LIVE_POLL_INTERVAL_SECONDS,
    timeout: float = LIVE_WAIT_TIMEOUT_SECONDS,
) -> None:
    """Read the URL of each check every *poll_interval* seconds until each one
    serves the release.

    A check that passes is not read again, and the log names it. A URL that
    fails to answer counts as one that does not serve the release yet. The
    wait stops with an error once *timeout* seconds have passed. *sleep* and
    *clock* are `time.sleep` and `time.monotonic` outside the tests.
    """
    deadline = clock() + timeout
    waiting = list(checks)
    while True:
        problems: list[tuple[LiveCheck, str]] = []
        for check in waiting:
            problem = _read_problem(check, client)
            if problem is None:
                console.print(
                    f"[green]Live:[/green] {check.url} serves {check.content}.",
                    soft_wrap=True,
                )
                continue
            problems.append((check, problem))
        if not problems:
            return
        if clock() >= deadline:
            raise ReleaseError(_not_live_message(problems, timeout))
        waiting = [check for check, _ in problems]
        sleep(poll_interval)


def _not_live_message(problems: Sequence[tuple[LiveCheck, str]], timeout: float) -> str:
    lines = [
        (
            f"after {timeout / 60:.0f} minutes, users still do not get the "
            f"release from {len(problems)} URL(s):"
        )
    ]
    lines += [
        f"- {check.url} does not serve {check.content}: {problem}."
        for check, problem in problems
    ]
    lines.append(
        "The release is published, and nothing here changes it. Make each URL "
        "serve the release, then re-run this job. docs.peppy.bot is the docs "
        "service on Railway: deploy the latest commit of `main` there. "
        "peppy.bot redirects install.sh and the archives to GitHub with the "
        "rules of docroot-htaccess.txt in the landing-page repository, set by "
        "hand in the .htaccess of its docroot."
    )
    return "\n".join(lines)
