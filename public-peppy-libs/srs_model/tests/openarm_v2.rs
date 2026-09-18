//! Verify srs_model builds a valid 7-DOF SRS chain from the vendored OpenArm v2.0 URDF,
//! whose revolute pinch-gripper fingers branch off the wrist tip (`ee_base_link`). This
//! guards the load-bearing assumption that the chain walk stops at the 7th revolute joint
//! and does not trip on the finger branch, and that the reoriented v2 frames still satisfy
//! the SRS concurrency checks so gravity/Coriolis feedforward evaluates.

use srs_model::{ARM_DOF, Arm};

const V2_URDF: &str = "../openarm_description/assets/openarm_v20.urdf";
const V1_URDF: &str = "../openarm_description/assets/openarm_v10.urdf";

#[test]
fn builds_v2_srs_chain_for_both_arms() {
    for base in ["openarm_left_base_link", "openarm_right_base_link"] {
        let arm = Arm::from_urdf_file(V2_URDF, base).unwrap_or_else(|e| panic!("v2 {base}: {e}"));

        let limits = arm.limits();
        assert_eq!(limits.len(), ARM_DOF);
        assert_eq!(
            limits[3].lo, 0.0,
            "{base}: elbow (j4) mechanical lower is 0.0"
        );

        // The feedforward path the real arm runs each control tick: gravity + Coriolis
        // from the posed chain must evaluate to finite torques for the v2 model.
        let posed = arm.at(&[0.0; ARM_DOF]);
        let gravity = posed.gravity_torques();
        let coriolis = posed.coriolis_torques(&[0.1; ARM_DOF]);
        assert!(
            gravity.iter().chain(coriolis.iter()).all(|t| t.is_finite()),
            "{base}: gravity/Coriolis torques must be finite"
        );
    }
}

#[test]
fn v1_still_builds() {
    let arm = Arm::from_urdf_file(V1_URDF, "openarm_left_link0").expect("v1 builds");
    assert_eq!(arm.limits().len(), ARM_DOF);
}
