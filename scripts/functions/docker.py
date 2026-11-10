"""Docker buildx: build the base images, push them to Docker Hub, move `latest`.

The base images are what the definition file of a container node starts from.
apt in a base image, and so in every node build on top of it, reads one dated
snapshot of the Ubuntu archive (see base_images/apt_snapshot.sh). A build of
the base images picks the snapshot once, a time SNAPSHOT_AGE before it starts,
and gives it to every image and to every platform.

The images are published in two steps, so that a node can be built from new
images before every node build reads them:

1. `build_all_base_images` pushes each image under a tag of its own, then
   probes each one on each platform with the regular Ubuntu mirrors out of
   reach.
2. `move_latest_to` moves the `latest` tag of each image to that tag. The
   definition files of the hubs name `latest`.
"""

from __future__ import annotations

import base64
import json
import subprocess
from dataclasses import dataclass
from datetime import UTC, datetime, timedelta
from pathlib import Path

from .cli import ReleaseError, console, need_cmd, prompt, run_with_error_handling

DOCKER_HUB_ACCOUNT = "peppybot"
DOCKER_PLATFORMS: list[str] = ["linux/amd64", "linux/arm64"]
BUILDX_BUILDER_NAME = "peppy-multiplatform"
LATEST_TAG = "latest"

# The script of the build context that points apt at the snapshot in the build
# of a base image (`setup`) and checks a published one (`probe`).
APT_SNAPSHOT_SCRIPT = "apt_snapshot.sh"
# The build argument through which a Dockerfile receives the snapshot id.
APT_SNAPSHOT_BUILD_ARG = "APT_SNAPSHOT"
# The label in which a base image holds its snapshot id.
APT_SNAPSHOT_LABEL = "bot.peppy.apt-snapshot"
_APT_SNAPSHOT_ID_FORMAT = "%Y%m%dT%H%M%SZ"

# How far before the start of a build its snapshot lies. The snapshot service
# files a publication of the archive under the time at which the run that
# published it started, and serves it only once that run ends, which was more
# than ten minutes later when measured. Until then the snapshot of a later
# time lacks that publication: the snapshot of a time close to now can still
# change, and an image built on it would be behind its own snapshot.
SNAPSHOT_AGE = timedelta(hours=1)

# The names of the regular Ubuntu mirrors. The probe of a published image
# resolves them to an address where nothing answers, so it passes only when
# apt reads the snapshot alone.
UBUNTU_MIRROR_HOSTS: tuple[str, ...] = (
    "archive.ubuntu.com",
    "security.ubuntu.com",
    "ports.ubuntu.com",
)
_ADDRESS_WITHOUT_ANSWER = "127.0.0.1"
_PROBE_SCRIPT_MOUNT = "/run/apt_snapshot.sh"


@dataclass(frozen=True)
class AptSnapshot:
    """One dated snapshot of the Ubuntu archive (https://snapshot.ubuntu.com).

    `id` is the UTC time of the snapshot in the form the service reads,
    YYYYMMDDTHHMMSSZ. Raises ReleaseError for any other text.
    """

    id: str

    def __post_init__(self) -> None:
        try:
            moment = datetime.strptime(self.id, _APT_SNAPSHOT_ID_FORMAT)
        except ValueError:
            moment = None
        # strptime also reads fields that have fewer digits than the form.
        if moment is None or moment.strftime(_APT_SNAPSHOT_ID_FORMAT) != self.id:
            raise ReleaseError(
                f"'{self.id}' is not an Ubuntu snapshot id: "
                "it is a UTC time as YYYYMMDDTHHMMSSZ"
            )

    @classmethod
    def at(cls, moment: datetime) -> AptSnapshot:
        """The snapshot of the archive as it is at *moment*.

        *moment* carries its time zone: a time without one names no moment.
        """
        if moment.tzinfo is None:
            raise ValueError("a snapshot is taken at a time that has a time zone")
        return cls(moment.astimezone(UTC).strftime(_APT_SNAPSHOT_ID_FORMAT))


@dataclass(frozen=True)
class BaseImage:
    """A base Docker image to build and push."""

    name: str
    dockerfile_dir: str
    repo: str


BASE_IMAGES: list[BaseImage] = [
    BaseImage(
        name="python-uv",
        dockerfile_dir="python_uv",
        repo=f"{DOCKER_HUB_ACCOUNT}/python-uv-base",
    ),
    BaseImage(
        name="rust-cargo",
        dockerfile_dir="rust_cargo",
        repo=f"{DOCKER_HUB_ACCOUNT}/rust-cargo-base",
    ),
]


DOCKER_HUB_REGISTRY = "https://index.docker.io/v1/"


