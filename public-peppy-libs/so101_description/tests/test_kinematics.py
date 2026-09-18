"""Real placo FK/IK against the mini URDF: the solver wiring, unit
conversions, and the verified-solution gate."""

import math
from pathlib import Path

import numpy as np
import pytest

from so101_description.kinematics import Kinematics
from so101_description.transforms import relative_rotation_rad

URDF = str(Path(__file__).parent / "assets" / "mini_so101.urdf")
HOME = (0.0, 0.0, 0.0, 0.0, 0.0)


@pytest.fixture(scope="module")
def kinematics():
    return Kinematics(URDF)


def test_fk_at_home_matches_link_stack(kinematics):
    position, orientation = kinematics.forward_kinematics(HOME)
    # The chain is a straight stack of z offsets at home.
    assert position == pytest.approx((0.0, 0.0, 0.05 + 0.03 + 0.11 + 0.10 + 0.05 + 0.08))
    assert orientation == pytest.approx((0.0, 0.0, 0.0, 1.0))


def test_fk_varies_with_each_positioning_joint(kinematics):
    # Evaluated at a bent posture: at home the chain is coaxial, so the two
    # z-axis joints (shoulder_pan, wrist_roll) would spin the EE in place.
    bent_home = (0.2, -0.4, 0.6, 0.3, 0.0)
    reference, _ = kinematics.forward_kinematics(bent_home)
    for j in range(4):  # wrist_roll only reorients the EE frame on this chain
        nudged = tuple(q + (0.3 if i == j else 0.0) for i, q in enumerate(bent_home))
        position, _ = kinematics.forward_kinematics(nudged)
        assert not all(
            math.isclose(a, b, abs_tol=1e-9) for a, b in zip(position, reference, strict=True)
        ), f"joint {j} did not move the end effector"


def test_ik_reaches_a_known_fk_pose(kinematics):
    target_joints = (0.3, -0.4, 0.6, 0.2, 0.0)
    position, orientation = kinematics.forward_kinematics(target_joints)
    solution = kinematics.inverse_kinematics(HOME, position, orientation)
    assert solution is not None
    reached, _ = kinematics.forward_kinematics(solution)
    assert math.dist(reached, position) <= 0.01


def test_unreachable_pose_is_refused(kinematics):
    # Twice the arm's full extension.
    solution = kinematics.inverse_kinematics(HOME, (0.0, 0.0, 1.0), (0.0, 0.0, 0.0, 1.0))
    assert solution is None


def test_streaming_solver_steps_toward_a_reachable_pose(kinematics):
    target_joints = (0.3, -0.4, 0.6, 0.2, 0.0)
    position, orientation = kinematics.forward_kinematics(target_joints)
    joints = HOME
    for _ in range(60):  # a second of 60 Hz streaming
        joints = kinematics.inverse_kinematics_streaming(joints, position, orientation)
        assert joints is not None
    reached, _ = kinematics.forward_kinematics(joints)
    assert math.dist(reached, position) <= 0.02


def test_streaming_solver_tracks_the_workspace_boundary(kinematics):
    # Far out of reach: the stream must keep returning a finite best-effort
    # configuration (boundary tracking), never None, never non-finite.
    joints = HOME
    for _ in range(30):
        joints = kinematics.inverse_kinematics_streaming(
            joints, (0.0, 0.0, 1.0), (0.0, 0.0, 0.0, 1.0)
        )
        assert joints is not None
        assert all(math.isfinite(j) for j in joints)


def test_ik_accepts_the_best_orientation_for_an_underactuated_pose(kinematics):
    # Five joints underactuate the three orientation degrees of freedom, so
    # acceptance is positional: a reachable position paired with an
    # orientation the chain cannot meet still solves, landing the position
    # and taking the solver's best orientation.
    target_joints = (0.3, -0.5, 0.7, 0.2, 0.0)
    position, reachable_orientation = kinematics.forward_kinematics(target_joints)
    impossible_orientation = (0.0, 0.0, 0.0, 1.0)
    assert reachable_orientation != pytest.approx(impossible_orientation)
    solution = kinematics.inverse_kinematics(HOME, position, impossible_orientation)
    assert solution is not None
    reached, _ = kinematics.forward_kinematics(solution)
    assert math.dist(reached, position) <= 0.01


