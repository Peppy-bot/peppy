use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize, Serialize)]
pub struct JointStatesMsg {
    pub robot: String,
    pub step: u64,
    #[serde(default)]
    pub joint_names: Vec<String>,
    pub positions: Vec<f64>,
    pub velocities: Vec<f64>,
    pub stamp: f64,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct ImuMsg {
    pub robot: String,
    pub step: u64,
    pub orientation: [f64; 4],
    pub angular_velocity: [f64; 3],
    pub linear_acceleration: [f64; 3],
    pub stamp: f64,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct TfFrame {
    pub name: String,
    pub parent: String,
    pub position: [f64; 3],
    pub orientation: [f64; 4],
}

#[derive(Debug, Deserialize, Serialize)]
pub struct TfTreeMsg {
    pub robot: String,
    pub step: u64,
    pub frames: Vec<TfFrame>,
    pub stamp: f64,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct ClockMsg {
    pub step: u64,
    pub sim_time: f64,
    pub stamp: f64,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct EePoseMsg {
    pub robot: String,
    pub step: u64,
    pub position: [f64; 3],
    pub orientation: [f64; 4],
    pub stamp: f64,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct OdometryMsg {
    pub robot: String,
    pub step: u64,
    pub position: [f64; 3],
    pub orientation: [f64; 4],
    pub linear_velocity: [f64; 3],
    pub angular_velocity: [f64; 3],
    pub stamp: f64,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct WrenchMsg {
    pub robot: String,
    pub step: u64,
    pub force: [f64; 3],
    pub torque: [f64; 3],
    pub stamp: f64,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct ContactForce {
    pub body1: String,
    pub body2: String,
    pub position: [f64; 3],
    pub force: [f64; 3],
}

#[derive(Debug, Deserialize, Serialize)]
pub struct ContactForcesMsg {
    pub robot: String,
    pub step: u64,
    pub contacts: Vec<ContactForce>,
    pub stamp: f64,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct GripperStateMsg {
    pub robot: String,
    pub step: u64,
    pub joint_names: Vec<String>,
    pub positions: Vec<f64>,
    // Optional on the wire — some publishers omit applied_forces (e.g. when the
    // engine doesn't expose actuator force readback). Defaults to empty Vec so
    // consumers don't fail to deserialize against partial payloads.
    #[serde(default)]
    pub applied_forces: Vec<f64>,
    pub stamp: f64,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct JointCommandMsg {
    pub positions: Vec<f64>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct MoveArmFeedbackMsg {
    pub joint_positions: Vec<f64>,
    pub current_ee_position: [f64; 3],
    pub action_time: f64,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct MoveArmResultMsg {
    pub success: bool,
    pub message: String,
    pub final_joint_positions: Vec<f64>,
    pub final_ee_position: [f64; 3],
    pub action_time: f64,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct MoveGripperFeedbackMsg {
    pub joint_positions: Vec<f64>,
    pub action_time: f64,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct MoveGripperResultMsg {
    pub success: bool,
    pub message: String,
    pub final_joint_positions: Vec<f64>,
    pub action_time: f64,
}
