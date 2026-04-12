"""Docker buildx: build and push base images to Docker Hub."""

from __future__ import annotations

import base64
import json
import subprocess
from dataclasses import dataclass
from pathlib import Path

from .cli import ReleaseError, console, need_cmd, prompt, run_with_error_handling


DOCKER_HUB_ACCOUNT = "tuatini"
DOCKER_PLATFORMS: list[str] = ["linux/amd64", "linux/arm64"]
BUILDX_BUILDER_NAME = "peppy-multiplatform"


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
        repo=f"{DOCKER_HUB_ACCOUNT}/peppy-python-uv-base",
    ),
    BaseImage(
        name="rust-cargo",
        dockerfile_dir="rust_cargo",
        repo=f"{DOCKER_HUB_ACCOUNT}/peppy-rust-cargo-base",
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
        except Exception:
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
        )
    except (FileNotFoundError, subprocess.TimeoutExpired):
        return None

    if result.returncode != 0:
        return None

    try:
        cred = json.loads(result.stdout)
    except (json.JSONDecodeError, ValueError):
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
    )
    if result.returncode != 0:
        return set()

    platforms: set[str] = set()
    for line in result.stdout.splitlines():
        if "Platforms:" in line:
            # Line format: "Platforms: linux/amd64, linux/arm64, ..."
            _, _, value = line.partition("Platforms:")
            for platform in value.split(","):
                # Strip qualifiers like linux/arm64/v8 -> linux/arm64
                parts = platform.strip().split("/")
                if len(parts) >= 2:
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
        console.print(
            f"Creating buildx builder [bold]{BUILDX_BUILDER_NAME}[/bold]..."
        )
        result = subprocess.run(
            [
                "docker", "buildx", "create",
                "--name", BUILDX_BUILDER_NAME,
                "--driver", "docker-container",
                "--bootstrap",
            ],
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
    )
    if result.returncode != 0:
        raise ReleaseError(
            "docker buildx is not available. "
            "Install it or upgrade Docker: https://docs.docker.com/buildx/install/"
        )

    logged_in_user = _get_docker_hub_username()
    if logged_in_user is None:
        raise ReleaseError(
            "not logged into Docker Hub. Run 'docker login' first."
        )

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
    tags: list[str],
    base_images_dir: Path,
) -> None:
    """Build and push a single base image with the given tags."""
    context_dir = base_images_dir / image.dockerfile_dir

    cmd: list[str] = [
        "docker", "buildx", "build",
        "--builder", BUILDX_BUILDER_NAME,
        "--platform", ",".join(DOCKER_PLATFORMS),
        "--push",
    ]
    for tag in tags:
        cmd.extend(["-t", f"{image.repo}:{tag}"])
    cmd.append(str(context_dir))

    console.print(
        f"Building and pushing [bold]{image.repo}[/bold] "
        f"(tags: {', '.join(tags)})..."
    )

    result = subprocess.run(cmd)
    if result.returncode != 0:
        raise ReleaseError(
            f"docker buildx build failed for {image.repo} "
            f"(exit {result.returncode})"
        )


def build_all_base_images(scripts_dir: Path, tag: str) -> None:
    """Build and push all base images with the given release tag."""
    base_images_dir = scripts_dir / "base_images"
    tags = ["latest", tag]

    for image in BASE_IMAGES:
        build_and_push_base_image(image, tags, base_images_dir)

    console.print()
    for image in BASE_IMAGES:
        console.print(f"[green]Pushed:[/green] {image.repo}:{tag}")


def _run() -> None:
    """Interactive entry point: validate, prompt for tag, build and push."""
    validate_docker_environment()

    tag = prompt("Tag for the base images (example: v0.0.1)")
    if not tag:
        raise ReleaseError("tag cannot be empty")

    scripts_dir = Path(__file__).resolve().parent.parent
    build_all_base_images(scripts_dir, tag)


def main() -> None:
    run_with_error_handling(_run)
