"""Setpoint parsing at the wire boundary. Parse, don't validate: a message
either becomes a typed target or a SetpointError naming what was wrong."""

from __future__ import annotations

import math

from control_core_py.runtime import SetpointError

from so101_description.units import NUM_JOINTS


def parse_joint_setpoints(positions, velocities, efforts) -> tuple[float, ...]:
    """The commanded joint positions (rad), or a SetpointError.

    Efforts are rejected outright: the STS3215 has no torque mode, so a
    non-empty efforts vector is a command this hardware cannot honor and
    silently dropping it would misrepresent what the arm executes.
    Velocities are accepted empty or index-aligned and ignored either way:
    the servo's position loop takes no velocity feedforward.
    """
    if len(efforts) != 0:
        raise SetpointError(
            "efforts rejected: the SO-101 is position-controlled and applies no torque feedforward"
        )
    if len(positions) != NUM_JOINTS:
        raise SetpointError(f"expected {NUM_JOINTS} positions, got {len(positions)}")
    if len(velocities) not in (0, NUM_JOINTS):
        raise SetpointError(f"expected 0 or {NUM_JOINTS} velocities, got {len(velocities)}")
    if not all(math.isfinite(p) for p in positions):
        raise SetpointError("non-finite joint position")
    return tuple(float(p) for p in positions)


def parse_gripper_setpoint(opening: float, max_effort: float) -> float:
    """The commanded opening fraction clamped to 0..1, or a SetpointError.

    max_effort is validated but not forwarded: the gripper servo's torque and
    current ceilings are fixed in firmware configuration at connect, so the
    node has no per-command effort knob and reports max_effort 0 on the wire.
    """
    if not math.isfinite(opening):
        raise SetpointError("non-finite gripper opening")
    if not math.isfinite(max_effort) or max_effort < 0.0:
        raise SetpointError("max_effort must be finite and non-negative")
    return min(max(float(opening), 0.0), 1.0)
