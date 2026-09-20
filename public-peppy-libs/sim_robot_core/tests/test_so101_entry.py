"""The SO-101 entry, held to so101_description: the hardware's source of
truth for joint names, the limb vocabulary, the start posture and the
simulated front camera."""

from __future__ import annotations

import pytest
from so101_description import limbs, simulation
from so101_description.units import GRIPPER_NAME, JOINT_NAMES

from sim_robot_core.models import shipped_entry

SO101 = shipped_entry("so101")


def test_the_limbs_are_the_ones_the_robot_answers_to():
    assert SO101.arm_names() == [limbs.ARM_LIMB]
    assert SO101.gripper_names() == [limbs.GRIPPER_LIMB]


def test_the_joints_are_the_descriptions_in_wire_order():
    assert SO101.arms[0].joints == JOINT_NAMES
    assert SO101.grippers[0].joints == (GRIPPER_NAME,)


def test_every_joint_starts_where_the_description_says():
    assert SO101.start_posture == pytest.approx(simulation.start_positions_rad())


def test_the_front_camera_is_the_descriptions():
    (camera,) = SO101.cameras
    described = simulation.front_camera()
    assert (camera.name, camera.parent_link) == (described.name, described.parent_link)
    assert camera.pos == pytest.approx(described.pos)
    assert camera.quat_wxyz == pytest.approx(described.quat_wxyz)
    assert camera.fovy_deg == pytest.approx(described.fovy_deg)
    assert (camera.width, camera.height, camera.fps) == (
        described.width,
        described.height,
        described.fps,
    )
    assert camera.depth is None
