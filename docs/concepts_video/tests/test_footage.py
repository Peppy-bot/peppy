"""The footage list, and the bucket that holds the footage."""

import hashlib
import json
from pathlib import Path

import pytest

from tools import files, footage, paths
from tools.footage import FootageError, FootageFile


class FakeBucket:
    """A bucket in memory, with the calls of the S3 client that the project makes."""

    def __init__(self) -> None:
        self.objects: dict[str, bytes] = {}
        self.uploads = 0

    def upload_file(self, source: str, bucket: str, key: str) -> None:
        assert bucket == footage.BUCKET
        self.objects[key] = Path(source).read_bytes()
        self.uploads += 1

    def download_file(self, bucket: str, key: str, destination: str) -> None:
        assert bucket == footage.BUCKET
        Path(destination).write_bytes(self.objects[key])

    def list_objects_v2(self, Bucket: str, Prefix: str, MaxKeys: int) -> dict:
        return {"Contents": [{"Key": key} for key in self.objects if key.startswith(Prefix)][:MaxKeys]}


def recording(folder: Path, name: str = "opening_two_arms.mp4", content: bytes = b"a recording") -> Path:
    file = folder / name
    file.write_bytes(content)
    return file


@pytest.fixture(autouse=True)
def footage_cache(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> Path:
    monkeypatch.setattr(paths, "FOOTAGE_CACHE", tmp_path / "cache")
    return tmp_path / "cache"


def test_the_list_that_a_command_writes_is_the_list_that_the_build_reads(tmp_path: Path) -> None:
    sha256 = "a" * 64
    listed = (FootageFile(name="opening_two_arms.mp4", sha256=sha256, key=f"{sha256}/opening_two_arms.mp4"),)

    footage.write_footage_list(tmp_path / "footage.json", listed)

    assert footage.read_footage_list(tmp_path / "footage.json") == listed


@pytest.mark.parametrize(
    ("entry", "reason"),
    [
        ({"name": "a.mp4", "sha256": "a" * 64}, "an entry has a `name`, a `sha256` and a `key`"),
        ({"name": "../a.mp4", "sha256": "a" * 64, "key": "a" * 64 + "/../a.mp4"}, "is not a footage name"),
        ({"name": "a.mp4", "sha256": "a" * 64, "key": "footage/a.mp4"}, "the key is not `<sha256>/<name>`"),
    ],
)
def test_an_entry_in_another_form_is_refused(tmp_path: Path, entry: dict, reason: str) -> None:
    (tmp_path / "footage.json").write_text(json.dumps([entry]), encoding="utf-8")

    with pytest.raises(FootageError, match=reason):
        footage.read_footage_list(tmp_path / "footage.json")


def test_two_entries_with_one_name_are_refused(tmp_path: Path) -> None:
    entries = [{"name": "a.mp4", "sha256": sha * 64, "key": sha * 64 + "/a.mp4"} for sha in "ab"]
    (tmp_path / "footage.json").write_text(json.dumps(entries), encoding="utf-8")

    with pytest.raises(FootageError, match="two entries have the same name"):
        footage.read_footage_list(tmp_path / "footage.json")


def test_an_upload_puts_the_file_under_the_key_of_its_content(tmp_path: Path) -> None:
    bucket = FakeBucket()

    added = footage.upload(recording(tmp_path), bucket)

    sha256 = hashlib.sha256(b"a recording").hexdigest()
    assert added == FootageFile(name="opening_two_arms.mp4", sha256=sha256, key=f"{sha256}/opening_two_arms.mp4")
    assert bucket.objects == {added.key: b"a recording"}


def test_an_upload_of_content_that_the_bucket_holds_sends_nothing(tmp_path: Path) -> None:
    bucket = FakeBucket()
    footage.upload(recording(tmp_path), bucket)

    footage.upload(recording(tmp_path), bucket)

    assert bucket.uploads == 1


def test_an_upload_refuses_a_name_that_is_not_safe_in_a_path(tmp_path: Path) -> None:
    with pytest.raises(FootageError, match="is not a footage name"):
        footage.upload(recording(tmp_path, name="Opening Two Arms.mp4"), FakeBucket())


def test_a_fetch_puts_the_file_into_the_cache(tmp_path: Path) -> None:
    bucket = FakeBucket()
    added = footage.upload(recording(tmp_path), bucket)
    assert not footage.is_in_cache(added)

    footage.fetch(added, bucket)

    assert footage.is_in_cache(added)
    assert added.cache_path.read_bytes() == b"a recording"


def test_a_fetch_refuses_content_with_another_hash_and_leaves_no_file(tmp_path: Path) -> None:
    bucket = FakeBucket()
    added = footage.upload(recording(tmp_path), bucket)
    bucket.objects[added.key] = b"other content"

    with pytest.raises(files.WrongContent):
        footage.fetch(added, bucket)

    assert list(added.cache_path.parent.iterdir()) == []


def test_a_fetch_of_the_absent_files_gets_each_file_that_the_cache_does_not_hold(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    bucket = FakeBucket()
    cached = footage.upload(recording(tmp_path, "cached.mp4", b"in the cache"), bucket)
    absent = footage.upload(recording(tmp_path, "absent.mp4", b"not in the cache"), bucket)
    footage.fetch(cached, bucket)
    del bucket.objects[cached.key]
    monkeypatch.setattr(footage, "bucket_client", lambda: bucket)

    footage.fetch_absent((cached, absent))

    assert footage.is_in_cache(cached) and footage.is_in_cache(absent)


def test_a_fetch_of_the_absent_files_does_not_reach_the_bucket_when_the_cache_holds_each_file(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    bucket = FakeBucket()
    cached = footage.upload(recording(tmp_path), bucket)
    footage.fetch(cached, bucket)
    for name in footage.BUCKET_VARIABLES:
        monkeypatch.delenv(name, raising=False)

    footage.fetch_absent((cached,))
    footage.fetch_absent(())


def test_a_cached_file_with_another_hash_is_not_in_the_cache(tmp_path: Path) -> None:
    bucket = FakeBucket()
    added = footage.upload(recording(tmp_path), bucket)
    footage.fetch(added, bucket)
    added.cache_path.write_bytes(b"changed on the disk")

    assert not footage.is_in_cache(added)


def test_a_file_with_the_name_of_an_entry_replaces_that_entry() -> None:
    first = FootageFile(name="a.mp4", sha256="a" * 64, key="a" * 64 + "/a.mp4")
    other = FootageFile(name="b.mp4", sha256="b" * 64, key="b" * 64 + "/b.mp4")
    again = FootageFile(name="a.mp4", sha256="c" * 64, key="c" * 64 + "/a.mp4")

    assert footage.with_file((first, other), again) == (again, other)
    assert footage.with_file((first,), other) == (first, other)


def test_the_bucket_client_names_each_variable_that_the_environment_does_not_have(monkeypatch: pytest.MonkeyPatch) -> None:
    for name in footage.BUCKET_VARIABLES:
        monkeypatch.delenv(name, raising=False)
    monkeypatch.setenv(footage.ENDPOINT_VARIABLE, "https://example.invalid")

    with pytest.raises(FootageError, match="WALDO_R2_ACCESS_KEY_ID, WALDO_R2_SECRET_ACCESS_KEY"):
        footage.bucket_client()