def _get_docker_hub_username() -> str | None:
    """Extract the Docker Hub username from ~/.docker/config.json.

    Supports both inline auth entries and external credential stores
    (e.g. osxkeychain, desktop). Returns the username if found, or
    None if not logged in.
    """
    config_path = Path.home() / ".docker" / "config.json"
    if not config_path.is_file():
        return None

    try:
        config = json.loads(config_path.read_text(encoding="utf-8"))
    except (json.JSONDecodeError, OSError):
        return None

    auth_entry = config.get("auths", {}).get(DOCKER_HUB_REGISTRY, {}).get("auth")
    if auth_entry:
        try:
            decoded = base64.b64decode(auth_entry).decode("utf-8")
        except (TypeError, ValueError):
            # TypeError: an entry that is not a string. ValueError: bad
            # base64 (binascii.Error) or bytes that are not UTF-8.
            return None
        username, _, _ = decoded.partition(":")
        return username or None

    creds_store = config.get("credsStore")
    if creds_store and DOCKER_HUB_REGISTRY in config.get("auths", {}):
        return _get_username_from_cred_store(creds_store)

    return None


def _get_username_from_cred_store(creds_store: str) -> str | None:
    """Query a Docker credential store helper for the Hub username."""
    helper = f"docker-credential-{creds_store}"
    try:
        result = subprocess.run(
            [helper, "get"],
            input=DOCKER_HUB_REGISTRY,
            capture_output=True,
            text=True,
            timeout=10,
            check=False,
        )
    except (FileNotFoundError, subprocess.TimeoutExpired):
        return None

    if result.returncode != 0:
        return None

    try:
        cred = json.loads(result.stdout)
    except ValueError:
        return None

    return cred.get("Username") or None


def _inspect_builder_platforms(builder_name: str) -> set[str]:
    """Return the set of platforms supported by a buildx builder.

    Returns an empty set if the builder does not exist.
    """
    result = subprocess.run(
        ["docker", "buildx", "inspect", builder_name],
        capture_output=True,
        text=True,
        check=False,
    )
    if result.returncode != 0:
        return set()

    platforms: set[str] = set()
    for line in result.stdout.splitlines():
        # Line format: "Platforms: linux/amd64, linux/arm64, ..."
        _, found, value = line.partition("Platforms:")
        if not found:
            continue
        for platform in value.split(","):
            # Strip qualifiers like linux/arm64/v8 -> linux/arm64
            parts = platform.strip().split("/")
            if len(parts) < 2:
                continue
            platforms.add(f"{parts[0]}/{parts[1]}")
    return platforms


def _ensure_buildx_builder() -> None:
    """Ensure a multi-platform buildx builder exists with QEMU support.

    Creates the builder if it doesn't exist. Raises ReleaseError if
    the required platforms are not available (typically means QEMU is
    not registered).
    """
    required = set(DOCKER_PLATFORMS)

    platforms = _inspect_builder_platforms(BUILDX_BUILDER_NAME)
    if required <= platforms:
        return

    if not platforms:
        console.print(f"Creating buildx builder [bold]{BUILDX_BUILDER_NAME}[/bold]...")
        result = subprocess.run(
            [
                "docker",
                "buildx",
                "create",
                "--name",
                BUILDX_BUILDER_NAME,
                "--driver",
                "docker-container",
                "--bootstrap",
            ],
            check=False,
        )
        if result.returncode != 0:
            raise ReleaseError(
                f"failed to create buildx builder '{BUILDX_BUILDER_NAME}' "
                f"(exit {result.returncode})"
            )
        platforms = _inspect_builder_platforms(BUILDX_BUILDER_NAME)

    missing = required - platforms
    if missing:
        raise ReleaseError(
            f"buildx builder '{BUILDX_BUILDER_NAME}' is missing platforms: "
            f"{', '.join(sorted(missing))}.\n"
            "Register QEMU with:\n"
            "  docker run --privileged --rm tonistiigi/binfmt --install all\n"
            "Then retry."
        )


def validate_docker_environment() -> None:
    """Check that docker and buildx are available and the user is logged in.

    Raises ReleaseError if docker/buildx is missing or the logged-in
    Docker Hub account does not match DOCKER_HUB_ACCOUNT.
    """
    need_cmd("docker")

    result = subprocess.run(
        ["docker", "buildx", "version"],
        capture_output=True,
        text=True,
        check=False,
    )
    if result.returncode != 0:
        raise ReleaseError(
            "docker buildx is not available. "
            "Install it or upgrade Docker: https://docs.docker.com/buildx/install/"
        )

    logged_in_user = _get_docker_hub_username()
    if logged_in_user is None:
        raise ReleaseError("not logged into Docker Hub. Run 'docker login' first.")

    if logged_in_user != DOCKER_HUB_ACCOUNT:
        raise ReleaseError(
            f"Docker Hub is logged in as '{logged_in_user}', "
            f"expected '{DOCKER_HUB_ACCOUNT}'. "
            f"Run 'docker login' with the correct account."
        )

    console.print(
        f"[green]Docker Hub:[/green] logged in as [bold]{DOCKER_HUB_ACCOUNT}[/bold]"
    )

    _ensure_buildx_builder()
    console.print(
        f"[green]Buildx:[/green] builder [bold]{BUILDX_BUILDER_NAME}[/bold] "
        f"ready for {', '.join(DOCKER_PLATFORMS)}"
    )


