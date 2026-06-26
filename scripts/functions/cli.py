"""Interactive CLI utilities: fatal errors, command checks, prompts, and error handling."""

from __future__ import annotations

import os
import platform
import shutil
import socket
import ssl
import sys
from collections.abc import Callable, Sequence

from rich.console import Console
from rich.prompt import Confirm, Prompt

console = Console(stderr=True)

# Host:port of a live prod per-user-router gateway that presents the prod
# `*.zenoh.<domain>` wildcard certificate. The release gate connects here to
# prove the prod routers are publicly trusted before shipping a system-store-only
# CLI. The gateway is SNI-passthrough, so this must be a routable router/capability
# that actually serves the wildcard leaf (e.g. a stable health router).
PROD_ROUTER_ENDPOINT_ENV = "PEPPY_PROD_ROUTER_ENDPOINT"

RELEASE_TRIPLES: tuple[str, ...] = (
    "aarch64-apple-darwin",
    "x86_64-unknown-linux-gnu",
    "aarch64-unknown-linux-gnu",
)


class ReleaseError(Exception):
    """Raised when the release process encounters a fatal error.

    All error messages are formatted for human consumption (printed to stderr).
    Entry points catch this and call sys.exit(1).
    """


def need_cmd(cmd: str) -> str:
    """Verify that an external command is available on PATH.

    Returns the resolved path to the command.
    Raises ReleaseError if the command is not found.
    """
    path = shutil.which(cmd)
    if path is None:
        raise ReleaseError(f"missing required command: {cmd}")
    return path


def prompt(label: str, default: str = "") -> str:
    """Prompt the user for a string value, with optional default shown in brackets.

    Returns the user's input, or the default if they press enter.
    """
    return Prompt.ask(label, default=default or None) or ""


def prompt_yn(label: str, default_yes: bool = False) -> bool:
    """Prompt the user for a yes/no answer.

    Returns True for yes, False for no. Empty input returns the default.
    """
    return Confirm.ask(label, default=default_yes)


def prompt_choice(label: str, choices: Sequence[str], default: str) -> str:
    """Prompt the user to pick one of a fixed set of choices.

    Returns the chosen value. Empty input returns the default.
    """
    return Prompt.ask(label, choices=list(choices), default=default)


def _probe_publicly_trusted_tls(host: str, port: int, timeout: float = 10.0) -> None:
    """Open a TLS connection to host:port using ONLY the system trust store and
    verify the presented chain is valid, unexpired, and matches `host`.

    This is exactly the validation the shipped (release) CLI does at runtime — it
    bakes in no custom CA and validates router certs against the system store — run
    once at release time. A completed handshake means the chain validated against a
    public root and the name matched. Raises ReleaseError on any failure
    (unreachable, self-signed, private CA, expired, name mismatch).

    Factored out (and using the module-level `socket`/`ssl`) so tests can stub the
    network layer. NOTE: the release host's system trust store must be populated;
    no custom CA is consulted, by design.
    """
    ctx = ssl.create_default_context()  # system roots, verify on, check_hostname on
    try:
        with socket.create_connection((host, port), timeout=timeout) as sock:
            # wrap_socket performs the handshake (and cert + hostname validation)
            # immediately for a connected socket; a clean return means it passed.
            with ctx.wrap_socket(sock, server_hostname=host):
                pass
    except ssl.SSLCertVerificationError as e:
        raise ReleaseError(
            f"prod router {host}:{port} is NOT publicly trusted: {e}. The shipped CLI "
            "validates router certificates against the system trust store only, so the "
            "prod routers must present a publicly-trusted *.zenoh.<domain> certificate "
            "(e.g. issued via ACME/cert-manager) before this release can ship."
        ) from e
    except (ssl.SSLError, OSError) as e:
        raise ReleaseError(
            f"could not verify prod router {host}:{port} over TLS: {e}. It must be a live, "
            "routable endpoint presenting the prod wildcard certificate (the SNI-passthrough "
            f"gateway needs a real router behind {host})."
        ) from e


