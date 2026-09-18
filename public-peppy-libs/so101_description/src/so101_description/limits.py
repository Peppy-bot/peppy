"""Joint position limits parsed from the SO-101 URDF at startup.

The backbone owns the URDF, so it owns limit enforcement too: goals beyond a
limit are rejected, streamed targets and IK solutions are clamped or refused,
and postures are validated at launch. Parse, don't validate: a URDF missing a
named revolute joint or its limit fails the launch.
"""

from __future__ import annotations

import math
import xml.etree.ElementTree as ET
from dataclasses import dataclass

from so101_description.units import JOINT_NAMES


@dataclass(frozen=True)
class JointLimits:
    lower: tuple[float, ...]
    upper: tuple[float, ...]

    def contains(self, positions: tuple[float, ...]) -> bool:
        if len(positions) != len(self.lower):
            return False
        return all(
            lo <= p <= hi
            for p, lo, hi in zip(positions, self.lower, self.upper, strict=True)
        )

    def clamp(self, positions: tuple[float, ...]) -> tuple[float, ...]:
        if len(positions) != len(self.lower):
            raise ValueError("expected one position per joint")
        return tuple(
            min(max(p, lo), hi)
            for p, lo, hi in zip(positions, self.lower, self.upper, strict=True)
        )


def from_urdf(urdf_path: str) -> JointLimits:
    """The five joints' position limits (rad), in wire order."""
    # Direct children only: transmission blocks nest limitless <joint> stubs
    # under the same names, and iter() would let them shadow the real joints.
    joints = {
        joint.get("name"): joint
        for joint in ET.parse(urdf_path).getroot().findall("joint")
    }
    lower, upper = [], []
    for name in JOINT_NAMES:
        joint = joints.get(name)
        if joint is None:
            raise ValueError(f"URDF has no joint named {name}")
        limit = joint.find("limit")
        if limit is None or limit.get("lower") is None or limit.get("upper") is None:
            raise ValueError(f"URDF joint {name} declares no position limits")
        lo, hi = float(limit.get("lower")), float(limit.get("upper"))
        if not (math.isfinite(lo) and math.isfinite(hi) and lo < hi):
            raise ValueError(f"URDF joint {name} has unusable limits [{lo}, {hi}]")
        lower.append(lo)
        upper.append(hi)
    return JointLimits(lower=tuple(lower), upper=tuple(upper))