def rotated(orientation, axis, degrees):
    """`orientation` turned about one of its own axes."""
    half = math.radians(degrees) / 2.0
    unit = np.array(axis, float) / np.linalg.norm(axis)
    d = np.array([*(unit * math.sin(half)), math.cos(half)])
    x1, y1, z1, w1 = orientation
    x2, y2, z2, w2 = d
    return (
        w1 * x2 + x1 * w2 + y1 * z2 - z1 * y2,
        w1 * y2 - x1 * z2 + y1 * w2 + z1 * x2,
        w1 * z2 + x1 * y2 - y1 * x2 + z1 * w2,
        w1 * w2 - x1 * x2 - y1 * y2 - z1 * z2,
    )


def test_an_impossible_orientation_does_not_cost_the_position(kinematics):
    # Five joints cannot turn the grasp point about its own x axis, and both
    # pose objectives are soft, so the solver will trade position away to
    # chase an orientation it can never reach. A move asks first for a place
    # to be: the position it was given must survive the attempt.
    seed = (0.0, 0.3, 0.4, 0.2, 0.0)
    position, orientation = kinematics.forward_kinematics(seed)
    solution = kinematics.inverse_kinematics(
        seed, position, rotated(orientation, (1, 0, 0), 20)
    )
    assert solution is not None
    reached, _ = kinematics.forward_kinematics(solution)
    assert math.dist(reached, position) < 0.001


def test_an_impossible_orientation_does_not_refuse_a_reachable_position(kinematics):
    # The position here is reachable by construction. Weighted hard enough,
    # the orientation objective drags the solve outside the position
    # tolerance and the caller is told the pose is unreachable, naming the
    # wrong cause.
    seed = (0.0, 0.3, 0.4, 0.2, 0.0)
    position, orientation = kinematics.forward_kinematics(seed)
    assert kinematics.inverse_kinematics(
        seed, position, rotated(orientation, (1, 0, 0), 90)
    ) is not None


def test_a_reachable_orientation_is_still_honoured(kinematics):
    # The orientation weight is small, not absent: dropping it to zero would
    # abandon the wrist orientations this arm can actually hold.
    seed = (0.0, 0.3, 0.4, 0.2, 0.0)
    position, orientation = kinematics.forward_kinematics(seed)
    target = rotated(orientation, (0, 0, 1), 20)
    solution = kinematics.inverse_kinematics(seed, position, target)
    assert solution is not None
    _, reached = kinematics.forward_kinematics(solution)
    assert math.degrees(relative_rotation_rad(reached, target)) < 1.0


def test_the_deployed_arm_still_reaches_an_orientation_it_can_hold():
    # Against the real model, not the mini URDF: the seed already satisfies
    # the requested position, so a solve that stopped at the first step
    # inside the position tolerance would return before the orientation
    # objective had moved the wrist at all.
    from so101_description.model import KINEMATICS_URDF_PATH

    kinematics = Kinematics(KINEMATICS_URDF_PATH)
    seed = (0.0, 0.3, 0.4, 0.2, 0.0)
    position, orientation = kinematics.forward_kinematics(seed)
    target = rotated(orientation, (0, 0, 1), 20)
    solution = kinematics.inverse_kinematics(seed, position, target)
    assert solution is not None
    reached_position, reached_orientation = kinematics.forward_kinematics(solution)
    assert math.degrees(relative_rotation_rad(reached_orientation, target)) < 1.0
    assert math.dist(reached_position, position) < 0.001


def test_a_caller_can_tighten_the_position_bar(kinematics):
    # A bar far below what one linearised step converges to inside the
    # iteration budget must refuse rather than round up to the default.
    target = kinematics.forward_kinematics((0.4, 0.3, -0.5, 0.2, 0.1))
    assert kinematics.inverse_kinematics(HOME, *target) is not None
    assert (
        kinematics.inverse_kinematics(
            HOME, *target, position_tolerance_m=1e-12
        )
        is None
    )


def test_an_orientation_bar_refuses_what_the_wrist_cannot_reach(kinematics):
    # Five joints underactuate three rotational degrees of freedom, so some
    # reachable positions carry an unreachable orientation. The case is found
    # rather than pinned: the descent is mildly path dependent, so a hardcoded
    # pose can stop qualifying when the tests around it change.
    reachable_but_turned = None
    for seed_scale in range(1, 40):
        angle = seed_scale * 0.13
        position = kinematics.forward_kinematics(
            (0.2, 0.3 - angle / 8, angle / 2, 0.1, 0.0)
        )[0]
        turned = (
            math.sin(angle),
            math.cos(angle) * math.sin(angle),
            math.cos(angle),
            math.sin(angle / 2),
        )
        norm = math.sqrt(sum(c * c for c in turned))
        turned = tuple(c / norm for c in turned)
        solution = kinematics.inverse_kinematics(HOME, position, turned)
        if solution is None:
            continue
        missed = relative_rotation_rad(
            kinematics.forward_kinematics(solution)[1], turned
        )
        if missed > 0.1:
            reachable_but_turned = (position, turned, missed)
            break
    assert reachable_but_turned is not None, "no position-reachable orientation miss found"
    position, turned, missed = reachable_but_turned
    assert (
        kinematics.inverse_kinematics(
            HOME, position, turned, orientation_tolerance_rad=missed / 2
        )
        is None
    )


