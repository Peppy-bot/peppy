"""FK/IK over lerobot's placo-based RobotKinematics, radians at this boundary.

lerobot's solver speaks degrees and 4x4 matrices; everything above this module
speaks joint_link radians and wire poses. IK is verified by FK before it is
trusted: placo returns its best effort even far from the target, and an
unreached pose must fail the caller instead of moving the arm somewhere else.

What that verification costs, measured on poses reachable by construction (a
random in-limits joint vector, its FK taken as the target): 14% of targets are
refused when seeded from the ready posture and 39% when seeded from an
arbitrary one, concentrated near the base (25% within 0.23 m, 7% beyond
0.45 m). The cause is structural rather than a tuning miss. lerobot's
inverse_kinematics is a single linearised step, so iterating it is a local
descent from the seed and reaches only what the seed's branch reaches; a
refusal means this branch did not arrive, not that the pose is unreachable.
Callers that need the wider workspace re-seed and ask again.
"""

from __future__ import annotations

import math

import numpy as np

from so101_description.model import END_EFFECTOR_FRAME
from so101_description.transforms import (
    matrix_from_pose,
    pose_from_matrix,
    relative_rotation_rad,
)
from so101_description.units import JOINT_NAMES

# Positional acceptance for a verified point-to-point solution.
IK_POSITION_TOLERANCE_M = 0.01
# lerobot's inverse_kinematics runs one QP step, sized for streaming small
# deltas; a point-to-point solve iterates it until FK verifies the position.
IK_MAX_ITERATIONS = 100
# The streamed path is best effort: a few steps bound the per-tick cost, and
# an out-of-reach pose tracks the workspace boundary instead of freezing.
IK_STREAM_ITERATIONS = 3

# Position and orientation are both soft objectives of one QP, minimising
# `position_weight * |dp|^2 + orientation_weight * |dtheta|^2`. The terms are
# metres against radians, so the weight ratio is not the trade: at lerobot's
# default 0.01 a 20 degree miss outweighs a 7 mm miss by more than twenty to
# one, and the solver spends position buying orientation. Five joints
# underactuate three rotational degrees of freedom, so on this arm that
# purchase is frequently impossible and the position is spent for nothing.
#
# Weighted this low the orientation objective still resolves the wrist onto
# the pose it was given, reaching a reachable orientation within a third of a
# degree, while an unreachable one stops taking the position with it. Zero is
# not the answer; it abandons reachable orientations entirely.
ORIENTATION_WEIGHT = 1e-4


def _bar(value: float | None, default: float) -> float:
    """A caller's acceptance bar, or the default when unstated. A bar that is
    not a usable positive distance is a caller error, not a reason to fall
    back to the default."""
    if value is None:
        return default
    if not math.isfinite(value) or value <= 0.0:
        raise ValueError(f"tolerance must be finite and positive, got {value}")
    return value

