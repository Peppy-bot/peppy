//! Pure-Rust driver for OpenArm's Damiao DM motors over Linux SocketCAN.
//!
//! [`ArmCan`] and [`GripperCan`] take `&mut self` for every bus operation;
//! wrap them in `Arc<Mutex<_>>` for cross-task sharing.
//!
//! The arm motor lineup and CAN addressing are identical across OpenArm v1.0 and v2.0
//! (same DM motors, same bus IDs), so they live once at the crate root ([`ARM_MOTOR_TYPES`],
//! [`ARM_SEND_IDS`], [`ARM_RECV_IDS`]). Only the gripper differs per generation: [`v10`]
//! is the prismatic parallel-jaw gripper (MIT position control); [`v20`] is the revolute
//! pinch gripper (POS_FORCE control with a commanded force limit).
//!
//! State readback is poll-based: a `recv_all` pass decodes pending state
//! frames into a cache that `get_state` snapshots.

mod bus;
mod protocol;

use std::marker::PhantomData;
use std::num::NonZeroU32;
use std::time::Duration;

use bus::{MotorBus, MotorSlot};
use protocol::TorquePu;
pub use protocol::{EffectiveRatings, FaultKind, MotorParam, MotorStatus, MotorType};

/// Enable attempts before bring-up refuses readiness, for
/// [`ArmCan::enable_and_confirm`] and its gripper counterpart. A one-shot
/// enable can silently fail to take (seen on hardware: a motor answering
/// every poll while ignoring commands); the retries cover an enable frame
/// the motor missed or dropped.
pub const ENABLE_ATTEMPTS: std::num::NonZeroU32 = std::num::NonZeroU32::new(3).expect("non-zero");

/// Degrees of freedom of the arm. Both generations are 7-DOF SRS.
pub const ARM_DOF: usize = 7;

/// A fixed-length array of one `f64` per arm joint.
pub type JointVec = [f64; ARM_DOF];

/// Per-joint arm motor models, j1..j7. Identical across v1.0 and v2.0.
pub const ARM_MOTOR_TYPES: [MotorType; ARM_DOF] = [
    MotorType::DM8009,
    MotorType::DM8009,
    // j3 and j4 are the 10 rad/s DM4340 variant: on both arms they report
    // a VelocityMax of 10, and the two variants differ only in that scale.
    // Decoding them at 8 under-read their measured velocity by 20% and
    // over-commanded their velocity setpoints by 25%.
    MotorType::DM4340_48V,
    MotorType::DM4340_48V,
    MotorType::DM4310,
    MotorType::DM4310,
    MotorType::DM4310,
];
/// Per-joint arm command (send) CAN ids. Identical across v1.0 and v2.0.
pub const ARM_SEND_IDS: [u32; ARM_DOF] = [0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07];
/// Per-joint arm state (recv) CAN ids. Identical across v1.0 and v2.0.
pub const ARM_RECV_IDS: [u32; ARM_DOF] = [0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17];

/// Gripper hardware constants for the OpenArm v1.0 platform: a prismatic parallel-jaw
/// gripper on a single DM4310, run in MIT position control.
pub mod v10 {
    use super::MotorType;

    pub const GRIPPER_MOTOR_TYPE: MotorType = MotorType::DM4310;
    pub const GRIPPER_SEND_ID: u32 = 0x08;
    pub const GRIPPER_RECV_ID: u32 = 0x18;

    // joint=0 m (closed) ↔ motor=0 rad, joint=GRIPPER_OPEN_M ↔ motor=GRIPPER_OPEN_RAD.
    // Values match ROS2 openarm/v10_simple_hardware.
    pub const GRIPPER_OPEN_M: f64 = 0.044;
    #[allow(clippy::approx_constant)]
    pub const GRIPPER_OPEN_RAD: f64 = -1.0472;
}

/// Gripper hardware constants for the OpenArm v2.0 platform: a revolute pinch gripper on a
/// single DM4310, run in POS_FORCE control (position command with a speed + force limit).
pub mod v20 {
    use super::MotorType;

