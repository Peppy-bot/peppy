"""The lerobot device boundary: calibrated connect, the flat read, and the
channel-name convention on either side of it."""

import pytest

from so101_description import device
from so101_description.units import GRIPPER_NAME, MOTOR_NAMES, NUM_JOINTS


class FakeHardware:
    def __init__(self):
        self.positions = {name: float(i) for i, name in enumerate(MOTOR_NAMES)}

    def read_positions(self):
        return dict(self.positions)


def test_read_positions_validated_orders_and_splits():
    joints, gripper = device.read_positions_validated(FakeHardware())
    assert joints == tuple(float(i) for i in range(NUM_JOINTS))
    assert gripper == float(NUM_JOINTS)


def test_read_positions_validated_rejects_non_finite():
    hardware = FakeHardware()
    hardware.positions["elbow_flex"] = float("nan")
    with pytest.raises(ValueError, match="non-finite"):
        device.read_positions_validated(hardware)


def test_read_positions_validated_rejects_missing_motor():
    hardware = FakeHardware()
    del hardware.positions["gripper"]
    with pytest.raises(KeyError):
        device.read_positions_validated(hardware)


class FakeRobot:
    def __init__(self, calibrated):
        self.is_calibrated = calibrated
        self.connect_calls = []
        self.disconnected = False

    def connect(self, calibrate):
        self.connect_calls.append(calibrate)

    def disconnect(self):
        self.disconnected = True


def test_connect_calibrated_never_enters_the_interactive_flow():
    robot = FakeRobot(calibrated=True)
    device.connect_calibrated(robot, "arm")
    assert robot.connect_calls == [False]


def test_uncalibrated_device_disconnects_and_fails_the_launch():
    robot = FakeRobot(calibrated=False)
    with pytest.raises(RuntimeError, match="calibration is missing"):
        device.connect_calibrated(robot, "arm")
    assert robot.disconnected


def test_lerobot_channel_names_round_trip_to_bare_motor_names():
    # lerobot keys position channels `<motor>.pos`; the wire and every
    # node-side map use the bare name, and this is the only place that knows.
    goals = {"shoulder_pan": 1.0, "gripper": 2.0}
    channels = device.position_channels(goals)
    assert channels == {"shoulder_pan.pos": 1.0, "gripper.pos": 2.0}
    assert device.motor_positions(channels) == goals


def test_a_channel_without_the_suffix_is_left_alone():
    # removesuffix is not a strip: a name lerobot did not decorate must not
    # lose characters that happen to match.
    assert device.motor_positions({"gripper": 1.0}) == {"gripper": 1.0}
    assert device.motor_positions({"pos": 1.0}) == {"pos": 1.0}


@pytest.mark.parametrize(
    "channels",
    [
        {"wrist_flex.pos": 1.0, "wrist_flex": 2.0},
        # Either order collides; neither may depend on which the dict visits
        # first.
        {"wrist_flex": 1.0, "wrist_flex.pos": 2.0},
        {GRIPPER_NAME: 1.0, f"{GRIPPER_NAME}.pos": 2.0},
    ],
)
def test_two_channels_naming_one_motor_are_refused(channels):
    # `.pos` stripping can collide; two readings for one joint cannot be
    # silently reduced to whichever the dict happened to visit last.
    with pytest.raises(ValueError):
        device.motor_positions(channels)
