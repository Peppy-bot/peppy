import math
import xml.etree.ElementTree as ET

from so101_description import limits, postures, simulation
from so101_description.model import KINEMATICS_URDF_PATH
from so101_description.units import GRIPPER_NAME, JOINT_NAMES


def test_a_simulated_arm_starts_in_home_with_its_jaw_closed():
    assert simulation.start_posture_name() == "home"
    assert simulation.start_gripper_opening() == 0.0
    closed, _ = simulation.gripper_range_rad()
    assert simulation.start_positions_rad() == {
        **dict(zip(JOINT_NAMES, postures.HOME_POSITIONS_RAD, strict=True)),
        GRIPPER_NAME: closed,
    }


def test_the_jaw_range_is_the_urdfs():
    assert simulation.gripper_range_rad() == (-0.174533, 1.74533)


def test_the_start_posture_is_inside_the_urdf_limits():
    start = tuple(simulation.start_positions_rad()[name] for name in JOINT_NAMES)
    assert limits.from_urdf(KINEMATICS_URDF_PATH).contains(start)


def test_the_front_camera_hangs_from_a_link_of_the_urdf():
    links = {link.get("name") for link in ET.parse(KINEMATICS_URDF_PATH).getroot().findall("link")}
    assert simulation.front_camera().parent_link in links


def test_the_front_camera_is_the_hardware_stream():
    camera = simulation.front_camera()
    assert camera.name == "front"
    assert (camera.width, camera.height, camera.fps) == (1280, 720, 30)
    assert 0.0 < camera.fovy_deg < 180.0


def test_the_front_camera_looks_back_at_the_workspace():
    camera = simulation.front_camera()
    w, x, y, z = camera.quat_wxyz
    assert math.isclose(math.sqrt(w * w + x * x + y * y + z * z), 1.0, abs_tol=1e-6)
    # The view axis is the camera's -Z, rotated into the base frame.
    view = (-2.0 * (x * z + w * y), -2.0 * (y * z - w * x), -(1.0 - 2.0 * (x * x + y * y)))
    # It stands in front of the arm (+x), so it looks back along -x and down.
    assert camera.pos[0] > 0.0
    assert view[0] < 0.0 and view[2] < 0.0
    assert abs(view[1]) < 1e-6
