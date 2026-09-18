"""The robot model this description vouches for: the kinematics URDF the
postures and limits were verified against, and the TCP frame every wire pose
names."""

from __future__ import annotations

from pathlib import Path

# A fixed frame on the fixed-jaw body near the jaw region; on this
# single-moving-jaw gripper the pad midpoint shifts with the opening, so no
# static frame is exactly the contracts' grasp point.
END_EFFECTOR_FRAME = "gripper_frame_link"

KINEMATICS_URDF_PATH = str(Path(__file__).parent / "urdf" / "so101_kinematics.urdf")
