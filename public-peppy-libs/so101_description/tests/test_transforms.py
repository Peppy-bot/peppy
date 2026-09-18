import math

import numpy as np
import pytest

from so101_description.transforms import PoseError, matrix_from_pose, pose_from_matrix

_HALF_TURN_Z = (0.0, 0.0, 1.0, 0.0)
_QUARTER_TURN_X = (math.sin(math.pi / 8), 0.0, 0.0, math.cos(math.pi / 8))


@pytest.mark.parametrize(
    "quat", [(0.0, 0.0, 0.0, 1.0), _HALF_TURN_Z, _QUARTER_TURN_X]
)
def test_round_trip(quat):
    position = (0.1, -0.2, 0.3)
    matrix = matrix_from_pose(position, quat)
    got_position, got_quat = pose_from_matrix(matrix)
    assert np.allclose(got_position, position)
    # q and -q are the same rotation; the round trip keeps w non-negative.
    reference = quat if quat[3] >= 0 else tuple(-c for c in quat)
    assert np.allclose(got_quat, reference, atol=1e-9)


def test_rotation_matrix_is_orthonormal():
    matrix = matrix_from_pose((0, 0, 0), _QUARTER_TURN_X)
    r = matrix[:3, :3]
    assert np.allclose(r @ r.T, np.eye(3), atol=1e-12)
    assert math.isclose(float(np.linalg.det(r)), 1.0, abs_tol=1e-12)


def test_slightly_denormalized_quaternion_is_renormalized():
    scaled = tuple(c * 1.0005 for c in _QUARTER_TURN_X)
    matrix = matrix_from_pose((0, 0, 0), scaled)
    r = matrix[:3, :3]
    assert np.allclose(r @ r.T, np.eye(3), atol=1e-6)


@pytest.mark.parametrize(
    "position,quat",
    [
        ((0, 0), (0, 0, 0, 1)),
        ((0, 0, 0), (0, 0, 1)),
        ((0, 0, math.nan), (0, 0, 0, 1)),
        ((0, 0, 0), (0, 0, 0, 2.0)),
        ((0, 0, 0), (0, 0, 0, 0)),
    ],
)
def test_bad_poses_are_refused(position, quat):
    with pytest.raises(PoseError):
        matrix_from_pose(position, quat)