    pub const GRIPPER_MOTOR_TYPE: MotorType = MotorType::DM4310;
    pub const GRIPPER_SEND_ID: u32 = 0x08;
    pub const GRIPPER_RECV_ID: u32 = 0x18;

    // The gripper motor closes at 0 rad and opens toward GRIPPER_OPEN_RAD. This is the
    // motor-frame open angle used by enactic's POS_FORCE reference (test_gripper_posforce
    // commands 0..π/2); each finger joint travels π/2 rad (the right hand's URDF range
    // mirrored to -π/2..0), so the motor↔finger ratio is 1:1.
    pub const GRIPPER_OPEN_RAD: f64 = std::f64::consts::FRAC_PI_2;
}

/// Receive window for the motor's reply to a control-mode write during
/// gripper bring-up, matching the enactic reference timing (demo.cpp waits
/// 2000us after enable and parameter round-trips).
const CTRL_MODE_ECHO_TIMEOUT_US: u32 = 2000;

/// State of the gripper motor from the most recent `recv_all`. Temperatures
/// are raw degrees C; the status is [`MotorStatus::Unreported`] until the
/// first state frame decodes. The same DM motor as every arm joint, with the
/// same fault behaviour: a fault drops it out of Enable Mode mid-grasp.
#[derive(Debug, Clone, Copy, Default)]
pub struct GripperState {
    pub position: f64,
    pub velocity: f64,
    pub torque: f64,
    pub status: MotorStatus,
    pub temp_mos_c: f64,
    pub temp_rotor_c: f64,
    /// Receive passes since this motor's last decoded state frame.
    pub passes_since_state: u32,
}

/// State of all arm joints from the most recent `recv_all`. Temperatures are
/// raw degrees C; each status is [`MotorStatus::Unreported`] until that
/// joint's first state frame decodes.
#[derive(Debug, Clone, Copy, Default)]
pub struct ArmState {
    pub positions: JointVec,
    pub velocities: JointVec,
    pub torques: JointVec,
    pub statuses: [MotorStatus; ARM_DOF],
    pub temps_mos_c: JointVec,
    pub temps_rotor_c: JointVec,
    /// Per joint, completed `recv_all` passes since that motor's last state
    /// frame; the other fields are cached from that frame, so a growing
    /// count means they are stale, not current.
    pub passes_since_state: [u32; ARM_DOF],
}

#[derive(Debug, thiserror::Error)]
pub enum CanError {
    #[error("failed to open CAN interface '{interface}'")]
    Open {
        interface: String,
        source: std::io::Error,
    },
    #[error("CAN I/O failed")]
    Io(#[from] std::io::Error),
    #[error("CAN id {0:#x} exceeds the 11-bit standard range")]
    InvalidCanId(u32),
    #[error("torque_pu must be per-unit in 0..=1, got {0}")]
    TorqueOutOfRange(f64),
    #[error("command value must be finite, got {0}")]
    NonFiniteCommand(f64),
    #[error("receive timeout must be positive; the reply pass waits on it")]
    ZeroReceiveTimeout,
}

/// Motors that did not acknowledge an enable, split by what the bus showed.
/// A refusal means the motor is talking but not taking commands; silence
/// means it is not talking at all (unpowered, unplugged, wrong bus).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Unconfirmed {
    pub refused: Vec<u32>,
    pub silent: Vec<u32>,
}

impl Unconfirmed {
    pub fn is_empty(&self) -> bool {
        self.refused.is_empty() && self.silent.is_empty()
    }
}

impl std::fmt::Display for Unconfirmed {
    /// Names both groups by send id, listing only the non-empty ones so the
    /// common single-fault case reads as one clause.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let ids = |group: &[u32]| {
            group
                .iter()
                .map(|id| format!("{id:#04x}"))
                .collect::<Vec<_>>()
                .join(", ")
        };
        match (self.refused.is_empty(), self.silent.is_empty()) {
            (true, true) => write!(f, "every motor confirmed enable"),
            (false, true) => write!(f, "motors [{}] refused enable", ids(&self.refused)),
            (true, false) => write!(f, "motors [{}] are silent", ids(&self.silent)),
            (false, false) => write!(
                f,
                "motors [{}] refused enable and [{}] are silent",
                ids(&self.refused),
                ids(&self.silent)
            ),
        }
    }
}

