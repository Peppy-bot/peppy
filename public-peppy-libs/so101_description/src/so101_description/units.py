"""Wire-unit conversions between lerobot device units and pairing contracts.

The device side speaks lerobot's normalized units: calibration-centered
degrees for the five joints, 0..100 percent travel for the gripper. The wire
speaks joint_link radians and gripper_link opening fractions 0..1.
"""

from __future__ import annotations

import math

JOINT_NAMES = (
    "shoulder_pan",
    "shoulder_lift",
    "elbow_flex",
    "wrist_flex",
    "wrist_roll",
)
NUM_JOINTS = len(JOINT_NAMES)
GRIPPER_NAME = "gripper"
MOTOR_NAMES = (*JOINT_NAMES, GRIPPER_NAME)


def joints_deg_from_rad(positions_rad: tuple[float, ...]) -> tuple[float, ...]:
    return tuple(math.degrees(p) for p in positions_rad)


def joints_rad_from_deg(positions_deg: tuple[float, ...]) -> tuple[float, ...]:
    return tuple(math.radians(p) for p in positions_deg)


def gripper_percent_from_fraction(opening: float) -> float:
    return min(max(opening, 0.0), 1.0) * 100.0


def gripper_fraction_from_percent(percent: float) -> float:
    """Encoder readings past calibrated travel must not leave the contract's
    0..1 range, so the measured direction clamps."""
    return min(max(percent / 100.0, 0.0), 1.0)
