#!/usr/bin/env python3
"""Exit 1 if docs/ is out of date with code changes between BASE and HEAD."""

from __future__ import annotations

import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))

from functions.docs import check_main  # noqa: E402


if __name__ == "__main__":
    check_main()