/// Why enabling failed: the bus itself, or motors that never acknowledged.
#[derive(Debug, thiserror::Error)]
pub enum EnableFailure {
    #[error(transparent)]
    Can(#[from] CanError),
    #[error("{0}")]
    Unconfirmed(Unconfirmed),
}

pub type Result<T> = std::result::Result<T, CanError>;

/// 7-DOF arm on one CAN interface. Open with [`ArmCan::open`], then
/// `enable_all` before commanding.
pub struct ArmCan(MotorBus);

impl ArmCan {
    /// Opens `can_interface` and registers the seven arm motors
    /// ([`ARM_MOTOR_TYPES`] on [`ARM_SEND_IDS`] / [`ARM_RECV_IDS`]).
    pub fn open(can_interface: &str, enable_fd: bool) -> Result<Self> {
        let slots = (0..ARM_DOF)
            .map(|i| MotorSlot::new(ARM_MOTOR_TYPES[i], ARM_SEND_IDS[i], ARM_RECV_IDS[i], 0))
            .collect::<Result<Vec<_>>>()?;
        Ok(Self(MotorBus::open(can_interface, enable_fd, slots)?))
    }

    pub fn enable_all(&mut self) -> Result<()> {
        self.0.enable_all()
    }

    pub fn disable_all(&mut self) -> Result<()> {
        self.0.disable_all()
    }

    /// Receives pending state frames into the cache: waits up to
    /// `first_timeout_us` for the first frame, then drains without waiting.
    pub fn recv_all(&mut self, first_timeout_us: u32) -> Result<()> {
        self.0.recv_all(first_timeout_us)
    }

    /// Receives and discards pending frames (bring-up replies that must not
    /// land in the state cache).
    pub fn drain(&mut self, first_timeout_us: u32) -> Result<()> {
        self.0.drain(first_timeout_us)
    }

    /// Enable every motor and confirm each acknowledges it, retrying the
    /// enable for stragglers and reporting the ones that never confirm.
    /// Blocking; see [`MotorBus::enable_and_confirm`].
    pub fn enable_and_confirm(
        &mut self,
        attempts: NonZeroU32,
        settle: Duration,
        recv_timeout_us: u32,
    ) -> std::result::Result<(), EnableFailure> {
        self.0.enable_and_confirm(attempts, settle, recv_timeout_us)
    }

    /// Requests a state frame from every motor without commanding it; follow
    /// with [`recv_all`](Self::recv_all) to decode the replies.
    pub fn refresh_all(&mut self) -> Result<()> {
        self.0.refresh_all()
    }

    /// Solicits a state frame from every joint and collects the replies, so
    /// [`get_state`](Self::get_state) reflects one fresh pass rather than
    /// whichever motors happened to answer first. Read-only.
    pub fn refresh_state(&mut self, recv_timeout_us: u32) -> Result<()> {
        self.0.refresh_state(recv_timeout_us)
    }

    /// Reads one register from every joint, j1..j7, `None` where a motor did
    /// not answer. Read-only: safe on a disabled, faulted, or immobile arm.
    pub fn read_param(
        &mut self,
        param: MotorParam,
        recv_timeout_us: u32,
    ) -> Result<[Option<f64>; ARM_DOF]> {
        let values = self.0.read_param(param, recv_timeout_us)?;
        Ok(values
            .try_into()
            .expect("open() registers exactly ARM_DOF slots"))
    }

    /// MIT-mode command to all joints: PD to `q`/`dq` plus feedforward `tau`.
    /// Refused whole or delivered to every joint: encoding happens before any
    /// send, and a send failure does not stop the remaining joints' frames.
    pub fn mit_control(
        &mut self,
        kp: &JointVec,
        kd: &JointVec,
        q: &JointVec,
        dq: &JointVec,
        tau: &JointVec,
    ) -> Result<()> {
        let frames = mit_frames(kp, kd, q, dq, tau)?;
        self.0.send_all(frames)
    }

