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


class TestGetRepoRoot:
    def test_valid_repo(self, tmp_repo: Path) -> None:
        original_cwd = os.getcwd()
        try:
            os.chdir(tmp_repo)
            root = get_repo_root()
            assert root == tmp_repo
        finally:
            os.chdir(original_cwd)

    def test_not_a_repo(self, tmp_path: Path) -> None:
        original_cwd = os.getcwd()
        try:
            os.chdir(tmp_path)
            with pytest.raises(ReleaseError, match="must be run inside a git repository"):
                get_repo_root()
        finally:
            os.chdir(original_cwd)


class TestGetCurrentBranch:
    def test_on_branch(self, tmp_repo: Path) -> None:
        original_cwd = os.getcwd()
        try:
            os.chdir(tmp_repo)
            branch = get_current_branch()
            # git init creates either "main" or "master" depending on config
            assert branch is not None
        finally:
            os.chdir(original_cwd)

    def test_detached_head(self, tmp_repo: Path) -> None:
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


class TestHasUncommittedChanges:
    def test_clean(self, tmp_repo: Path) -> None:
        original_cwd = os.getcwd()
        try:
            os.chdir(tmp_repo)
            assert has_uncommitted_changes() is False
        finally:
            os.chdir(original_cwd)

    def test_dirty_unstaged(self, tmp_repo: Path) -> None:
        original_cwd = os.getcwd()
        try:
            os.chdir(tmp_repo)
            (tmp_repo / "README.md").write_text("modified")
            assert has_uncommitted_changes() is True
        finally:
            os.chdir(original_cwd)

    def test_dirty_staged(self, tmp_repo: Path) -> None:
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


class TestGetTagCommit:
    def test_existing_tag(self, tmp_repo: Path) -> None:
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

    def test_missing_tag(self, tmp_repo: Path) -> None:
        original_cwd = os.getcwd()
        try:
            os.chdir(tmp_repo)
            with pytest.raises(ReleaseError, match="not found locally"):
                get_tag_commit("v999.0.0")
        finally:
            os.chdir(original_cwd)


class TestGetHeadCommit:
    def test_returns_sha(self, tmp_repo: Path) -> None:
        original_cwd = os.getcwd()
        try:
            os.chdir(tmp_repo)
            commit = get_head_commit()
            assert len(commit) == 40
        finally:
            os.chdir(original_cwd)


class TestCheckout:
    def test_checkout_tag(self, tmp_repo: Path) -> None:
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

    def test_checkout_nonexistent(self, tmp_repo: Path) -> None:
        original_cwd = os.getcwd()
        try:
            os.chdir(tmp_repo)
            with pytest.raises(ReleaseError, match="failed to checkout"):
                checkout("nonexistent-ref")
        finally:
            os.chdir(original_cwd)