def build_and_push_base_image(
    image: BaseImage,
    tag: str,
    snapshot: AptSnapshot,
    base_images_dir: Path,
) -> None:
    """Build one base image on *snapshot* and push it under *tag* alone.

    The build context is *base_images_dir*, which holds the script that the
    Dockerfile of every image shares.
    """
    cmd: list[str] = [
        "docker",
        "buildx",
        "build",
        "--builder",
        BUILDX_BUILDER_NAME,
        "--platform",
        ",".join(DOCKER_PLATFORMS),
        "--push",
        "--build-arg",
        f"{APT_SNAPSHOT_BUILD_ARG}={snapshot.id}",
        "--label",
        f"{APT_SNAPSHOT_LABEL}={snapshot.id}",
        "--file",
        str(base_images_dir / image.dockerfile_dir / "Dockerfile"),
        "-t",
        f"{image.repo}:{tag}",
        str(base_images_dir),
    ]

    console.print(
        f"Building and pushing [bold]{image.repo}:{tag}[/bold] "
        f"on the Ubuntu snapshot {snapshot.id}..."
    )

    result = subprocess.run(cmd, check=False)
    if result.returncode != 0:
        raise ReleaseError(
            f"docker buildx build failed for {image.repo} (exit {result.returncode})"
        )


def probe_base_image(
    image: BaseImage,
    tag: str,
    snapshot: AptSnapshot,
    base_images_dir: Path,
) -> None:
    """Check that the published *image*:*tag* reads *snapshot* alone.

    On each platform, a container of the image pulled from Docker Hub runs the
    probe of the snapshot script: apt names no source but the snapshot, an
    update of the index upgrades no package of the image, and a package
    installs. The names of the regular Ubuntu mirrors resolve to an address
    where nothing answers. Raises ReleaseError when a probe fails.
    """
    reference = f"{image.repo}:{tag}"
    for platform in DOCKER_PLATFORMS:
        cmd: list[str] = [
            "docker",
            "run",
            "--rm",
            "--pull",
            "always",
            "--platform",
            platform,
        ]
        for host in UBUNTU_MIRROR_HOSTS:
            cmd.extend(["--add-host", f"{host}:{_ADDRESS_WITHOUT_ANSWER}"])
        cmd.extend(
            [
                "--volume",
                f"{base_images_dir / APT_SNAPSHOT_SCRIPT}:{_PROBE_SCRIPT_MOUNT}:ro",
                reference,
                "sh",
                _PROBE_SCRIPT_MOUNT,
                "probe",
                snapshot.id,
            ]
        )

        console.print(f"Probing [bold]{reference}[/bold] on {platform}...")

        result = subprocess.run(cmd, check=False)
        if result.returncode != 0:
            raise ReleaseError(
                f"{reference} on {platform} does not install from the Ubuntu "
                f"snapshot {snapshot.id} alone (exit {result.returncode})"
            )


def build_all_base_images(scripts_dir: Path, tag: str, snapshot: AptSnapshot) -> None:
    """Build every base image on *snapshot*, push it under *tag*, and probe it.

    `latest` does not move: see `move_latest_to`.
    """
    base_images_dir = scripts_dir / "base_images"

    for image in BASE_IMAGES:
        build_and_push_base_image(image, tag, snapshot, base_images_dir)

    for image in BASE_IMAGES:
        probe_base_image(image, tag, snapshot, base_images_dir)

    console.print()
    for image in BASE_IMAGES:
        console.print(
            f"[green]Pushed:[/green] {image.repo}:{tag} (Ubuntu snapshot {snapshot.id})"
        )


