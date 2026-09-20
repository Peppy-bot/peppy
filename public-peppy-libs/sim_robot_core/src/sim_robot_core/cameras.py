"""Camera plumbing shared by the simulation engines.

Parses the cameras of a model's entry into validated configs, converts
rendered depth to the z16 wire format, and paces per-camera capture;
everything renderer-specific stays in the engine.
"""

from __future__ import annotations

import math
from dataclasses import dataclass
from typing import Optional

import numpy as np

# Wire constants shared with the real camera nodes: rgb8 color frames and z16
# depth (u16 little-endian millimeters, 0 = invalid), aligned depth-to-color
# because depth renders from the color camera's own pose.
COLOR_ENCODING = "rgb8"
DEPTH_ENCODING = "z16"
DEPTH_UNIT_M_PER_LSB = 0.001
ALIGN_MODE = "depth_to_color"

# The largest value the z16 wire carries, and the deepest depth it can therefore
# express. Both the encoder and the range check derive from the unit declared
# above, so the unit consumers are told is the unit the bytes are in.
_MAX_DEPTH_LSB = 65535.0
_MAX_RANGE_M = _MAX_DEPTH_LSB * DEPTH_UNIT_M_PER_LSB
# stream_info carries frames_per_second as a u8.
_MAX_FPS = 255


@dataclass(frozen=True)
class DepthSpec:
    width: int
    height: int
    min_depth_m: float
    max_range_m: float

    def __post_init__(self) -> None:
        if not 0.0 < self.min_depth_m < self.max_range_m <= _MAX_RANGE_M:
            raise ValueError(
                f"depth range ({self.min_depth_m}, {self.max_range_m}) must "
                f"satisfy 0 < min < max <= {_MAX_RANGE_M} (the z16 wire ceiling)"
            )


@dataclass(frozen=True)
class CameraConfig:
    name: str
    parent_link: str
    pos: tuple[float, float, float]
    quat_wxyz: tuple[float, float, float, float]
    fovy_deg: float
    width: int
    height: int
    fps: int
    depth: Optional[DepthSpec]


# Every key each object accepts. A key outside these is a typo, and letting it
# pass would quietly change the stream: `dpeth` yields a colour-only camera
# rather than the rgbd one the file describes.
_CAMERA_KEYS = frozenset(
    {"name", "parent_link", "pos", "quat_wxyz", "fovy_deg", "color", "fps", "depth"}
)
_COLOR_KEYS = frozenset({"width", "height"})
_DEPTH_KEYS = frozenset({"width", "height", "min_depth_m", "max_range_m"})


def _reject_unknown_keys(obj: dict, allowed: frozenset, what: str, fail) -> None:
    unknown = sorted(set(obj) - allowed)
    if unknown:
        raise fail(f"{what} has unknown key(s) {unknown}; allowed: {sorted(allowed)}")


def parse_cameras(source: str, entries) -> tuple[CameraConfig, ...]:
    """Parse and validate the cameras a model's entry lists, failing loudly on
    any bad entry so a typo never silently drops or distorts a stream.
    `source` names the entry in every refusal. A model with no camera lists
    none."""
    if not isinstance(entries, list):
        raise RuntimeError(f"{source}: 'cameras' must be a list, got {entries!r}")
    configs = tuple(_parse_camera(source, entry) for entry in entries)
    names = [config.name for config in configs]
    if len(set(names)) != len(names):
        raise RuntimeError(f"{source} has duplicate camera names: {sorted(names)}")
    return configs


