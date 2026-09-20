"""One entry per robot model: what a robot of that model is made of, whatever
engine stands it.

An entry names the model's limbs as the robot itself answers to them in
limb_motion and limb_state (left_arm and left_gripper on an OpenArm, arm and
gripper on an SO-101), the joints each limb moves, the cameras the robot
carries with the links they hang from, and the posture the robot starts in.
The entries ship with this package, under models/, so the engines agree on
them by reading the same file.

What an engine alone knows about a model (the file it loads, its gains, its
torque limits, its extras) is that engine's own entry, one file per model
beside the engine's code. `Models.read` pairs the two: an engine stands the
models it has an entry for, and a model with no entry is refused with the
ones it does stand.
"""

from __future__ import annotations

import math
from dataclasses import dataclass
from pathlib import Path
from typing import Optional

import pyjson5

from sim_robot_core.cameras import CameraConfig, parse_cameras
from sim_robot_core.pairs import Held

_ENTRIES_DIR = Path(__file__).parent / "models"
_ENTRY_SUFFIX = ".json5"

# Where a gripper's closed pose sits on each finger joint's range.
#: Closed at the joint's zero and open at the far end of its travel, whichever
#: side of zero that is: the signed travel is lower + upper, which cancels any
#: symmetric slack an importer added around the nominal 0..travel range.
CLOSED_AT_ZERO = "zero"
#: Closed at the joint's lower limit and open at its upper limit, the whole
#: range being the travel.
CLOSED_AT_LOWER_LIMIT = "lower_limit"
_CLOSED_AT = (CLOSED_AT_ZERO, CLOSED_AT_LOWER_LIMIT)

_ENTRY_KEYS = frozenset({"arms", "grippers", "cameras", "start_posture"})
_ARM_KEYS = frozenset({"name", "joints"})
_GRIPPER_KEYS = frozenset({"name", "fingers", "closed_at"})


@dataclass(frozen=True)
class Arm:
    """One arm of a model: the name the robot answers to for it, and the
    joints it moves, in the order its pair carries them."""

    name: str
    joints: tuple[str, ...]


@dataclass(frozen=True)
class Gripper:
    """One gripper of a model: the name the robot answers to for it, the
    finger joints it moves, and where its closed pose sits on their range."""

    name: str
    joints: tuple[str, ...]
    closed_at: str


@dataclass(frozen=True)
class FingerSpan:
    """One finger joint's closed position and its signed travel to fully
    open, so an opening fraction f sits at closed + f * travel."""

    closed: float
    travel: float

    def position(self, opening: float) -> float:
        return self.closed + self.travel * opening

    def opening(self, position: float) -> float:
        return (position - self.closed) / self.travel


def finger_span(joint_name: str, lower: float, upper: float, closed_at: str) -> FingerSpan:
    """The span of a finger joint from its limit range (prismatic metres or
    revolute radians), under the rule the gripper's entry names."""
    if closed_at == CLOSED_AT_LOWER_LIMIT:
        span = FingerSpan(closed=lower, travel=upper - lower)
    elif closed_at == CLOSED_AT_ZERO:
        if not lower <= 0.0 <= upper:
            raise RuntimeError(
                f"finger joint '{joint_name}' range ({lower}, {upper}) does not contain the"
                " closed pose (0)"
            )
        span = FingerSpan(closed=0.0, travel=lower + upper)
    else:
        raise RuntimeError(
            f"finger joint '{joint_name}': closed_at {closed_at!r} is not one of {list(_CLOSED_AT)}"
        )
    if abs(span.travel) <= 1e-9:
        raise RuntimeError(
            f"finger joint '{joint_name}' range ({lower}, {upper}) has no usable travel"
        )
    return span


