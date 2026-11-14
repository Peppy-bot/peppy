"""The pinned files that the build gets, and the lint findings that are faults."""

import hashlib
import json
from pathlib import Path
import re
import sys

import pytest

from tools import files, hyperframes, paths
from tools.pins import read_pins, voice_tools_identity

SHA256 = re.compile(r"^[0-9a-f]{64}$")


def test_the_pins_give_a_version_of_chrome_and_a_hash_for_each_file() -> None:
    pins = read_pins(paths.PINS)

    assert re.fullmatch(r"\d+\.\d+\.\d+\.\d+", pins.chrome_version)
    assert len(pins.voice_model_files) == 2
    assert all(SHA256.match(file.sha256) and file.url.startswith("https://") for file in pins.voice_model_files)
    assert SHA256.match(pins.robot_model.sha256)


def test_the_key_of_the_robot_model_holds_its_hash() -> None:
    robot_model = read_pins(paths.PINS).robot_model

    assert robot_model.sha256 in robot_model.url


def test_the_identity_of_the_voice_tools_names_the_model_files_and_the_packages() -> None:
    pins = read_pins(paths.PINS)

    identity = json.loads(voice_tools_identity(pins))

    assert identity["model_files"] == {file.cache_path: file.sha256 for file in pins.voice_model_files}
    assert set(identity["packages"]) == {"kokoro-onnx", "onnxruntime"}
    assert voice_tools_identity(pins) == voice_tools_identity(pins)


def test_a_download_writes_the_file_with_the_recorded_hash(tmp_path: Path) -> None:
    source = tmp_path / "source.bin"
    source.write_bytes(b"the content")

    files.download(source.as_uri(), tmp_path / "cache" / "file.bin", hashlib.sha256(b"the content").hexdigest())

    assert (tmp_path / "cache" / "file.bin").read_bytes() == b"the content"


def test_a_download_refuses_content_with_another_hash_and_leaves_no_file(tmp_path: Path) -> None:
    source = tmp_path / "source.bin"
    source.write_bytes(b"other content")

    with pytest.raises(files.WrongContent):
        files.download(source.as_uri(), tmp_path / "cache" / "file.bin", hashlib.sha256(b"the content").hexdigest())

    assert list((tmp_path / "cache").iterdir()) == []


def finding(code: str, severity: str) -> dict:
    return {"code": code, "severity": severity, "file": "index.html", "message": "A message."}


def test_each_error_and_each_warning_of_the_lint_is_a_fault() -> None:
    findings = [finding("clip_ends_past_root_duration", "warning"), finding("media_missing_id", "error"), finding("a_hint", "info")]

    assert hyperframes.lint_faults(findings, media_are_present=True) == findings[:2]


def test_an_absent_media_file_is_a_fault_only_when_the_media_are_present() -> None:
    findings = [finding("audio_src_not_found", "error"), finding("video_src_not_found", "error")]

    assert hyperframes.lint_faults(findings, media_are_present=True) == findings
    assert hyperframes.lint_faults(findings, media_are_present=False) == []


def test_the_number_of_lines_of_a_compiled_composition_is_not_a_fault() -> None:
    findings = [finding("composition_file_too_large", "warning"), finding("clip_ends_past_root_duration", "warning")]

    assert hyperframes.lint_faults(findings, media_are_present=True) == findings[1:]


def test_hyperframes_gets_the_path_of_the_ffmpeg_of_the_pixi_environment() -> None:
    environment = hyperframes.environment()

    pixi_programs = Path(sys.executable).parent
    assert environment["HYPERFRAMES_FFMPEG_PATH"] == str(pixi_programs / "ffmpeg")
    assert environment["HYPERFRAMES_FFPROBE_PATH"] == str(pixi_programs / "ffprobe")
    assert environment["HYPERFRAMES_PYTHON"] == sys.executable


def test_an_environment_with_no_ffmpeg_is_refused(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setattr(sys, "executable", str(tmp_path / "python"))

    with pytest.raises(hyperframes.MissingProgram, match="has no `ffmpeg`"):
        hyperframes.environment()
