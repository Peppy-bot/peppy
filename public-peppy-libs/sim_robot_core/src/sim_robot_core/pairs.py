"""The four pairing slots of a simulation engine, and what each pair names.

An engine declares one slot per kind of pair, each holding any number of
pairs: `arms` and `grippers` for the limbs it follows, `rgb_cameras` and
`rgbd_cameras` for the cameras it renders. Nothing in a slot's name says
which robot, limb or camera a pair is, so every pair is read for it:

- its robot is the copy the pair carries, the name the robot attached under.
  A pair with no copy belongs to a robot launched outside one, and outside a
  copy there is one robot in the scene for it to belong to;
- a limb pair's limb is the link the pair comes from on the robot's side, so
  a backbone names its downstream links after its limbs (`left_arm`, `arm`);
- a camera pair's camera is its relay's name in the copy (`wrist_left`,
  `front`): the relay's instance id without the copy's prefix.

A new robot, limb or camera therefore changes no engine's interface.
"""

from __future__ import annotations

from dataclasses import dataclass
from typing import Callable, Iterable, Optional

ARMS = "arms"
GRIPPERS = "grippers"
RGB_CAMERAS = "rgb_cameras"
RGBD_CAMERAS = "rgbd_cameras"
LIMB_SLOTS = (ARMS, GRIPPERS)
CAMERA_SLOTS = (RGB_CAMERAS, RGBD_CAMERAS)
SLOTS = (*LIMB_SLOTS, *CAMERA_SLOTS)


def limb_of(member) -> str:
    """The limb a limb pair drives: the link it comes from on the robot's
    side."""
    return member.info.peer_link_id


def camera_of(member) -> str:
    """The camera a camera pair streams: its relay's name in the copy. A
    launch mints a copy's instance ids as `<copy>_<id>`, so the name is the
    id the launcher wrote; a relay outside a copy runs under that id as it
    is. An id that does not carry its copy's prefix is read as it is too:
    it names no camera of any model, so the match refuses the robot naming
    it, where raising here would stop the reading of every other pair."""
    instance_id = member.info.producer.instance_id
    if member.copy is None:
        return instance_id
    prefix = f"{member.copy}_"
    name = instance_id[len(prefix):] if instance_id.startswith(prefix) else ""
    return name or instance_id


_NAME_OF = {
    ARMS: limb_of,
    GRIPPERS: limb_of,
    RGB_CAMERAS: camera_of,
    RGBD_CAMERAS: camera_of,
}


@dataclass(frozen=True)
class Held:
    """What one robot holds, or what one model can hold, on the four slots:
    the limb and camera names, by slot."""

    arms: frozenset = frozenset()
    grippers: frozenset = frozenset()
    rgb_cameras: frozenset = frozenset()
    rgbd_cameras: frozenset = frozenset()

    def of_slot(self, slot: str) -> frozenset:
        return getattr(self, slot)

    def holds_a_limb(self) -> bool:
        return bool(self.arms or self.grippers)

    def is_empty(self) -> bool:
        return not any(self.of_slot(slot) for slot in SLOTS)

    def describe(self) -> str:
        """Every name, slot by slot, for a refusal that names both lists."""
        return ", ".join(f"{slot} {sorted(self.of_slot(slot))}" for slot in SLOTS)


class PairTable:
    """Reads the members of the four slots into robots, limbs and cameras.

    `members` gives, per slot, the callable that returns the slot's live
    members (a generated `peers(node_runner)` bound to the runner), and
    `sole_robot` the name of the only robot in the scene when there is
    exactly one, which is the robot a pair with no copy belongs to."""

    def __init__(
        self,
        members: dict[str, Callable[[], Iterable]],
        sole_robot: Callable[[], Optional[str]],
    ) -> None:
        missing = sorted(set(SLOTS) - set(members))
        if missing:
            raise ValueError(f"the pair table needs every slot, and lacks {missing}")
        self._members = dict(members)
        self._sole_robot = sole_robot

    def _robot_of(self, member) -> Optional[str]:
        return member.copy or self._sole_robot()

    def pair_of(self, slot: str, peer) -> Optional[tuple[str, str]]:
        """The (robot, name) of the pair `peer` is the other end of, or None
        when the slot no longer holds it or no robot answers for it."""
        for member in self._members[slot]():
            if member.info != peer:
                continue
            robot = self._robot_of(member)
            return None if robot is None else (robot, _NAME_OF[slot](member))
        return None

    def peer_of(self, slot: str, robot: str, name: str):
        """The peer of the pair this robot holds for `name` on `slot`, or None
        while it holds none."""
        for member in self._members[slot]():
            if self._robot_of(member) == robot and _NAME_OF[slot](member) == name:
                return member.info
        return None

    def held(self) -> dict[str, Held]:
        """What every robot holding at least one pair holds, by robot."""
        names: dict[str, dict[str, set[str]]] = {}
        for slot in SLOTS:
            for member in self._members[slot]():
                robot = self._robot_of(member)
                if robot is None:
                    continue
                names.setdefault(robot, {}).setdefault(slot, set()).add(_NAME_OF[slot](member))
        return {
            robot: Held(**{slot: frozenset(by_slot.get(slot, ())) for slot in SLOTS})
            for robot, by_slot in names.items()
        }

    def held_by(self, robot: str) -> Held:
        """What `robot` holds, which is nothing for a robot with no pair."""
        return self.held().get(robot, Held())