def verify_prod_router_publicly_trusted() -> None:
    """Release gate: prove the prod per-user routers present a publicly-trusted,
    valid TLS certificate before shipping a system-store-only CLI.

    A release CLI bakes in no custom CA, so it MUST NOT ship against a deployment
    whose routers aren't publicly trusted (it could never federate to them). Reads
    PEPPY_PROD_ROUTER_ENDPOINT (host:port) and validates it against the system trust
    store. Raises ReleaseError if the env var is unset or the endpoint cannot be
    validated.
    """
    endpoint = os.environ.get(PROD_ROUTER_ENDPOINT_ENV, "").strip()
    if not endpoint:
        raise ReleaseError(
            f"{PROD_ROUTER_ENDPOINT_ENV} is required for a prod release: the shipped CLI "
            "trusts only the system trust store, so the release must first prove the prod "
            "routers present a publicly-trusted certificate. Set it to the prod router "
            "gateway host:port (e.g. health.zenoh.<prod-domain>:7443)."
        )
    host, _, port_str = endpoint.rpartition(":")
    if not host or not port_str:
        raise ReleaseError(
            f"{PROD_ROUTER_ENDPOINT_ENV} must be host:port, got {endpoint!r}"
        )
    try:
        port = int(port_str)
    except ValueError:
        raise ReleaseError(
            f"{PROD_ROUTER_ENDPOINT_ENV} has a non-numeric port: {endpoint!r}"
        ) from None
    _probe_publicly_trusted_tls(host, port)


def validate_release_environment(
    required_commands: Sequence[str] = ("git", "cargo", "rustc"),
    *,
    require_token: bool = True,
) -> str:
    """Validate the release environment: check token and required commands.

    When require_token is False (for --local mode), skips token validation
    and returns an empty string for the token.

    For a prod release (require_token=True) this also runs the publicly-trusted
    prod-router gate up front (`verify_prod_router_publicly_trusted`), so a
    system-store-only CLI can never be shipped against a deployment whose routers
    aren't publicly trusted — aborted before anything is built. `--local` skips it
    (require_token=False) and `--base-images` never reaches this function.

    Returns the validated token string (empty if not required).
    Raises ReleaseError if any check fails.
    """
    token = ""
    if require_token:
        # Prove the prod routers are publicly trusted *before* building anything.
        verify_prod_router_publicly_trusted()

        token = os.environ.get("GITHUB_PEPPY_RELEASE_TOKEN", "").strip()
        if not token:
            raise ReleaseError("GITHUB_PEPPY_RELEASE_TOKEN env var is required")

    for cmd in required_commands:
        need_cmd(cmd)

    return token


def run_with_error_handling(fn: Callable[[], None]) -> None:
    """Run a function with standard error handling for release scripts.

    Catches ReleaseError and KeyboardInterrupt, exits with appropriate codes.
    """
    try:
        fn()
    except ReleaseError as e:
        console.print(f"[red]error:[/red] {e}")
        sys.exit(1)
    except KeyboardInterrupt:
        console.print("\n[dim]Aborted.[/dim]")
        sys.exit(130)


def detect_platform() -> tuple[str, str]:
    """Return (os_name, arch) for the current host.

    Examples: ('Darwin', 'arm64'), ('Linux', 'x86_64').
    """
    return (platform.system(), platform.machine())


def is_macos_arm64() -> bool:
    """Return True if running on macOS Apple Silicon."""
    os_name, arch = detect_platform()
    return os_name == "Darwin" and arch == "arm64"


def is_linux() -> bool:
    """Return True if running on Linux."""
    os_name, _ = detect_platform()
    return os_name == "Linux"


def get_native_triple() -> str:
    """Return the Rust target triple for the current host."""
    os_name, arch = detect_platform()
    if os_name == "Darwin" and arch == "arm64":
        return "aarch64-apple-darwin"
    if os_name == "Linux" and arch in ("x86_64", "amd64"):
        return "x86_64-unknown-linux-gnu"
    if os_name == "Linux" and arch in ("aarch64", "arm64"):
        return "aarch64-unknown-linux-gnu"
    raise ReleaseError(f"unsupported platform: {os_name} {arch}")


def get_targets_for_platform() -> list[str]:
    """Return the list of target triples to build for the current platform.

    On macOS ARM64: all 4 release triples.
    On Linux: only the native triple.
    """
    if is_macos_arm64():
        return list(RELEASE_TRIPLES)
    if is_linux():
        return [get_native_triple()]
    raise ReleaseError("unsupported platform (requires macOS ARM64 or Linux)")
