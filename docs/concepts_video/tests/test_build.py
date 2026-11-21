"""The format and the file of what the build renders."""

from pathlib import Path
import subprocess

import pytest

from tools import build, paths
from tools.timing import FRAMES_PER_SECOND


def test_each_frame_of_the_timeline_is_a_whole_number_of_frames_of_the_video() -> None:
    assert build.VIDEO_FRAMES_PER_SECOND % FRAMES_PER_SECOND == 0


def test_the_video_and_a_preview_have_one_size_and_one_frame_rate_each() -> None:
    assert build.VIDEO_FORMAT == ["--resolution", "landscape-4k", "--fps", "60"]
    assert build.PREVIEW_FORMAT == ["--resolution", "landscape", "--fps", "30"]


def test_the_build_writes_the_video_and_the_video_of_a_part_into_the_build_directory() -> None:
    assert build.video_file(None, is_preview=False) == paths.VIDEO
    assert build.video_file("splash", is_preview=False) == paths.BUILD / "splash.mp4"


def test_a_preview_has_a_name_of_its_own_and_never_replaces_a_video() -> None:
    assert build.video_file(None, is_preview=True) == paths.BUILD / "concepts_video.preview.mp4"
    assert build.video_file("splash", is_preview=True) == paths.BUILD / "splash.preview.mp4"


def test_the_command_builds_the_full_video_in_its_format_when_it_gets_no_argument() -> None:
    arguments = build.parse_arguments([])

    assert arguments.part is None
    assert arguments.preview is False
    assert arguments.cpu is False


def test_the_command_takes_a_part_and_the_preview_option_in_each_order() -> None:
    for given in (["splash", "--preview"], ["--preview", "splash"]):
        arguments = build.parse_arguments(given)

        assert arguments.part == "splash"
        assert arguments.preview is True
        assert arguments.cpu is False


def test_the_command_accepts_the_cpu_flag() -> None:
    arguments = build.parse_arguments(["--cpu", "opening"])

    assert arguments.part == "opening"
    assert arguments.cpu is True


def test_gpu_render_options_enables_hardware_encoding_only_when_detected_and_not_forced_to_cpu() -> None:
    assert build.gpu_render_options(force_cpu=False, has_gpu_encoder=True) == ["--gpu"]
    assert build.gpu_render_options(force_cpu=False, has_gpu_encoder=False) == []
    assert build.gpu_render_options(force_cpu=True, has_gpu_encoder=True) == ["--no-browser-gpu"]
    assert build.gpu_render_options(force_cpu=True, has_gpu_encoder=False) == ["--no-browser-gpu"]


def test_hardware_encoder_probe_returns_true_when_a_listed_encoder_succeeds(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    calls: list[list[str]] = []

    def fake_run(command: list[str], **kwargs: object) -> subprocess.CompletedProcess[str]:
        calls.append(command)
        if "-encoders" in command:
            return subprocess.CompletedProcess(command, 0, stdout=" V....D h264_nvenc\n V....D h264_vaapi\n", stderr="")
        if "h264_nvenc" in command:
            return subprocess.CompletedProcess(command, 1, stdout="", stderr="no nvidia")
        return subprocess.CompletedProcess(command, 0, stdout="", stderr="")

    monkeypatch.setattr(build.subprocess, "run", fake_run)

    assert build.has_hardware_video_encoder(tmp_path / "ffmpeg") is True
    assert len(calls) == 3
    assert "h264_vaapi" in calls[2]


def test_hardware_encoder_probe_returns_false_when_no_gpu_encoder_works(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    def fake_run(command: list[str], **kwargs: object) -> subprocess.CompletedProcess[str]:
        if "-encoders" in command:
            return subprocess.CompletedProcess(command, 0, stdout=" V....D h264_nvenc\n", stderr="")
        return subprocess.CompletedProcess(command, 1, stdout="", stderr="no gpu")

    monkeypatch.setattr(build.subprocess, "run", fake_run)

    assert build.has_hardware_video_encoder(tmp_path / "ffmpeg") is False


def test_hardware_encoder_probe_returns_false_when_ffmpeg_fails(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    def fake_run(command: list[str], **kwargs: object) -> subprocess.CompletedProcess[str]:
        raise subprocess.TimeoutExpired(command, 10)

    monkeypatch.setattr(build.subprocess, "run", fake_run)

    assert build.has_hardware_video_encoder(tmp_path / "ffmpeg") is False


def test_render_workers_scales_with_processors_and_memory() -> None:
    assert build.render_workers(processors=2, memory_mb=4096) == 1
    assert build.render_workers(processors=8, memory_mb=16384) == 4
    assert build.render_workers(processors=16, memory_mb=65536) == 8
    assert build.render_workers(processors=24, memory_mb=131072) == 12
    assert build.render_workers(processors=24, memory_mb=8192) == 2
    assert build.render_workers(processors=64, memory_mb=262144) == build.MAX_RENDER_WORKERS


def test_node_options_raises_heap_limit_only_when_workers_need_more_than_default() -> None:
    assert build.node_options_for_workers(4) is None
    assert build.node_options_for_workers(4, "--trace-warnings") == "--trace-warnings"
    assert build.node_options_for_workers(12) == "--max-old-space-size=4864"
    assert build.node_options_for_workers(12, "--trace-warnings") == "--trace-warnings --max-old-space-size=4864"
    assert build.node_options_for_workers(12, "--max-old-space-size=8192") == "--max-old-space-size=8192"