    /// Snapshot of joint state from the most recent [`recv_all`](Self::recv_all).
    pub fn get_state(&self) -> ArmState {
        let mut state = ArmState::default();
        for (i, slot) in self.0.slots().iter().enumerate() {
            let motor = slot.state();
            state.positions[i] = motor.position;
            state.velocities[i] = motor.velocity;
            state.torques[i] = motor.torque;
            state.statuses[i] = motor.status;
            state.temps_mos_c[i] = motor.temp_mos_c;
            state.temps_rotor_c[i] = motor.temp_rotor_c;
            state.passes_since_state[i] = slot.passes_since_state();
        }
        state
    }
}

/// Encodes the seven per-joint MIT frames before anything touches the bus, so
/// one bad value (for example a NaN feedforward) refuses the whole command
/// instead of commanding the joints ahead of it and skipping the rest.
fn mit_frames(
    kp: &JointVec,
    kd: &JointVec,
    q: &JointVec,
    dq: &JointVec,
    tau: &JointVec,
) -> Result<Vec<protocol::OutFrame>> {
    (0..ARM_DOF)
        .map(|i| {
            protocol::mit_frame(
                ARM_MOTOR_TYPES[i],
                ARM_SEND_IDS[i],
                kp[i],
                kd[i],
                q[i],
                dq[i],
                tau[i],
            )
        })
        .collect()
}

/// Marker for the MIT control mode (v1.0 prismatic gripper).
pub enum Mit {}
/// Marker for the POS_FORCE control mode (v2.0 pinch gripper).
pub enum PosForce {}

mod sealed {
    use crate::protocol::ControlMode;

    pub trait Sealed {
        const CONTROL_MODE: ControlMode;
    }
    impl Sealed for super::Mit {
        const CONTROL_MODE: ControlMode = ControlMode::Mit;
    }
    impl Sealed for super::PosForce {
        const CONTROL_MODE: ControlMode = ControlMode::PosForce;
    }
}

/// Gripper control mode, fixed at open time: [`Mit`] or [`PosForce`].
pub trait Mode: sealed::Sealed {}
impl Mode for Mit {}
impl Mode for PosForce {}

/// 1-DOF gripper on one CAN interface. The control mode is part of the type:
/// open with [`GripperCan::open_mit`] (v1.0) or
/// [`GripperCan::open_pos_force`] (v2.0), then `enable_all` before
/// commanding. Opening writes the motor's control-mode parameter and consumes
/// the reply, so the bus is quiet when it returns.
pub struct GripperCan<M: Mode> {
    bus: MotorBus,
    _mode: PhantomData<M>,
}

impl GripperCan<Mit> {
    /// Opens the gripper motor in MIT mode; command it with
    /// [`mit_control`](Self::mit_control).
    pub fn open_mit(
        can_interface: &str,
        enable_fd: bool,
        motor_type: MotorType,
        send_id: u32,
        recv_id: u32,
    ) -> Result<Self> {
        Self::open(can_interface, enable_fd, motor_type, send_id, recv_id, 0)
    }

    /// MIT-mode command: PD to `q`/`dq` plus feedforward `tau`.
    pub fn mit_control(&mut self, kp: f64, kd: f64, q: f64, dq: f64, tau: f64) -> Result<()> {
        let slot = &self.bus.slots()[0];
        let frame = protocol::mit_frame(slot.motor_type(), slot.send_id(), kp, kd, q, dq, tau)?;
        self.bus.send(&frame)
    }
}

impl GripperCan<PosForce> {
    /// Opens the gripper motor in POS_FORCE mode; command it with
    /// [`set_position`](Self::set_position).
    pub fn open_pos_force(
        can_interface: &str,
        enable_fd: bool,
        motor_type: MotorType,
        send_id: u32,
        recv_id: u32,
    ) -> Result<Self> {
        Self::open(
            can_interface,
            enable_fd,
            motor_type,
            send_id,
            recv_id,
            protocol::POS_FORCE_ID_OFFSET,
        )
    }