@dataclass(frozen=True)
class ModelEntry:
    """What a robot of one model is made of."""

    model: str
    arms: tuple[Arm, ...]
    grippers: tuple[Gripper, ...]
    cameras: tuple[CameraConfig, ...]
    # Joint name to the position the robot starts in. Empty for a model that
    # starts where its file puts it.
    start_posture: dict[str, float]

    def arm_names(self) -> list[str]:
        return [arm.name for arm in self.arms]

    def arm_joint_counts(self) -> list[int]:
        return [len(arm.joints) for arm in self.arms]

    def gripper_names(self) -> list[str]:
        return [gripper.name for gripper in self.grippers]

    def arm_joints(self) -> list[str]:
        return [joint for arm in self.arms for joint in arm.joints]

    def finger_joints(self) -> list[str]:
        return [joint for gripper in self.grippers for joint in gripper.joints]

    def joints(self) -> list[str]:
        """Every joint a robot of this model moves."""
        return [*self.arm_joints(), *self.finger_joints()]

    def rgb_camera_names(self) -> list[str]:
        return [camera.name for camera in self.cameras if camera.depth is None]

    def rgbd_camera_names(self) -> list[str]:
        return [camera.name for camera in self.cameras if camera.depth is not None]

    def has(self) -> Held:
        """Everything a robot of this model can hold a pair for."""
        return Held(
            arms=frozenset(self.arm_names()),
            grippers=frozenset(self.gripper_names()),
            rgb_cameras=frozenset(self.rgb_camera_names()),
            rgbd_cameras=frozenset(self.rgbd_camera_names()),
        )

    def holds_every_limb(self, held: Held) -> bool:
        """Whether `held` drives every limb of this model, which is what a
        robot's readiness asks: a setpoint reaches each limb and its state
        comes back."""
        has = self.has()
        return has.arms <= held.arms and has.grippers <= held.grippers

    def foreign(self, held: Held) -> Held:
        """The pairs of `held` that name a limb or a camera this model lacks.
        Such a pair never drives or streams anything, so a robot holding one
        does not stay."""
        has = self.has()
        return Held(
            arms=held.arms - has.arms,
            grippers=held.grippers - has.grippers,
            rgb_cameras=held.rgb_cameras - has.rgb_cameras,
            rgbd_cameras=held.rgbd_cameras - has.rgbd_cameras,
        )

    def mismatch(self, held: Held) -> Optional[str]:
        """Why `held` is not what a robot of this model pairs, naming both
        lists, or None when it is: every limb of the model and nothing the
        model lacks. A camera the model has may go unpaired, since a camera
        nobody views is not rendered."""
        if self.holds_every_limb(held) and self.foreign(held).is_empty():
            return None
        return (
            f"its pairs name {held.describe()}, and the model {self.model} has "
            f"{self.has().describe()}"
        )


def _fail(source: str, reason: str) -> RuntimeError:
    return RuntimeError(f"{source}: {reason}")


def _reject_unknown_keys(source: str, obj: dict, allowed: frozenset, what: str) -> None:
    unknown = sorted(set(obj) - allowed)
    if unknown:
        raise _fail(source, f"{what} has unknown key(s) {unknown}; allowed: {sorted(allowed)}")


def _names(source: str, value, what: str) -> tuple[str, ...]:
    if (
        not isinstance(value, list)
        or not value
        or not all(isinstance(name, str) and name for name in value)
    ):
        raise _fail(source, f"{what} must be a non-empty list of names, got {value!r}")
    return tuple(value)


def _limb_name(source: str, entry, what: str) -> str:
    if not isinstance(entry, dict):
        raise _fail(source, f"{what} entry must be an object, got {entry!r}")
    name = entry.get("name")
    if not isinstance(name, str) or not name:
        raise _fail(source, f"{what} entry missing 'name': {entry}")
    return name


def _parse_arm(source: str, entry) -> Arm:
    name = _limb_name(source, entry, "arm")
    _reject_unknown_keys(source, entry, _ARM_KEYS, f"arm '{name}'")
    return Arm(name=name, joints=_names(source, entry.get("joints"), f"arm '{name}' joints"))


def _parse_gripper(source: str, entry) -> Gripper:
    name = _limb_name(source, entry, "gripper")
    _reject_unknown_keys(source, entry, _GRIPPER_KEYS, f"gripper '{name}'")
    closed_at = entry.get("closed_at")
    if closed_at not in _CLOSED_AT:
        raise _fail(
            source, f"gripper '{name}' closed_at {closed_at!r} is not one of {list(_CLOSED_AT)}"
        )
    return Gripper(
        name=name,
        joints=_names(source, entry.get("fingers"), f"gripper '{name}' fingers"),
        closed_at=closed_at,
    )


