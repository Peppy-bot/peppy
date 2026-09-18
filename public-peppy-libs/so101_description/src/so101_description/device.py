"""The SO-family device boundary over lerobot: calibrated connection, the
flat motor-list read (five joints then the gripper), and the wire-name
convention on either side of it, all shared by both hardware nodes."""

from __future__ import annotations

import math

from so101_description.units import MOTOR_NAMES, NUM_JOINTS


def connect_calibrated(robot, what: str) -> None:
    """Connect a lerobot device without the interactive calibration flow and
    refuse to serve uncalibrated: a headless node must never block on a
    terminal prompt, and an uncalibrated bus is a launch error."""
    robot.connect(calibrate=False)
    if not robot.is_calibrated:
        robot.disconnect()
        raise RuntimeError(
            f"{what} calibration is missing or no longer matches the motors; "
            "run lerobot-calibrate for this id"
        )


def read_positions_validated(hardware) -> tuple[tuple[float, ...], float]:
    """One full position read as (joints_deg, gripper_percent), raising on a
    missing motor or a non-finite value."""
    by_motor = hardware.read_positions()
    values = tuple(float(by_motor[name]) for name in MOTOR_NAMES)
    if not all(math.isfinite(v) for v in values):
        raise ValueError("non-finite position read")
    return values[:NUM_JOINTS], values[NUM_JOINTS]


# The fastest either hardware node may drive the bus. Six STS3215s share one
# 1 Mbaud half-duplex line, and a goal write plus a position read must both
# fit in every cycle. One fact about one bus, so both nodes state this same
# ceiling rather than each deriving its own.
BUS_MAX_RATE_HZ = 1000

# lerobot names a motor's position channel `<motor>.pos`; peppy's wire and
# every node-side map are keyed by the bare motor name.
_POSITION_SUFFIX = ".pos"


def motor_positions(channels: dict[str, float]) -> dict[str, float]:
    """A lerobot observation or action keyed by bare motor name.

    Stripping the suffix can collide (`wrist_flex.pos` and `wrist_flex` name
    the same motor afterwards), and a collision means two readings for one
    joint. Refused rather than resolved: nothing here can know which of them
    the bus actually reported."""
    positions: dict[str, float] = {}
    for key, value in channels.items():
        motor = key.removesuffix(_POSITION_SUFFIX)
        if motor in positions:
            raise ValueError(f"two channels name motor {motor}")
        positions[motor] = value
    return positions


def position_channels(goals_by_motor: dict[str, float]) -> dict[str, float]:
    """Goals keyed the way lerobot's action interface expects them."""
    return {f"{motor}{_POSITION_SUFFIX}": value for motor, value in goals_by_motor.items()}
