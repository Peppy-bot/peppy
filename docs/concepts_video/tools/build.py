"""Builds the video from the files of the repository: `pixi run build`.

The build gets the pinned Chrome, the model of the voice and the footage,
makes the voice and the music, compiles the compositions, renders the MP4
file, and writes the captions and the chapters. It writes each output into
`build/`.

`pixi run build <part id>` builds the video of one part only, into
`build/<part id>.mp4`. A part is one segment, or `splash`: the splash screen
with the first segment after it.

With `--preview`, the build renders a preview: a file with fewer pixels and
fewer frames, which is ready much sooner. It writes the preview beside the
video, into `build/<name>.preview.mp4`.
"""

import argparse
import os
from pathlib import Path
import subprocess

from tools import captions, footage, hyperframes, paths, pins, project, voice
from tools.timing import FRAMES_PER_SECOND, Timeline
from tools.transcript import Transcript
from tools.voice_durations import RecordedDuration, read_voice_durations, write_voice_durations

# The render settings that change the file. `delivery` is the quality of
# HyperFrames for a final file. Browser GPU stays in HyperFrames's `auto` mode
# unless `--cpu` is passed, so Chrome uses a host GPU when detected and falls
# back to SwiftShader otherwise. The render stops on any lint error or warning.
RENDER_OPTIONS = ["--quality", "delivery", "--strict-all"]
# Hardware H.264 encoders that HyperFrames uses with `--gpu` on Linux, in its
# order of preference, with the extra FFmpeg probe options each one needs.
GPU_ENCODER_PROBES: tuple[tuple[str, tuple[str, ...], tuple[str, ...]], ...] = (
    ("h264_nvenc", (), ()),
    ("h264_vaapi", ("-vaapi_device", "/dev/dri/renderD128", "-vf", "format=nv12,hwupload"), ()),
    ("h264_qsv", (), ()),
    ("h264_amf", (), ("-rc", "cqp", "-qp_i", "28", "-qp_p", "28")),
)
GPU_ENCODER_PROBE_TIMEOUT_SECONDS = 10
# HyperFrames caps explicit workers at 24 and budgets 1536 MB of RAM per
# Chrome worker and 1024 + 320 * workers MB of V8 heap. Each worker uses
# roughly two processors alongside the live FFmpeg encoder, so half the
# available processors saturates the CPU without starving the encoder.
MAX_RENDER_WORKERS = 24
CORES_PER_WORKER = 2
MEMORY_MB_PER_WORKER = 1536
NODE_DEFAULT_HEAP_MB = 4096
NODE_HEAP_RESERVED_MB = 1024
NODE_HEAP_MB_PER_WORKER = 320
# The video has 3840 x 2160 pixels and 60 frames in one second. The page of
# the video has 1920 x 1080 pixels, so Chrome draws two pixels across each
# pixel of the page. The timeline has 30 frames in one second, so each frame
# of the timeline is two frames of the video.
VIDEO_FRAMES_PER_SECOND = 60
VIDEO_FORMAT = ["--resolution", "landscape-4k", "--fps", str(VIDEO_FRAMES_PER_SECOND)]
# A preview has the pixels of the page and the frames of the timeline.
PREVIEW_FORMAT = ["--resolution", "landscape", "--fps", str(FRAMES_PER_SECOND)]


def main() -> None:
    arguments = parse_arguments()
    transcript, pinned, footage_list = project.read_project()
    part = None if arguments.part is None else project.part_of(transcript, arguments.part)

    _install_node_packages()
    chrome = pins.ensure_chrome(pinned)
    pins.ensure_voice_model(pinned)
    pins.ensure_robot_model(pinned)
    footage.fetch_absent(footage_list)

    voices = voice.make_sentence_voices(transcript, pins.voice_tools_identity(pinned))
    recorded = read_voice_durations(paths.VOICE_DURATIONS)
    record = voice.record_of(voices, recorded)
    if record != recorded:
        write_voice_durations(paths.VOICE_DURATIONS, record)
        print(f"The voice changed: {paths.VOICE_DURATIONS.name} has the new durations. Commit it.")

    output = video_file(arguments.part, arguments.preview)
    render_format = PREVIEW_FORMAT if arguments.preview else VIDEO_FORMAT
    if part is not None:
        _compile_and_render(part, record, voices, footage_list, chrome, output, render_format, arguments.cpu)
        return
    timeline = _compile_and_render(transcript, record, voices, footage_list, chrome, output, render_format, arguments.cpu)
    paths.CAPTIONS.write_text(captions.subrip_captions(transcript, timeline), encoding="utf-8")
    paths.CHAPTERS.write_text(captions.youtube_chapters(transcript, timeline), encoding="utf-8")
    _report(transcript, timeline, output)


def parse_arguments(arguments: list[str] | None = None) -> argparse.Namespace:
    """The arguments of the command. With no list, they are the arguments of the process."""
    parser = argparse.ArgumentParser(description="Build the concepts video.")
    parser.add_argument("part", nargs="?", help="build the video of this part only: the id of a segment, or `splash`")
    parser.add_argument(
        "--preview",
        action="store_true",
        help="render a preview with 1920 x 1080 pixels and 30 frames in one second, into `build/<name>.preview.mp4`",
    )
    parser.add_argument(
        "--cpu",
        action="store_true",
        help="force software rendering and CPU video encoding instead of using a host GPU",
    )
    return parser.parse_args(arguments)


