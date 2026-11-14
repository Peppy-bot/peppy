"""Where the files of the video project are."""

from pathlib import Path

PROJECT_ROOT = Path(__file__).resolve().parent.parent

# The pages of the docs site, which the transcript rests on.
DOCS_ROOT = PROJECT_ROOT.parent / "src" / "content" / "docs"

TRANSCRIPT = PROJECT_ROOT / "transcript.toml"
VOICE_DURATIONS = PROJECT_ROOT / "voice_durations.json"
UPLOAD = PROJECT_ROOT / "upload.json"
FOOTAGE_LIST = PROJECT_ROOT / "footage.json"
PINS = PROJECT_ROOT / "pins.toml"
COMPOSITIONS = PROJECT_ROOT / "compositions"
STYLES = PROJECT_ROOT / "styles"
ASSETS = PROJECT_ROOT / "assets"
NODE_MODULES = PROJECT_ROOT / "node_modules"

# The Chrome, the voice model, the voice of each sentence and the footage.
# The build gets or makes each of these files one time.
CACHE = PROJECT_ROOT / ".cache"
HYPERFRAMES_CACHE = CACHE / "hyperframes"
VOICE_CACHE = CACHE / "voice"
FOOTAGE_CACHE = CACHE / "footage"
ROBOT_MODEL = CACHE / "models" / "openarm_v2.glb"

# What the build writes.
BUILD = PROJECT_ROOT / "build"
COMPILED_PROJECT = BUILD / "project"
VIDEO = BUILD / "concepts_video.mp4"
CAPTIONS = BUILD / "concepts_video.en.srt"
CHAPTERS = BUILD / "concepts_video.chapters.txt"
