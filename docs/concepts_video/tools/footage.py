"""The footage of the robots and of the simulators.

The footage is a recording, so no command can make it again. The bucket
`concepts-video-assets` of Cloudflare R2 holds each file, and `footage.json`
lists the name, the SHA-256 and the key of each one. The key of a file
contains its SHA-256, so the content at a key never changes.
"""

from dataclasses import dataclass
import json
import os
from pathlib import Path
import re

from tools import files, paths

BUCKET = "concepts-video-assets"
# The variables that give the endpoint of the bucket and a key that reaches it.
ENDPOINT_VARIABLE = "WALDO_R2_JURISDICTION_ENDPOINT"
ACCESS_KEY_ID_VARIABLE = "WALDO_R2_ACCESS_KEY_ID"
SECRET_ACCESS_KEY_VARIABLE = "WALDO_R2_SECRET_ACCESS_KEY"
BUCKET_VARIABLES = (ENDPOINT_VARIABLE, ACCESS_KEY_ID_VARIABLE, SECRET_ACCESS_KEY_VARIABLE)

# A name is safe as a path in the compiled project and as a part of a key.
FOOTAGE_NAME = re.compile(r"^[a-z0-9][a-z0-9_]*\.[a-z0-9]+$")


class FootageError(Exception):
    """The footage list, a footage file or the bucket is not as the project needs it."""


@dataclass(frozen=True)
class FootageFile:
    name: str
    sha256: str
    key: str

    @property
    def cache_path(self) -> Path:
        return paths.FOOTAGE_CACHE / self.sha256 / self.name


def key_of(name: str, sha256: str) -> str:
    return f"{sha256}/{name}"


def read_footage_list(path: Path) -> tuple[FootageFile, ...]:
    document = json.loads(path.read_text(encoding="utf-8"))
    listed = tuple(_parse_entry(entry, index) for index, entry in enumerate(document))
    names = [file.name for file in listed]
    if len(set(names)) != len(names):
        raise FootageError(f"{path.name}: two entries have the same name")
    return listed


def write_footage_list(path: Path, listed: tuple[FootageFile, ...]) -> None:
    document = [{"name": file.name, "sha256": file.sha256, "key": file.key} for file in listed]
    path.write_text(json.dumps(document, indent=2) + "\n", encoding="utf-8")


def with_file(listed: tuple[FootageFile, ...], added: FootageFile) -> tuple[FootageFile, ...]:
    """The list with `added` in the place of the entry of the same name, or at the end."""
    if not any(file.name == added.name for file in listed):
        return listed + (added,)
    return tuple(added if file.name == added.name else file for file in listed)


def bucket_client():
    """A client of the bucket, from the `WALDO_R2_*` variables of the environment."""
    missing = [name for name in BUCKET_VARIABLES if not os.environ.get(name)]
    if missing:
        raise FootageError(f"the footage is in the bucket `{BUCKET}`, and the environment has no {', '.join(missing)}")
    # Only the two commands that reach the bucket need boto3.
    import boto3

    return boto3.client(
        "s3",
        endpoint_url=os.environ[ENDPOINT_VARIABLE],
        aws_access_key_id=os.environ[ACCESS_KEY_ID_VARIABLE],
        aws_secret_access_key=os.environ[SECRET_ACCESS_KEY_VARIABLE],
        region_name="auto",
    )


def is_in_cache(file: FootageFile) -> bool:
    return file.cache_path.is_file() and files.sha256_of(file.cache_path) == file.sha256


def fetch_absent(listed: tuple[FootageFile, ...]) -> None:
    """Get each file of the list that the cache does not hold."""
    absent = [file for file in listed if not is_in_cache(file)]
    if not absent:
        return
    client = bucket_client()
    for file in absent:
        print(f"Getting the footage {file.name}")
        fetch(file, client)


def fetch(file: FootageFile, client) -> None:
    """Get one file of the list into the cache, and refuse content with another hash."""
    file.cache_path.parent.mkdir(parents=True, exist_ok=True)
    partial = file.cache_path.with_name(file.name + ".part")
    client.download_file(BUCKET, file.key, str(partial))
    try:
        files.refuse_wrong_content(partial, file.sha256)
    except files.WrongContent:
        partial.unlink()
        raise
    partial.replace(file.cache_path)


def upload(source: Path, client) -> FootageFile:
    """Put one recording into the bucket under the key of its content, and give its entry."""
    if not FOOTAGE_NAME.match(source.name):
        raise FootageError(
            f"`{source.name}` is not a footage name: use lower-case letters, digits and `_`, then an extension"
        )
    sha256 = files.sha256_of(source)
    file = FootageFile(name=source.name, sha256=sha256, key=key_of(source.name, sha256))
    if not _bucket_holds(client, file.key):
        client.upload_file(str(source), BUCKET, file.key)
    return file


def _bucket_holds(client, key: str) -> bool:
    found = client.list_objects_v2(Bucket=BUCKET, Prefix=key, MaxKeys=1)
    return any(entry["Key"] == key for entry in found.get("Contents", []))


def _parse_entry(entry: object, index: int) -> FootageFile:
    where = f"footage.json, entry {index}"
    if not isinstance(entry, dict) or set(entry) != {"name", "sha256", "key"}:
        raise FootageError(f"{where}: an entry has a `name`, a `sha256` and a `key`")
    file = FootageFile(name=entry["name"], sha256=entry["sha256"], key=entry["key"])
    if not FOOTAGE_NAME.match(file.name):
        raise FootageError(f"{where}: `{file.name}` is not a footage name")
    if file.key != key_of(file.name, file.sha256):
        raise FootageError(f"{where}: the key is not `<sha256>/<name>`")
    return file