def _parse_start_posture(source: str, value, joints: list[str]) -> dict[str, float]:
    if not isinstance(value, dict):
        raise _fail(source, f"start_posture must map joints to positions, got {value!r}")
    unknown = sorted(set(value) - set(joints))
    if unknown:
        raise _fail(source, f"start_posture names joints no limb moves: {unknown}")
    posture: dict[str, float] = {}
    for joint, position in value.items():
        if isinstance(position, bool) or not isinstance(position, (int, float)):
            raise _fail(source, f"start_posture of '{joint}' must be a number, got {position!r}")
        if not math.isfinite(position):
            raise _fail(source, f"start_posture of '{joint}' must be finite, got {position!r}")
        posture[joint] = float(position)
    return posture


def parse_entry(model: str, source: str, raw) -> ModelEntry:
    """Parses one model's entry, refusing anything it does not spell out: a
    typo in an entry would otherwise stand a robot with a limb nothing
    drives."""
    if not isinstance(raw, dict):
        raise _fail(source, f"a model entry must be an object, got {raw!r}")
    _reject_unknown_keys(source, raw, _ENTRY_KEYS, "model entry")
    arms_raw = raw.get("arms", [])
    grippers_raw = raw.get("grippers", [])
    if not isinstance(arms_raw, list) or not isinstance(grippers_raw, list):
        raise _fail(source, "'arms' and 'grippers' must be lists")
    arms = tuple(_parse_arm(source, arm) for arm in arms_raw)
    grippers = tuple(_parse_gripper(source, gripper) for gripper in grippers_raw)
    if not arms and not grippers:
        raise _fail(source, "a model has at least one limb")
    limb_names = [limb.name for limb in (*arms, *grippers)]
    if len(set(limb_names)) != len(limb_names):
        raise _fail(source, f"limb names must be unique, got {sorted(limb_names)}")
    joints = [joint for limb in (*arms, *grippers) for joint in limb.joints]
    if len(set(joints)) != len(joints):
        raise _fail(source, f"a joint belongs to one limb, got {sorted(joints)}")
    return ModelEntry(
        model=model,
        arms=arms,
        grippers=grippers,
        cameras=parse_cameras(source, raw.get("cameras", [])),
        start_posture=_parse_start_posture(source, raw.get("start_posture", {}), joints),
    )


def shipped_models() -> list[str]:
    """The models this package carries an entry for."""
    return sorted(path.name[: -len(_ENTRY_SUFFIX)] for path in _ENTRIES_DIR.glob(f"*{_ENTRY_SUFFIX}"))


def shipped_entry(model: str) -> ModelEntry:
    """The entry this package ships for `model`."""
    path = _ENTRIES_DIR / f"{model}{_ENTRY_SUFFIX}"
    if not path.is_file():
        raise ValueError(
            f"no entry describes a {model!r}: the entries are {', '.join(shipped_models())}"
        )
    return parse_entry(model, str(path), pyjson5.loads(path.read_text()))


@dataclass(frozen=True)
class EngineModel:
    """One model an engine stands: what the robot is made of, and what this
    engine alone knows about it, as its own entry wrote it."""

    entry: ModelEntry
    engine: dict


class Models:
    """The models an engine stands, by the id a robot attaches with."""

    def __init__(self, models: dict[str, EngineModel]) -> None:
        if not models:
            raise RuntimeError("an engine stands at least one model")
        self._models = dict(models)

    @staticmethod
    def read(engine_entries_dir: Path) -> "Models":
        """Pairs every entry of the engine, one `<model>.json5` per model
        under `engine_entries_dir`, with the entry this package ships for
        the same model."""
        paths = sorted(engine_entries_dir.glob(f"*{_ENTRY_SUFFIX}"))
        if not paths:
            raise RuntimeError(f"{engine_entries_dir} holds no model entry")
        models: dict[str, EngineModel] = {}
        for path in paths:
            model = path.name[: -len(_ENTRY_SUFFIX)]
            engine = pyjson5.loads(path.read_text())
            if not isinstance(engine, dict):
                raise _fail(str(path), f"an engine's model entry must be an object, got {engine!r}")
            models[model] = EngineModel(entry=shipped_entry(model), engine=engine)
        return Models(models)

    def names(self) -> list[str]:
        return sorted(self._models)

    def of(self, model: str) -> EngineModel:
        """The model a robot attaches as. An id this engine has no entry for
        names the ones it stands, so a launcher's typo says what to write
        instead."""
        known = self._models.get(model)
        if known is None:
            raise ValueError(
                f"unknown model {model!r}: this engine stands {', '.join(self.names())}"
            )
        return known