def read_apt_snapshot(image: BaseImage, tag: str) -> AptSnapshot:
    """Return the snapshot that *image*:*tag* on Docker Hub holds in its label.

    Raises ReleaseError when the tag cannot be read, lacks a platform of
    DOCKER_PLATFORMS, holds no snapshot on one of them, or holds more than
    one snapshot.
    """
    reference = f"{image.repo}:{tag}"
    result = subprocess.run(
        [
            "docker",
            "buildx",
            "imagetools",
            "inspect",
            reference,
            "--format",
            "{{json .Image}}",
        ],
        capture_output=True,
        text=True,
        check=False,
    )
    if result.returncode != 0:
        raise ReleaseError(f"cannot read {reference}: {result.stderr.strip()}")

    try:
        image_of_platform = json.loads(result.stdout)
    except ValueError as e:
        raise ReleaseError(
            f"cannot read the image configuration of {reference}: {e}"
        ) from e
    # buildx keys the images of a multi-platform tag by platform. For a tag
    # of one platform it gives the image itself, which has no such key.
    if not isinstance(image_of_platform, dict):
        raise ReleaseError(f"{reference} is not a multi-platform image")

    snapshots: set[AptSnapshot] = set()
    for platform in DOCKER_PLATFORMS:
        platform_image = image_of_platform.get(platform)
        if not isinstance(platform_image, dict):
            raise ReleaseError(f"{reference} has no {platform}")
        # An image without a label has a null `config` or null `Labels`.
        labels = (platform_image.get("config") or {}).get("Labels") or {}
        snapshot_id = labels.get(APT_SNAPSHOT_LABEL)
        if not isinstance(snapshot_id, str):
            raise ReleaseError(
                f"{reference} on {platform} has no {APT_SNAPSHOT_LABEL} label: "
                "it does not read an Ubuntu snapshot"
            )
        snapshots.add(AptSnapshot(snapshot_id))

    if len(snapshots) != 1:
        found = ", ".join(sorted(snapshot.id for snapshot in snapshots))
        raise ReleaseError(
            f"the platforms of {reference} read different Ubuntu snapshots: {found}"
        )
    return snapshots.pop()


def move_latest_to(tag: str) -> None:
    """Move the `latest` tag of every base image to the image under *tag*.

    Reads the snapshot of every image first, and moves no `latest` unless they
    all read the same one: `latest` names images of one build.
    """
    snapshot_of_repo = {
        image.repo: read_apt_snapshot(image, tag) for image in BASE_IMAGES
    }
    snapshots = set(snapshot_of_repo.values())
    if len(snapshots) != 1:
        found = ", ".join(
            f"{repo}:{tag} reads {snapshot.id}"
            for repo, snapshot in snapshot_of_repo.items()
        )
        raise ReleaseError(
            f"the base images under {tag} read different Ubuntu snapshots "
            f"({found}): build them again"
        )
    snapshot = snapshots.pop()

    for image in BASE_IMAGES:
        result = subprocess.run(
            [
                "docker",
                "buildx",
                "imagetools",
                "create",
                "--tag",
                f"{image.repo}:{LATEST_TAG}",
                f"{image.repo}:{tag}",
            ],
            check=False,
        )
        if result.returncode != 0:
            raise ReleaseError(
                f"cannot move {image.repo}:{LATEST_TAG} to {tag} "
                f"(exit {result.returncode}). Run this again: `{LATEST_TAG}` "
                "of the base images must name one build."
            )

    console.print()
    for image in BASE_IMAGES:
        console.print(
            f"[green]Moved:[/green] {image.repo}:{LATEST_TAG} is {image.repo}:{tag} "
            f"(Ubuntu snapshot {snapshot.id})"
        )


def _utc_now() -> datetime:
    return datetime.now(UTC)


def _require_tag(tag: str | None) -> str:
    """Return *tag*, or the answer to a prompt for one when it is None."""
    if tag is None:
        tag = prompt("Tag of the base images (example: v0.0.1)")
    if not tag:
        raise ReleaseError("tag cannot be empty")
    if tag == LATEST_TAG:
        raise ReleaseError(
            f"'{LATEST_TAG}' is not a tag of its own: "
            "it moves to a tag once nodes build from that tag"
        )
    return tag


def _run_build(tag: str | None) -> None:
    """Build the base images on one snapshot and push them under a tag.

    The snapshot is the archive as it was SNAPSHOT_AGE before this starts.
    """
    validate_docker_environment()
    tag = _require_tag(tag)

    snapshot = AptSnapshot.at(_utc_now() - SNAPSHOT_AGE)
    scripts_dir = Path(__file__).resolve().parent.parent
    build_all_base_images(scripts_dir, tag, snapshot)

    console.print()
    console.print(
        f"Build one Python node and one Rust node from the tag {tag} on each "
        f"platform, then move `{LATEST_TAG}` to it:\n"
        f"  ./scripts/build_release.sh --base-images-latest --tag {tag}"
    )


def _run_move_latest(tag: str | None) -> None:
    """Move `latest` of the base images to a tag that `_run_build` pushed."""
    validate_docker_environment()
    move_latest_to(_require_tag(tag))


def main(tag: str | None = None) -> None:
    """Build, push and probe the base images. Prompts for the tag when None."""
    run_with_error_handling(lambda: _run_build(tag))


def move_latest_main(tag: str | None = None) -> None:
    """Move `latest` of the base images. Prompts for the tag when None."""
    run_with_error_handling(lambda: _run_move_latest(tag))