def test_a_generous_orientation_bar_still_accepts(kinematics):
    target = kinematics.forward_kinematics((0.3, 0.2, -0.4, 0.1, 0.2))
    assert (
        kinematics.inverse_kinematics(
            HOME, *target, orientation_tolerance_rad=math.pi
        )
        is not None
    )


@pytest.mark.parametrize("bad", [0.0, -0.01, math.nan, math.inf])
def test_an_unusable_tolerance_is_refused_rather_than_defaulted(kinematics, bad):
    target = kinematics.forward_kinematics((0.1, 0.1, -0.2, 0.0, 0.0))
    with pytest.raises(ValueError):
        kinematics.inverse_kinematics(HOME, *target, position_tolerance_m=bad)
    with pytest.raises(ValueError):
        kinematics.inverse_kinematics(
            HOME, *target, orientation_tolerance_rad=bad
        )


def test_the_jacobian_predicts_end_effector_motion(kinematics):
    # The property a speed cap depends on: J @ dq is the twist a joint step
    # produces. Checked against a finite difference small enough that the
    # second-order term is negligible.
    import numpy as np

    q = (0.3, -0.4, 0.6, 0.2, 0.5)
    direction = np.array([1.0, -0.7, 0.4, 0.2, -0.9])
    direction /= np.linalg.norm(direction)
    step = direction * 1e-6
    jacobian = kinematics.jacobian(q)
    assert jacobian.shape == (6, 5)

    predicted = np.linalg.norm((jacobian @ step)[:3])
    here = np.array(kinematics.forward_kinematics(q)[0])
    there = np.array(kinematics.forward_kinematics(tuple(np.array(q) + step))[0])
    assert predicted == pytest.approx(np.linalg.norm(there - here), rel=1e-3)


def test_the_jacobian_is_linear_in_the_step(kinematics):
    # Why the governor uses it rather than differencing forward kinematics:
    # halving the joint step must halve the end-effector speed exactly, so a
    # cap can be met by one division instead of a search.
    import numpy as np

    q = (0.1, -0.6, 0.9, 0.3, -0.2)
    jacobian = kinematics.jacobian(q)
    step = np.array([0.4, -0.3, 0.2, 0.1, -0.5])
    full = np.linalg.norm((jacobian @ step)[:3])
    for scale in (0.5, 0.25, 0.1):
        scaled = np.linalg.norm((jacobian @ (step * scale))[:3])
        assert scaled == pytest.approx(scale * full, rel=1e-12)


def test_the_jacobian_is_taken_at_the_tool_point(kinematics):
    # The trap this closes: Pinocchio's world Jacobian refers the twist to the
    # world origin, and its linear rows are not the tool's velocity. A pure
    # shoulder rotation moves the tool, so the linear part cannot vanish.
    import numpy as np

    q = (0.0, -0.5, 0.8, 0.3, 0.0)
    jacobian = kinematics.jacobian(q)
    pan_only = np.array([1.0, 0.0, 0.0, 0.0, 0.0])
    here = np.array(kinematics.forward_kinematics(q)[0])
    radius = math.hypot(here[0], here[1])
    assert radius > 0.05, "pick a posture where the tool is off the pan axis"
    assert np.linalg.norm((jacobian @ pan_only)[:3]) == pytest.approx(radius, rel=1e-3)


def test_asking_for_the_jacobian_does_not_disturb_a_streaming_solve():
    # placo's solver carries state between solves. An earlier version of
    # jacobian() moved the model and left it moved, which diverged an
    # interleaved streaming solve by radians while every other test passed.
    import numpy as np

    def run(interleave: bool):
        kinematics = Kinematics(URDF)
        seed, out = HOME, []
        for step in range(8):
            target = (0.2 + step * 0.05, -0.3, 0.5, 0.1, 0.0)
            position, orientation = kinematics.forward_kinematics(target)
            if interleave:
                kinematics.jacobian((0.9, -0.2, 0.4, 0.1, 0.3))
            seed = kinematics.inverse_kinematics_streaming(seed, position, orientation)
            out.append(seed)
        return out

    for undisturbed, interleaved in zip(run(False), run(True), strict=True):
        assert np.allclose(undisturbed, interleaved, atol=1e-12)