def video_file(part_id: str | None, is_preview: bool) -> Path:
    """The file that the build writes: the video or the video of one part, or the preview of it.

    A preview has a name of its own, so it never replaces a video.
    """
    name = paths.VIDEO.stem if part_id is None else part_id
    return paths.BUILD / (f"{name}.preview.mp4" if is_preview else f"{name}.mp4")


def total_memory_mb() -> int:
    """Total physical memory of this machine in MiB."""
    pages = os.sysconf("SC_PHYS_PAGES")
    page_size = os.sysconf("SC_PAGE_SIZE")
    return (pages * page_size) // (1024 * 1024)


def render_workers(processors: int | None = None, memory_mb: int | None = None) -> int:
    """How many parallel Chrome workers to run for the render.

    Uses half the available processors so Chrome workers and the live FFmpeg
    encoder saturate all cores without oversubscribing them, bounded by half
    of total memory and by the HyperFrames maximum.
    """
    cpu_count = hyperframes.available_processors() if processors is None else processors
    ram_mb = total_memory_mb() if memory_mb is None else memory_mb
    by_cpu = cpu_count // CORES_PER_WORKER
    by_memory = (ram_mb // 2) // MEMORY_MB_PER_WORKER
    return max(1, min(MAX_RENDER_WORKERS, by_cpu, by_memory))


def node_options_for_workers(workers: int, existing: str | None = None) -> str | None:
    """`NODE_OPTIONS` with a V8 heap limit large enough for `workers`, or `existing` unchanged."""
    required_mb = NODE_HEAP_RESERVED_MB + workers * NODE_HEAP_MB_PER_WORKER
    if required_mb <= NODE_DEFAULT_HEAP_MB:
        return existing
    if existing and "--max-old-space-size" in existing:
        return existing
    flag = f"--max-old-space-size={required_mb}"
    return f"{existing} {flag}".strip() if existing else flag


def has_hardware_video_encoder(ffmpeg: Path | None = None) -> bool:
    """Whether the pinned FFmpeg can encode H.264 on a GPU of this machine."""
    binary = hyperframes.pinned_program("ffmpeg") if ffmpeg is None else ffmpeg
    try:
        listed = subprocess.run(
            [str(binary), "-hide_banner", "-encoders"],
            check=True,
            capture_output=True,
            text=True,
            timeout=GPU_ENCODER_PROBE_TIMEOUT_SECONDS,
        ).stdout
    except (OSError, subprocess.SubprocessError):
        return False
    for encoder, input_options, encoder_options in GPU_ENCODER_PROBES:
        if encoder not in listed:
            continue
        command = [
            str(binary),
            "-hide_banner",
            "-loglevel",
            "error",
            "-f",
            "lavfi",
            "-i",
            "color=size=320x240:rate=1:duration=1",
            "-frames:v",
            "1",
            "-an",
            *input_options,
            "-c:v",
            encoder,
            *encoder_options,
            "-f",
            "null",
            "-",
        ]
        try:
            probe = subprocess.run(
                command,
                check=False,
                capture_output=True,
                timeout=GPU_ENCODER_PROBE_TIMEOUT_SECONDS,
            )
        except (OSError, subprocess.SubprocessError):
            continue
        if probe.returncode == 0:
            return True
    return False


def gpu_render_options(force_cpu: bool, has_gpu_encoder: bool) -> list[str]:
    """HyperFrames GPU flags for this build."""
    if force_cpu:
        return ["--no-browser-gpu"]
    return ["--gpu"] if has_gpu_encoder else []


def _install_node_packages() -> None:
    """Install HyperFrames and GSAP at the versions of package-lock.json."""
    subprocess.run(["npm", "ci", "--no-fund"], cwd=paths.PROJECT_ROOT, check=True)


def _compile_and_render(
    transcript: Transcript,
    record: dict[str, RecordedDuration],
    voices: list[voice.SentenceVoice],
    footage_list: tuple[footage.FootageFile, ...],
    chrome: Path,
    output: Path,
    render_format: list[str],
    force_cpu: bool = False,
) -> Timeline:
    """Compile the video of `transcript` into one project, and render it into `output` in `render_format`."""
    timeline = project.compile_with_media(transcript, record, voices, footage_list, paths.COMPILED_PROJECT)
    workers = render_workers()
    gpu_options = gpu_render_options(force_cpu, False if force_cpu else has_hardware_video_encoder())
    extra_environment = {"PRODUCER_HEADLESS_SHELL_PATH": str(chrome)}
    node_options = node_options_for_workers(workers, os.environ.get("NODE_OPTIONS"))
    if node_options:
        extra_environment["NODE_OPTIONS"] = node_options
    hyperframes.run(
        [
            "render",
            str(paths.COMPILED_PROJECT),
            *RENDER_OPTIONS,
            *gpu_options,
            "--workers",
            str(workers),
            *render_format,
            "--output",
            str(output),
        ],
        extra_environment=extra_environment,
    )
    return timeline


def _report(transcript: Transcript, timeline: Timeline, output: Path) -> None:
    minutes, seconds = divmod(timeline.frames // FRAMES_PER_SECOND, 60)
    print(f"The video is {minutes} min {seconds} s long, in {len(transcript.segments)} chapters.")
    print(f"Video:    {output}")
    print(f"Captions: {paths.CAPTIONS}")
    print(f"Chapters: {paths.CHAPTERS}")


if __name__ == "__main__":
    main()
