"""The Rust release that a cargo workspace needs, and the one a rustc is."""

from __future__ import annotations

import re
import tomllib
from dataclasses import dataclass
from pathlib import Path

from .cli import ReleaseError

# A `rust-version` of a manifest (`1.100`, `1.100.0`), or the release in the
# first line that `rustc --version` prints (`rustc 1.100.0 (<commit> <date>)`).
_RELEASE = re.compile(r"(\d+)\.(\d+)(?:\.(\d+))?")
_RUSTC_VERSION_LINE = re.compile(rf"rustc ({_RELEASE.pattern})(?:\s|$)")


@dataclass(frozen=True, order=True)
class RustVersion:
    """A Rust release. Releases compare in the order they came out.

    A `rust-version` without a patch number is the first release of its minor
    version, so its patch number is 0.
    """

    major: int
    minor: int
    patch: int

    @classmethod
    def parse(cls, text: str) -> RustVersion:
        """The release that *text* names, as `1.100` or as `1.100.0`.

        Raises ReleaseError for any other text.
        """
        release = _RELEASE.fullmatch(text)
        if release is None:
            raise ReleaseError(f"'{text}' is not a Rust release, such as 1.100.0")
        major, minor, patch = release.groups()
        return cls(int(major), int(minor), int(patch or 0))

    @classmethod
    def of_rustc(cls, version_output: str) -> RustVersion:
        """The release in what `rustc --version` printed.

        Raises ReleaseError when the output names no release.
        """
        line = _RUSTC_VERSION_LINE.match(version_output)
        if line is None:
            raise ReleaseError(
                f"`rustc --version` printed no Rust release: {version_output.strip()!r}"
            )
        return cls.parse(line.group(1))

    def __str__(self) -> str:
        return f"{self.major}.{self.minor}.{self.patch}"


def workspace_rust_version(manifest: Path) -> RustVersion:
    """The `rust-version` of `[workspace.package]` in the manifest *manifest*.

    Raises ReleaseError when the manifest cannot be read or declares none.
    """
    try:
        document = tomllib.loads(manifest.read_text())
    except (OSError, tomllib.TOMLDecodeError) as unreadable:
        raise ReleaseError(f"cannot read {manifest}: {unreadable}") from unreadable
    rust_version = document.get("workspace", {}).get("package", {}).get("rust-version")
    if not isinstance(rust_version, str):
        raise ReleaseError(
            f"{manifest} declares no rust-version in [workspace.package]"
        )
    return RustVersion.parse(rust_version)
