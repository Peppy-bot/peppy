"""The pinned tools and files that the build gets by itself.

pixi.toml and package.json pin Node.js, FFmpeg, Python, HyperFrames, GSAP and
three.js. pins.toml pins the Chrome that renders, the model of the voice and
the 3D model of the robot. The build gets each of them into the cache, and
refuses another version.
"""

from dataclasses import dataclass
from importlib import metadata
import json
from pathlib import Path
import subprocess
import tomllib

from tools import files, hyperframes, paths

# The Python packages that turn the model into a voice.
VOICE_PACKAGES = ("kokoro-onnx", "onnxruntime")


class WrongChrome(Exception):
    """HyperFrames renders with another Chrome than the pinned one."""


@dataclass(frozen=True)
class VoiceModelFile:
    # Below the cache of HyperFrames.
    cache_path: str
    url: str
    sha256: str


@dataclass(frozen=True)
class RobotModel:
    url: str
    sha256: str


@dataclass(frozen=True)
class Pins:
    chrome_version: str
    voice_model_files: tuple[VoiceModelFile, ...]
    robot_model: RobotModel


def read_pins(path: Path) -> Pins:
    with path.open("rb") as file:
        document = tomllib.load(file)
    return Pins(
        chrome_version=document["chrome"]["version"],
        voice_model_files=tuple(
            VoiceModelFile(cache_path=entry["cache_path"], url=entry["url"], sha256=entry["sha256"])
            for entry in document["voice_model_file"]
        ),
        robot_model=RobotModel(url=document["robot_model"]["url"], sha256=document["robot_model"]["sha256"]),
    )


def voice_tools_identity(pins: Pins) -> str:
    """What makes the voice: the files of the model and the packages that run it.

    Two builds with the same identity give the same voice for the same text.
    """
    identity = {
        "model_files": {file.cache_path: file.sha256 for file in pins.voice_model_files},
        "packages": {package: metadata.version(package) for package in VOICE_PACKAGES},
    }
    return json.dumps(identity, sort_keys=True)


def ensure_voice_model(pins: Pins) -> None:
    """Get each file of the model that the cache does not hold with its hash."""
    for file in pins.voice_model_files:
        destination = paths.HYPERFRAMES_CACHE / file.cache_path
        if destination.is_file() and files.sha256_of(destination) == file.sha256:
            continue
        print(f"Getting {file.url}")
        files.download(file.url, destination, file.sha256)


def ensure_robot_model(pins: Pins) -> Path:
    """Get the 3D model of the robot when the cache does not hold it with its hash."""
    model = pins.robot_model
    if paths.ROBOT_MODEL.is_file() and files.sha256_of(paths.ROBOT_MODEL) == model.sha256:
        return paths.ROBOT_MODEL
    print(f"Getting {model.url}")
    files.download(model.url, paths.ROBOT_MODEL, model.sha256)
    return paths.ROBOT_MODEL


def ensure_chrome(pins: Pins) -> Path:
    """Get the Chrome of HyperFrames, and give its path when it is the pinned one."""
    hyperframes.run(["browser", "ensure"])
    chrome = Path(hyperframes.output_of(["browser", "path"]).strip().splitlines()[-1])
    reported = subprocess.run([str(chrome), "--version"], check=True, capture_output=True, text=True).stdout
    if pins.chrome_version not in reported.split():
        raise WrongChrome(
            f"HyperFrames renders with `{reported.strip()}`, and pins.toml pins Chrome {pins.chrome_version}"
        )
    return chrome
