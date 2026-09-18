import math

import pytest

from so101_description.setpoints import (
    SetpointError,
    parse_gripper_setpoint,
    parse_joint_setpoints,
)

GOOD = [0.1, -0.2, 0.3, 0.0, 1.0]


def test_accepts_positions_only():
    assert parse_joint_setpoints(GOOD, [], []) == tuple(GOOD)


def test_accepts_aligned_velocities_and_ignores_them():
    assert parse_joint_setpoints(GOOD, [1.0] * 5, []) == tuple(GOOD)


def test_rejects_nonempty_efforts():
    with pytest.raises(SetpointError, match="efforts rejected"):
        parse_joint_setpoints(GOOD, [], [0.1] * 5)


def test_rejects_wrong_position_count():
    with pytest.raises(SetpointError, match="expected 5 positions"):
        parse_joint_setpoints([0.0] * 7, [], [])


def test_rejects_misaligned_velocities():
    with pytest.raises(SetpointError, match="velocities"):
        parse_joint_setpoints(GOOD, [0.0] * 3, [])


@pytest.mark.parametrize("bad", [math.nan, math.inf, -math.inf])
def test_rejects_nonfinite_position(bad):
    with pytest.raises(SetpointError, match="non-finite"):
        parse_joint_setpoints([0.0, 0.0, bad, 0.0, 0.0], [], [])


def test_gripper_clamps_to_unit_range():
    assert parse_gripper_setpoint(1.7, 0.0) == 1.0
    assert parse_gripper_setpoint(-0.2, 0.0) == 0.0
    assert parse_gripper_setpoint(0.5, 0.0) == 0.5


@pytest.mark.parametrize("bad", [math.nan, math.inf])
def test_gripper_rejects_nonfinite_opening(bad):
    with pytest.raises(SetpointError, match="non-finite"):
        parse_gripper_setpoint(bad, 0.0)


def test_gripper_rejects_bad_max_effort():
    with pytest.raises(SetpointError, match="max_effort"):
        parse_gripper_setpoint(0.5, -1.0)
    with pytest.raises(SetpointError, match="max_effort"):
        parse_gripper_setpoint(0.5, math.nan)
