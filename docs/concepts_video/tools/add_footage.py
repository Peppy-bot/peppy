"""Adds one footage file to the project: `pixi run add-footage <file>`.

The command puts the file into the bucket under the key of its content, and
writes its entry in `footage.json`. A file with the name of an entry replaces
that entry, and the content of the entry that it replaces stays in the bucket.
"""

import argparse
from pathlib import Path

from tools import footage, paths


def main() -> None:
    parser = argparse.ArgumentParser(description="Add one footage file to the concepts video.")
    parser.add_argument("file", type=Path, help="the recording, for example opening_two_arms.mp4")
    source: Path = parser.parse_args().file
    if not source.is_file():
        raise SystemExit(f"{source} is not a file.")
    listed = footage.read_footage_list(paths.FOOTAGE_LIST)
    added = footage.upload(source, footage.bucket_client())
    footage.write_footage_list(paths.FOOTAGE_LIST, footage.with_file(listed, added))
    print(f"`{added.name}` is in the bucket `{footage.BUCKET}` at {added.key}, and in {paths.FOOTAGE_LIST.name}.")


if __name__ == "__main__":
    main()