class Kinematics:
    def __init__(self, urdf_path: str):
        from lerobot.model.kinematics import RobotKinematics

        self._solver = RobotKinematics(
            urdf_path=urdf_path,
            target_frame_name=END_EFFECTOR_FRAME,
            joint_names=list(JOINT_NAMES),
        )

    def jacobian(self, positions_rad: tuple[float, ...]):
        """The end-effector Jacobian at these joint positions: 6 rows by one
        column per arm joint in wire order, linear velocity in rows 0..2 and
        angular in rows 3..5, both in world-aligned axes.

        `J @ dq` is the twist a joint step produces, and it is linear in that
        step, which is what a speed cap needs: scaling a step by s scales its
        end-effector speed by exactly s. Measuring the same thing by moving
        the arm and differencing forward kinematics does not have that
        property, because forward kinematics is not linear.

        Local-world-aligned, not world: Pinocchio's world Jacobian gives the
        twist of the body frame referred to the world origin, whose linear
        part is not the tool point's velocity. Against a finite difference the
        two disagree by tens of percent at every step size."""
        robot = self._solver.robot
        # The QP carries state between solves, and moving the model out from
        # under it diverges a streaming solve by radians. This is a query, so
        # it puts the configuration back before returning.
        restore = {name: robot.get_joint(name) for name in JOINT_NAMES}
        try:
            for name, value in zip(JOINT_NAMES, positions_rad, strict=True):
                robot.set_joint(name, value)
            robot.update_kinematics()
            columns = [robot.get_joint_v_offset(name) for name in JOINT_NAMES]
            return np.asarray(
                robot.frame_jacobian(END_EFFECTOR_FRAME, "local_world_aligned")
            )[:, columns]
        finally:
            for name, value in restore.items():
                robot.set_joint(name, value)
            robot.update_kinematics()

    def forward_kinematics(self, positions_rad: tuple[float, ...]):
        """(position m, quaternion xyzw) of the end effector."""
        matrix = self._solver.forward_kinematics(np.degrees(positions_rad))
        return pose_from_matrix(matrix)

    def inverse_kinematics(
        self,
        seed_rad: tuple[float, ...],
        position,
        orientation,
        *,
        position_tolerance_m: float | None = None,
        orientation_tolerance_rad: float | None = None,
    ) -> tuple[float, ...] | None:
        """Joint radians reaching the pose, or None when no verified solution
        meets the caller's bars.

        `position_tolerance_m` defaults to IK_POSITION_TOLERANCE_M.
        `orientation_tolerance_rad` defaults to no gate at all: five joints
        underactuate the three orientation degrees of freedom, so the solver's
        best orientation is taken rather than gated behind a bar almost no
        reachable pose could meet. A caller that does care states one and gets
        a refusal instead of a quiet approximation."""
        position_bar = _bar(position_tolerance_m, IK_POSITION_TOLERANCE_M)
        orientation_bar = _bar(orientation_tolerance_rad, math.inf)
        target_matrix = matrix_from_pose(position, orientation)
        target_position = tuple(position)
        target_orientation = tuple(orientation)
        joints_deg = np.degrees(seed_rad)
        for _ in range(IK_MAX_ITERATIONS):
            joints_deg = self._step(
                joints_deg, target_matrix, ORIENTATION_WEIGHT
            )
            solution_rad = self._as_radians(joints_deg)
            reached, reached_orientation = self.forward_kinematics(solution_rad)
            if math.dist(reached, target_position) > position_bar:
                continue
            if relative_rotation_rad(reached_orientation, target_orientation) <= orientation_bar:
                return solution_rad
        return None

    def inverse_kinematics_streaming(
        self, seed_rad: tuple[float, ...], position, orientation
    ) -> tuple[float, ...] | None:
        """Best-effort step toward the pose for the streamed path: a bounded
        few QP iterations, unverified, so an out-of-reach target tracks the
        workspace boundary instead of freezing the arm. None only for a
        solution the solver corrupted (non-finite)."""
        target_matrix = matrix_from_pose(position, orientation)
        joints_deg = np.degrees(seed_rad)
        for _ in range(IK_STREAM_ITERATIONS):
            joints_deg = self._step(
                joints_deg, target_matrix, ORIENTATION_WEIGHT
            )
        solution_rad = self._as_radians(joints_deg)
        if not all(math.isfinite(v) for v in solution_rad):
            return None
        return solution_rad

    def _step(self, joints_deg, target_matrix, orientation_weight: float):
        """One QP step. The orientation weight is stated at every call rather
        than inherited from lerobot's signature default, because which pose
        objective this arm should trade away is our decision, not theirs."""
        return self._solver.inverse_kinematics(
            joints_deg, target_matrix, orientation_weight=orientation_weight
        )

    @staticmethod
    def _as_radians(joints_deg) -> tuple[float, ...]:
        values = tuple(float(v) for v in np.radians(joints_deg))
        if len(values) != len(JOINT_NAMES):
            raise ValueError(
                f"solver returned {len(values)} joints, expected {len(JOINT_NAMES)}"
            )
        return values
