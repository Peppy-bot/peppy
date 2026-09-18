"""The named whole-arm postures, in joint_link radians.

Constants, matching openarm's description-owned postures: a posture is a fact
of the arm, not a deployment knob. Both are validated against the deployed
URDF's joint limits at startup.
"""

from __future__ import annotations

# The collapsed park pose the arm ships in and rests in, measured on
# hardware and pulled inside the model: the physical fold reaches -1.80 rad
# on the shoulder, past the URDF's -1.745 floor (the model under-states the
# real travel), so the target rests just inside it. Folded onto the base,
# stable, minimal holding torque, safe to release.
HOME_POSITIONS_RAD = (-0.05, -1.72, 1.68, 1.24, 0.32)

# The calibration middle: every joint at the center of its calibrated
# travel (the lerobot-calibrate homing pose), maximally far from every hard
# stop. Where work starts.
READY_POSITIONS_RAD = (0.0, 0.0, 0.0, 0.0, 0.0)