def _parse_camera(source: str, entry) -> CameraConfig:
    if not isinstance(entry, dict):
        raise RuntimeError(f"{source}: camera entry must be an object, got {entry!r}")
    name = entry.get("name")
    if not name or not isinstance(name, str):
        raise RuntimeError(f"{source}: camera entry missing 'name': {entry}")

    def fail(reason: str) -> RuntimeError:
        return RuntimeError(f"{source}: camera '{name}': {reason}")

    _reject_unknown_keys(entry, _CAMERA_KEYS, "camera entry", fail)
    parent_link = entry.get("parent_link")
    if not parent_link or not isinstance(parent_link, str):
        raise fail("missing 'parent_link'")
    pos = _finite_vector(entry.get("pos"), 3, fail, "pos")
    quat = _finite_vector(entry.get("quat_wxyz"), 4, fail, "quat_wxyz")
    norm = math.sqrt(sum(v * v for v in quat))
    if not math.isclose(norm, 1.0, abs_tol=1e-3):
        raise fail(f"quat_wxyz norm {norm} is not 1")
    fovy_deg = entry.get("fovy_deg")
    if not _is_number(fovy_deg) or not 0.0 < float(fovy_deg) < 180.0:
        raise fail(f"fovy_deg {fovy_deg!r} must be in (0, 180)")
    color = entry.get("color")
    if not isinstance(color, dict):
        raise fail(f"color must be an object with width/height, got {color!r}")
    _reject_unknown_keys(color, _COLOR_KEYS, "color", fail)
    width = _positive_int(color.get("width"), fail, "color.width")
    height = _positive_int(color.get("height"), fail, "color.height")
    fps = _positive_int(entry.get("fps"), fail, "fps")
    if fps > _MAX_FPS:
        raise fail(f"fps {fps} does not fit the u8 wire type")

    depth = None
    if "depth" in entry:
        d = entry["depth"]
        if not isinstance(d, dict):
            raise fail(f"depth must be an object, got {d!r}")
        _reject_unknown_keys(d, _DEPTH_KEYS, "depth", fail)
        min_depth_m = d.get("min_depth_m")
        max_range_m = d.get("max_range_m")
        if not _is_number(min_depth_m) or not _is_number(max_range_m):
            raise fail(f"depth range ({min_depth_m!r}, {max_range_m!r}) must be numbers")
        try:
            depth = DepthSpec(
                width=_positive_int(d.get("width"), fail, "depth.width"),
                height=_positive_int(d.get("height"), fail, "depth.height"),
                min_depth_m=float(min_depth_m),
                max_range_m=float(max_range_m),
            )
        except ValueError as exc:
            raise fail(str(exc)) from exc
        # Color and depth describe one camera, so the depth grid must decimate
        # the color grid by a single integer factor on both axes: the two grids
        # then share a principal point and an aspect ratio whichever way an
        # engine produces them.
        if (
            width % depth.width != 0
            or height % depth.height != 0
            or width // depth.width != height // depth.height
        ):
            raise fail(
                f"depth {depth.width}x{depth.height} must subsample color "
                f"{width}x{height} by one integer factor"
            )

    return CameraConfig(
        name=name,
        parent_link=parent_link,
        pos=pos,
        quat_wxyz=quat,
        fovy_deg=float(fovy_deg),
        width=width,
        height=height,
        fps=fps,
        depth=depth,
    )


def _is_number(value) -> bool:
    return isinstance(value, (int, float)) and not isinstance(value, bool)


def _finite_vector(value, length: int, fail, field: str) -> tuple[float, ...]:
    if (
        not isinstance(value, list)
        or len(value) != length
        or not all(_is_number(v) and math.isfinite(v) for v in value)
    ):
        raise fail(f"{field} must be {length} finite numbers, got {value!r}")
    return tuple(float(v) for v in value)


def _positive_int(value, fail, field: str) -> int:
    if not isinstance(value, int) or isinstance(value, bool) or value <= 0:
        raise fail(f"{field} must be a positive integer, got {value!r}")
    return value


def depth_to_z16(depth_m: np.ndarray, spec: DepthSpec) -> bytes:
    """Metric depth (float meters, HxW) to the z16 wire format: little-endian
    u16 counts of DEPTH_UNIT_M_PER_LSB rounded to the nearest, 0 for anything
    non-finite or outside [min_depth_m, max_range_m], clipped away from 0
    otherwise so a valid depth never reads as the invalid marker."""
    if depth_m.shape != (spec.height, spec.width):
        raise ValueError(
            f"depth shape {depth_m.shape} does not match spec {(spec.height, spec.width)}"
        )
    valid = (
        np.isfinite(depth_m)
        & (depth_m >= spec.min_depth_m)
        & (depth_m <= spec.max_range_m)
    )
    lsb = np.rint(depth_m / DEPTH_UNIT_M_PER_LSB)
    return np.where(valid, np.clip(lsb, 1.0, _MAX_DEPTH_LSB), 0.0).astype("<u2").tobytes()


class FrameIdCounter:
    """Wrapping u32 capture counter; an rgbd pair shares one id per capture."""

    def __init__(self) -> None:
        self._next = 0

    def next(self) -> int:
        value = self._next
        self._next = (self._next + 1) & 0xFFFFFFFF
        return value


class FramePacer:
    """Absolute-deadline pacing at a fixed fps against a monotonic clock.
    Deadlines advance by exact periods so the long-run rate holds. The grid
    may trail the clock by under one period, so ticks arriving at the paced
    rate with jitter are all taken rather than every other one; once it
    trails by a full period the schedule resyncs to now instead of bursting
    to catch up."""

    def __init__(self, fps: int) -> None:
        if fps <= 0:
            raise ValueError(f"fps must be positive, got {fps}")
        self._period = 1.0 / fps
        self._deadline: Optional[float] = None

    def seconds_until_due(self, now: float) -> float:
        """Return the remaining wait without claiming a frame or moving the deadline."""
        if self._deadline is None:
            return 0.0
        return max(0.0, self._deadline - now)

    def take_if_due(self, now: float) -> bool:
        """Claim the slot due at time now, advancing the schedule: True exactly
        once per period, and the caller owes that period a frame."""
        if self._deadline is None:
            self._deadline = now + self._period
            return True
        if now < self._deadline:
            return False
        self._deadline += self._period
        if now - self._deadline >= self._period:
            self._deadline = now + self._period
        return True
