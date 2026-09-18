"""Build (and optionally upload) peppylib wheels for PyPI.

Builds three abi3 wheels (one per platform) tagged with the version from the
mandatory PEPPY_GIT_TAG env var. The same value is read at compile time by the
Rust extension via `option_env!("PEPPY_GIT_TAG")`, so the wheel filename and
the runtime `__version__` stay in lockstep. Because the Rust extension is
built with PyO3's abi3-py311 feature, each platform wheel installs on any
CPython >= 3.11.

Targets:
    macOS arm64        (native, no zig)
    Linux x86_64       (cross via zig)
    Linux aarch64      (cross via zig)

Usage (from this crate's pixi env):
    PEPPY_GIT_TAG=v0.8.2 pixi run release-pypi              # build only
    PEPPY_GIT_TAG=v0.8.2 pixi run release-pypi --upload     # build + upload (needs PYPI_TOKEN)
"""

from __future__ import annotations

import argparse
import os
import re
import resource
import subprocess
import sys
from pathlib import Path

CRATE_DIR = Path(__file__).resolve().parent
WORKSPACE_ROOT = CRATE_DIR.parent.parent
WHEELS_DIR = WORKSPACE_ROOT / "target" / "wheels"
PYPROJECT_FILE = CRATE_DIR / "pyproject.toml"
DYNAMIC_VERSION_LINE = 'dynamic = ["version"]'

LINUX_TARGETS = ("x86_64-unknown-linux-gnu", "aarch64-unknown-linux-gnu")

# Loose PEP 440: X.Y.Z with optional pre/post/dev suffix. The optional 'v'
# prefix is stripped before this check.
PEP440 = re.compile(r"^\d+\.\d+\.\d+(?:(?:a|b|c|rc)\d+|\.post\d+|\.dev\d+)?$")


def resolve_version() -> tuple[str, str]:
    """Return (raw_tag, pep440_version).

    raw_tag is what the rest of the workspace uses (typically `vX.Y.Z`) and
    is what the Rust build reads via `option_env!("PEPPY_GIT_TAG")`.
    pep440_version drives the wheel filename — PyPI rejects a leading 'v'.
    """
    raw = os.environ.get("PEPPY_GIT_TAG", "").strip()
    if not raw:
        sys.exit(
            "PEPPY_GIT_TAG is not set. Set it to the release tag, e.g.\n"
            "    PEPPY_GIT_TAG=v0.8.2 pixi run release-pypi"
        )
    version = raw.removeprefix("v")
    if not PEP440.fullmatch(version):
        sys.exit(
            f"PEPPY_GIT_TAG={raw!r} is not a PyPI-compatible (PEP 440) version.\n"
            "Expected something like 'v0.8.2' or '0.8.2rc1'."
        )
    return raw, version


def write_pyproject_with_version(version: str) -> None:
    """Pin the wheel version by patching `[project]` in pyproject.toml.

    With `dynamic = ["version"]` left in place, maturin falls back to
    Cargo.toml's workspace version (0.0.1). Replacing the dynamic marker
    with an explicit `version = "X.Y.Z"` is the only reliable way to drive
    the wheel filename without mutating the workspace Cargo.toml (which
    would also bump every other crate's runtime version).
    """
    text = PYPROJECT_FILE.read_text()
    if text.count(DYNAMIC_VERSION_LINE) != 1:
        sys.exit(
            f"Expected exactly one occurrence of {DYNAMIC_VERSION_LINE!r} in "
            f"{PYPROJECT_FILE.name}; cannot safely patch."
        )
    PYPROJECT_FILE.write_text(text.replace(DYNAMIC_VERSION_LINE, f'version = "{version}"'))


def raise_fd_limit() -> None:
    """Raise RLIMIT_NOFILE so the zig cross-link doesn't hit ProcessFdQuotaExceeded.

    The Linux x86_64 link step opens hundreds of object files; macOS's default
    soft limit of 256 is far too low. Children inherit the bumped limit.
    """
    soft, hard = resource.getrlimit(resource.RLIMIT_NOFILE)
    if soft >= 8192:
        return
    candidates = [
        hard if hard != resource.RLIM_INFINITY else 524288,
        65536,
        10240,
        8192,
    ]
    for target in candidates:
        if target <= soft:
            continue
        try:
            resource.setrlimit(resource.RLIMIT_NOFILE, (target, hard))
            print(f"Raised RLIMIT_NOFILE: {soft} -> {target}")
            return
        except (ValueError, OSError):
            continue
    print(
        f"WARNING: could not raise RLIMIT_NOFILE above {soft}; "
        "the linker may fail with ProcessFdQuotaExceeded.",
        file=sys.stderr,
    )


def run(cmd: list[str], env: dict[str, str]) -> None:
    print(f"\n>>> {' '.join(cmd)}", flush=True)
    subprocess.run(cmd, cwd=CRATE_DIR, env=env, check=True)


def build_all(version: str) -> list[Path]:
    for stale in WHEELS_DIR.glob(f"peppylib-{version}-*.whl"):
        stale.unlink()

    run(["maturin", "build", "--release"], os.environ.copy())
    for target in LINUX_TARGETS:
        run(
            ["maturin", "build", "--release", "--zig", "--target", target],
            os.environ.copy(),
        )

    wheels = sorted(WHEELS_DIR.glob(f"peppylib-{version}-*.whl"))
    if len(wheels) != 3:
        sys.exit(
            f"Expected 3 wheels for {version}, found {len(wheels)}:\n"
            + "\n".join(f"  {w.name}" for w in wheels)
        )
    return wheels


def upload(wheels: list[Path]) -> None:
    """Publish wheels to PyPI using `uv publish`.

    `maturin upload` is deprecated upstream (PyO3/maturin#2334); the project
    now points users at `uv publish`, which is what we use here.
    """
    token = os.environ.get("PYPI_TOKEN", "").strip()
    if not token:
        sys.exit("PYPI_TOKEN is not set; cannot upload.")
    env = {**os.environ, "UV_PUBLISH_TOKEN": token}
    cmd = ["uv", "publish", *map(str, wheels)]
    print(f"\n>>> uv publish <{len(wheels)} wheels>", flush=True)
    subprocess.run(cmd, env=env, check=True)


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--upload",
        action="store_true",
        help="upload built wheels to PyPI (requires PYPI_TOKEN)",
    )
    args = parser.parse_args()

    raw_tag, version = resolve_version()
    print(f"Releasing peppylib {version} (PEPPY_GIT_TAG={raw_tag})")

    raise_fd_limit()

    original_pyproject = PYPROJECT_FILE.read_text()
    write_pyproject_with_version(version)
    try:
        wheels = build_all(version)
    finally:
        PYPROJECT_FILE.write_text(original_pyproject)

    print("\nBuilt wheels:")
    for w in wheels:
        print(f"  {w}")

    if args.upload:
        upload(wheels)
        print(f"\nUploaded peppylib {version} to PyPI.")


if __name__ == "__main__":
    main()