    /// POS_FORCE-mode command: drive to motor angle `q_rad` with an absolute
    /// speed limit `speed_rad_s` (`0..=100` rad/s, clamped) and a
    /// torque-current limit `torque_pu` (per-unit, `0..=1`; rejected outside
    /// that range). The commanded force is the grip force cap; measured
    /// torque comes back via [`get_state`](Self::get_state).
    pub fn set_position(&mut self, q_rad: f64, speed_rad_s: f64, torque_pu: f64) -> Result<()> {
        let torque = TorquePu::new(torque_pu)?;
        let send_id = self.bus.slots()[0].send_id();
        let frame = protocol::pos_force_frame(send_id, q_rad, speed_rad_s, torque)?;
        self.bus.send(&frame)
    }
}

impl<M: Mode> GripperCan<M> {
    fn open(
        can_interface: &str,
        enable_fd: bool,
        motor_type: MotorType,
        send_id: u32,
        recv_id: u32,
        extra_send_offset: u32,
    ) -> Result<Self> {
        let slot = MotorSlot::new(motor_type, send_id, recv_id, extra_send_offset)?;
        let mut bus = MotorBus::open(can_interface, enable_fd, vec![slot])?;
        bus.set_control_mode(M::CONTROL_MODE)?;
        bus.drain(CTRL_MODE_ECHO_TIMEOUT_US)?;
        Ok(Self {
            bus,
            _mode: PhantomData,
        })
    }

    pub fn enable_all(&mut self) -> Result<()> {
        self.bus.enable_all()
    }

    /// Re-enable after an unexpected Disabled, rewriting the control mode
    /// first: opening wrote the mode into motor RAM, so a motor that lost
    /// power since then is back on its flash default, and enabling it there
    /// would have it interpret this session's commands under the wrong frame
    /// layout. The motor mirrors the mode write back on the param id, which
    /// the state decoder already rejects, so no drain is needed.
    pub fn reenable(&mut self) -> Result<()> {
        self.bus.set_control_mode(M::CONTROL_MODE)?;
        self.bus.enable_all()
    }

    pub fn disable_all(&mut self) -> Result<()> {
        self.bus.disable_all()
    }

    /// Receives pending state frames into the cache: waits up to
    /// `first_timeout_us` for the first frame, then drains without waiting.
    pub fn recv_all(&mut self, first_timeout_us: u32) -> Result<()> {
        self.bus.recv_all(first_timeout_us)
    }

    /// Receives and discards pending frames (bring-up replies that must not
    /// land in the state cache).
    pub fn drain(&mut self, first_timeout_us: u32) -> Result<()> {
        self.bus.drain(first_timeout_us)
    }

    /// Enable the motor and confirm it acknowledges, retrying and reporting
    /// it if it never does. Blocking; see [`MotorBus::enable_and_confirm`].
    pub fn enable_and_confirm(
        &mut self,
        attempts: NonZeroU32,
        settle: Duration,
        recv_timeout_us: u32,
    ) -> std::result::Result<(), EnableFailure> {
        self.bus
            .enable_and_confirm(attempts, settle, recv_timeout_us)
    }

    /// Requests a state frame from the motor without commanding it; follow
    /// with [`recv_all`](Self::recv_all) to decode the reply.
    pub fn refresh_all(&mut self) -> Result<()> {
        self.bus.refresh_all()
    }

    /// Snapshot of gripper state from the most recent
    /// [`recv_all`](Self::recv_all). The `torque` field is the measured grip
    /// force feedback.
    pub fn get_state(&self) -> GripperState {
        let slot = &self.bus.slots()[0];
        let motor = slot.state();
        GripperState {
            position: motor.position,
            velocity: motor.velocity,
            torque: motor.torque,
            status: motor.status,
            temp_mos_c: motor.temp_mos_c,
            temp_rotor_c: motor.temp_rotor_c,
            passes_since_state: slot.passes_since_state(),
        }
    }
}

#[cfg(test)]
#[allow(clippy::assertions_on_constants)]
mod tests {
    use super::*;

    #[test]
    fn arm_arrays_have_consistent_length() {
        assert_eq!(ARM_MOTOR_TYPES.len(), ARM_DOF);
        assert_eq!(ARM_SEND_IDS.len(), ARM_DOF);
        assert_eq!(ARM_RECV_IDS.len(), ARM_DOF);
    }

