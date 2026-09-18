//! Shared fixture loading for the integration tests. Builds an [`Arm`] from the
//! bundled fixture URDF and the `base` link, the same robot-agnostic entry point
//! (`from_urdf`) production uses. `side` is `"left"`/`"right"`; left vs right is
//! just a different chain in the same URDF, selected by the base link.

use srs_model::Arm;

const FIXTURE_URDF: &str = include_str!("../fixtures/openarm_v10.urdf");

fn base(side: &str) -> String {
    format!("openarm_{side}_link0")
}

pub fn arm(side: &str) -> Arm {
    Arm::from_urdf(FIXTURE_URDF, &base(side)).expect("load fixture arm")
}
