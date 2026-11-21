"""The format and the file of what the build renders."""

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


def test_the_command_takes_a_part_and_the_preview_option_in_each_order() -> None:
    for given in (["splash", "--preview"], ["--preview", "splash"]):
        arguments = build.parse_arguments(given)

        assert arguments.part == "splash"
        assert arguments.preview is True


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
