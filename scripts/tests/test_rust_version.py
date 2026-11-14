"""Tests for functions.rust_version module."""

from __future__ import annotations

from pathlib import Path

import pytest

from functions.cli import ReleaseError
from functions.rust_version import RustVersion, workspace_rust_version


def test_parse_reads_a_release_with_its_patch_number() -> None:
    assert RustVersion.parse("1.100.2") == RustVersion(1, 100, 2)


def test_parse_reads_a_release_without_patch_number_as_its_first_patch() -> None:
    assert RustVersion.parse("1.100") == RustVersion(1, 100, 0)


@pytest.mark.parametrize("text", ["", "1", "stable", "1.100.0-beta.1", "v1.100.0"])
def test_parse_rejects_a_text_that_is_no_release(text: str) -> None:
    with pytest.raises(ReleaseError, match="is not a Rust release"):
        RustVersion.parse(text)


def test_releases_compare_by_number_and_not_as_text() -> None:
    assert RustVersion.parse("1.99.1") < RustVersion.parse("1.100")
    assert RustVersion.parse("1.100.1") > RustVersion.parse("1.100")
    assert RustVersion.parse("1.100.0") >= RustVersion.parse("1.100")


def test_of_rustc_reads_the_release_that_rustc_prints() -> None:
    output = "rustc 1.100.0 (0123456789 2026-11-12)\n"

    assert RustVersion.of_rustc(output) == RustVersion(1, 100, 0)


@pytest.mark.parametrize(
    "output",
    [
        "",
        "bash: rustc: command not found\n",
        "rustc 1.101.0-nightly (abc 2026-11-12)\n",
    ],
)
def test_of_rustc_rejects_an_output_that_names_no_release(output: str) -> None:
    with pytest.raises(ReleaseError, match="printed no Rust release"):
        RustVersion.of_rustc(output)


def test_a_release_is_shown_with_its_three_numbers() -> None:
    assert str(RustVersion.parse("1.100")) == "1.100.0"


def test_workspace_rust_version_reads_the_workspace_package_table(
    tmp_path: Path,
) -> None:
    manifest = tmp_path / "Cargo.toml"
    manifest.write_text('[workspace.package]\nrust-version = "1.100"\n')

    assert workspace_rust_version(manifest) == RustVersion(1, 100, 0)


def test_workspace_rust_version_raises_when_the_manifest_declares_none(
    tmp_path: Path,
) -> None:
    manifest = tmp_path / "Cargo.toml"
    manifest.write_text('[workspace.package]\nedition = "2024"\n')

    with pytest.raises(ReleaseError, match="declares no rust-version"):
        workspace_rust_version(manifest)


def test_workspace_rust_version_raises_when_the_manifest_is_missing(
    tmp_path: Path,
) -> None:
    with pytest.raises(ReleaseError, match="cannot read"):
        workspace_rust_version(tmp_path / "Cargo.toml")


def test_workspace_rust_version_raises_when_the_manifest_is_not_toml(
    tmp_path: Path,
) -> None:
    manifest = tmp_path / "Cargo.toml"
    manifest.write_text("[workspace.package\n")

    with pytest.raises(ReleaseError, match="cannot read"):
        workspace_rust_version(manifest)


@pytest.mark.parametrize("manifest", ["Cargo.toml", "peppy-shared/Cargo.toml"])
def test_each_workspace_of_this_repository_declares_its_rust_version(
    manifest: str,
) -> None:
    repo_root = Path(__file__).resolve().parents[2]

    assert workspace_rust_version(repo_root / manifest) >= RustVersion(1, 100, 0)
