#!/usr/bin/env python3
"""Update docs/ to reflect code changes between BASE and HEAD."""

from __future__ import annotations

import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))

from functions.docs import update_main  # noqa: E402


if __name__ == "__main__":
    update_main()
