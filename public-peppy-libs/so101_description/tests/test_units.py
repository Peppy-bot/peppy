import math

from so101_description import units


def test_joint_round_trip():
    rad = (0.0, 0.5, -1.2, math.pi / 2, -math.pi)
    deg = units.joints_deg_from_rad(rad)
    back = units.joints_rad_from_deg(deg)
    assert all(math.isclose(a, b, abs_tol=1e-12) for a, b in zip(rad, back, strict=True))


def test_gripper_round_trip():
    for fraction in (0.0, 0.25, 1.0):
        assert math.isclose(
            units.gripper_fraction_from_percent(units.gripper_percent_from_fraction(fraction)),
            fraction,
        )


def test_gripper_measured_clamps_out_of_range():
    assert units.gripper_fraction_from_percent(120.0) == 1.0
    assert units.gripper_fraction_from_percent(-5.0) == 0.0


def test_gripper_command_clamps_out_of_range():
    assert units.gripper_percent_from_fraction(1.5) == 100.0
    assert units.gripper_percent_from_fraction(-0.5) == 0.0


def test_joint_order_is_bus_order():
    assert units.MOTOR_NAMES == (*units.JOINT_NAMES, units.GRIPPER_NAME)
    assert units.NUM_JOINTS == 5