    /// The MIT command is all-or-nothing at encode time: a bad value in any
    /// joint refuses the whole command with no frame built, independent of
    /// which joint carries it (the old per-joint loop sent joints 1..k before
    /// discovering a bad value at k+1).
    #[test]
    fn one_bad_joint_refuses_the_whole_mit_command() {
        let good = [0.0; ARM_DOF];
        for bad_joint in [0, ARM_DOF - 1] {
            let mut tau = good;
            tau[bad_joint] = f64::NAN;
            let refused = mit_frames(&good, &good, &good, &good, &tau);
            assert!(
                matches!(refused, Err(CanError::NonFiniteCommand(_))),
                "NaN at joint {bad_joint} must refuse the command"
            );
        }
    }

    #[test]
    fn mit_frames_encodes_one_frame_per_joint_in_id_order() {
        let zero = [0.0; ARM_DOF];
        let frames = mit_frames(&zero, &zero, &zero, &zero, &zero).unwrap();
        assert_eq!(frames.len(), ARM_DOF);
        let ids: Vec<u32> = frames.iter().map(|f| f.id).collect();
        assert_eq!(ids, ARM_SEND_IDS);
    }

    #[test]
    fn v1_gripper_mapping_signs_oppose() {
        // Linear mapping: 0 m → 0 rad, GRIPPER_OPEN_M → GRIPPER_OPEN_RAD. The open
        // direction is negative in the motor frame; a sign flip sends the gripper
        // the wrong way.
        assert!(v10::GRIPPER_OPEN_M > 0.0);
        assert!(v10::GRIPPER_OPEN_RAD < 0.0);
    }

    #[test]
    fn v2_gripper_opens_positive() {
        // The v2 pinch gripper closes at 0 rad and opens toward a positive motor angle.
        assert!(v20::GRIPPER_OPEN_RAD > 0.0);
    }

    #[test]
    fn gripper_can_ids_do_not_collide_with_arm() {
        for (send, recv) in [
            (v10::GRIPPER_SEND_ID, v10::GRIPPER_RECV_ID),
            (v20::GRIPPER_SEND_ID, v20::GRIPPER_RECV_ID),
        ] {
            assert!(!ARM_SEND_IDS.contains(&send));
            assert!(!ARM_RECV_IDS.contains(&recv));
        }
    }
}

/// Log throttle for a periodic bus loop: one line per failure burst (the
/// first, then every [`Self::REPEAT_EVERY`]th) and one on recovery, instead of
/// several per tick at the loop rate. [`consecutive`](Self::consecutive) lets
/// the caller escalate when a burst has clearly stopped being transient.
#[derive(Debug, Default)]
pub struct CanErrorThrottle {
    consecutive: u64,
}

impl CanErrorThrottle {
    const REPEAT_EVERY: u64 = 100;

    pub fn new() -> Self {
        Self::default()
    }

    pub fn failure(&mut self, context: &str, e: &CanError) {
        if self.consecutive.is_multiple_of(Self::REPEAT_EVERY) {
            tracing::error!(
                "{context}: CAN tick failed ({} consecutive): {e}",
                self.consecutive + 1
            );
        }
        self.consecutive += 1;
    }

    pub fn success(&mut self, context: &str) {
        if self.consecutive > 0 {
            tracing::info!(
                "{context}: CAN recovered after {} failed ticks",
                self.consecutive
            );
        }
        self.consecutive = 0;
    }

    /// Failed ticks since the last success.
    pub fn consecutive(&self) -> u64 {
        self.consecutive
    }
}

#[cfg(test)]
mod throttle_tests {
    use super::*;

    #[test]
    fn throttle_counts_and_resets() {
        let mut t = CanErrorThrottle::new();
        let e = CanError::InvalidCanId(0x800);
        for _ in 0..5 {
            t.failure("test", &e);
        }
        assert_eq!(t.consecutive(), 5);
        t.success("test");
        assert_eq!(t.consecutive(), 0);
    }
}
