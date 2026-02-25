"""Tests for functions.repo module."""

from __future__ import annotations

import os
import subprocess
from pathlib import Path

import pytest

from functions.cli import ReleaseError
from functions.repo import (
    checkout,
    get_current_branch,
    get_head_commit,
    get_repo_root,
    get_tag_commit,
    has_uncommitted_changes,
)


def test_get_repo_root_valid_repo(tmp_repo: Path) -> None:
    original_cwd = os.getcwd()
    try:
        os.chdir(tmp_repo)
        root = get_repo_root()
        assert root == tmp_repo
    finally:
        os.chdir(original_cwd)


def test_get_repo_root_not_a_repo(tmp_path: Path) -> None:
    original_cwd = os.getcwd()
    try:
        os.chdir(tmp_path)
        with pytest.raises(ReleaseError, match="must be run inside a git repository"):
            get_repo_root()
    finally:
        os.chdir(original_cwd)


def test_get_current_branch_on_branch(tmp_repo: Path) -> None:
    original_cwd = os.getcwd()
    try:
        os.chdir(tmp_repo)
        branch = get_current_branch()
        # git init creates either "main" or "master" depending on config
        assert branch is not None
    finally:
        os.chdir(original_cwd)


def test_get_current_branch_detached_head(tmp_repo: Path) -> None:
    original_cwd = os.getcwd()
    try:
        os.chdir(tmp_repo)
        head = subprocess.run(
            ["git", "rev-parse", "HEAD"],
            capture_output=True,
            text=True,
            cwd=tmp_repo,
        ).stdout.strip()
        subprocess.run(
            ["git", "checkout", head],
            capture_output=True,
            cwd=tmp_repo,
        )
        branch = get_current_branch()
        assert branch is None
    finally:
        os.chdir(original_cwd)


def test_has_uncommitted_changes_clean(tmp_repo: Path) -> None:
    original_cwd = os.getcwd()
    try:
        os.chdir(tmp_repo)
        assert has_uncommitted_changes() is False
    finally:
        os.chdir(original_cwd)


def test_has_uncommitted_changes_dirty_unstaged(tmp_repo: Path) -> None:
    original_cwd = os.getcwd()
    try:
        os.chdir(tmp_repo)
        (tmp_repo / "README.md").write_text("modified")
        assert has_uncommitted_changes() is True
    finally:
        os.chdir(original_cwd)


def test_has_uncommitted_changes_dirty_staged(tmp_repo: Path) -> None:
    original_cwd = os.getcwd()
    try:
        os.chdir(tmp_repo)
        (tmp_repo / "new_file.txt").write_text("new")
        subprocess.run(
            ["git", "add", "new_file.txt"],
            cwd=tmp_repo,
            capture_output=True,
        )
        assert has_uncommitted_changes() is True
    finally:
        os.chdir(original_cwd)


def test_get_tag_commit_existing_tag(tmp_repo: Path) -> None:
    original_cwd = os.getcwd()
    try:
        os.chdir(tmp_repo)
        subprocess.run(
            ["git", "tag", "v0.1.0"],
            cwd=tmp_repo,
            capture_output=True,
            check=True,
        )
        commit = get_tag_commit("v0.1.0")
        assert len(commit) == 40  # SHA-1 hex
    finally:
        os.chdir(original_cwd)


def test_get_tag_commit_missing_tag(tmp_repo: Path) -> None:
    original_cwd = os.getcwd()
    try:
        os.chdir(tmp_repo)
        with pytest.raises(ReleaseError, match="not found locally"):
            get_tag_commit("v999.0.0")
    finally:
        os.chdir(original_cwd)


def test_get_head_commit_returns_sha(tmp_repo: Path) -> None:
    original_cwd = os.getcwd()
    try:
        os.chdir(tmp_repo)
        commit = get_head_commit()
        assert len(commit) == 40
    finally:
        os.chdir(original_cwd)


def test_checkout_tag(tmp_repo: Path) -> None:
    original_cwd = os.getcwd()
    try:
        os.chdir(tmp_repo)
        subprocess.run(
            ["git", "tag", "v0.1.0"],
            cwd=tmp_repo,
            capture_output=True,
            check=True,
        )
        checkout("v0.1.0")
        branch = get_current_branch()
        assert branch is None  # detached HEAD at tag
    finally:
        os.chdir(original_cwd)


def test_checkout_nonexistent(tmp_repo: Path) -> None:
    original_cwd = os.getcwd()
    try:
        os.chdir(tmp_repo)
        with pytest.raises(ReleaseError, match="failed to checkout"):
            checkout("nonexistent-ref")
    finally:
        os.chdir(original_cwd)
