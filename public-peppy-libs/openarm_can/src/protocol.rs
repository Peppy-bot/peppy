//! Damiao DM motor wire protocol: pure frame encode/decode, no I/O.
//!
//! Byte layouts and scaling follow the Damiao firmware conventions (reference:
//! enactic/openarm_can 1.2.8). MIT command and state frames are big-endian
//! nibble-packed with truncating quantization over `2^bits - 1` steps; POS_FORCE
//! commands and parameter writes are little-endian.

use crate::{CanError, Result};
use control_core::motor_health::{MotorCondition, Ratings};

/// Fixed CAN id for parameter reads/writes; the target motor's send id is
/// embedded little-endian in the first two payload bytes.
const PARAM_CAN_ID: u32 = 0x7FF;

/// POS_FORCE commands go to the motor's send id plus this offset.
pub(crate) const POS_FORCE_ID_OFFSET: u32 = 0x300;

/// Register id of the control-mode parameter (Damiao `RID::CTRL_MODE`).
const RID_CTRL_MODE: u8 = 0x0A;

const CMD_ENABLE: u8 = 0xFC;
const CMD_DISABLE: u8 = 0xFD;
/// How far under a motor's configured trip its critical threshold sits. A
/// threshold at the trip is not an early warning: the motor cuts out on the
/// same sample that crosses it, and the operator sees a fault rather than
/// the warning that was supposed to precede it.
const TRIP_MARGIN: f64 = 0.9;

/// How far a reported full scale may sit from the decode table before it
/// counts as a different scale: wide enough for the f32 the register travels
/// as, far narrower than the gap between any two models' scales.
const SCALE_TOLERANCE: f64 = 1e-3;

/// Damiao motor model, used to select the scaling limits for MIT command and
/// state frames. The model itself never goes on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MotorType {
    DM3507,
    DM4310,
    DM4310_48V,
    DM4340,
    DM4340_48V,
    DM6006,
    DM8006,
    DM8009,
    DM10010L,
    DM10010,
    DMH3510,
    DMH6215,
    DMG6220,
}

/// Per-model full-scale ranges: position +-`p_max` rad, velocity +-`v_max`
/// rad/s, torque +-`t_max` Nm.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Limits {
    pub p_max: f64,
    pub v_max: f64,
    pub t_max: f64,
}

impl MotorType {
    pub(crate) const fn limits(self) -> Limits {
        let (p_max, v_max, t_max) = match self {
            Self::DM3507 => (12.5, 50.0, 5.0),
            Self::DM4310 => (12.5, 30.0, 10.0),
            Self::DM4310_48V => (12.5, 50.0, 10.0),
            Self::DM4340 => (12.5, 8.0, 28.0),
            Self::DM4340_48V => (12.5, 10.0, 28.0),
            Self::DM6006 => (12.5, 45.0, 20.0),
            Self::DM8006 => (12.5, 45.0, 40.0),
            Self::DM8009 => (12.5, 45.0, 54.0),
            Self::DM10010L => (12.5, 25.0, 200.0),
            Self::DM10010 => (12.5, 20.0, 200.0),
            Self::DMH3510 => (12.5, 280.0, 1.0),
            Self::DMH6215 => (12.5, 45.0, 10.0),
            Self::DMG6220 => (12.5, 45.0, 10.0),
        };
        Limits {
            p_max,
            v_max,
            t_max,
        }
    }

    /// The full scale this driver decodes `param`'s axis with, for the three
    /// scale registers; `None` for a param that is not a scale.
    pub fn decode_full_scale(self, param: MotorParam) -> Option<f64> {
        let limits = self.limits();
        match param {
            MotorParam::PositionMax => Some(limits.p_max),
            MotorParam::VelocityMax => Some(limits.v_max),
            MotorParam::TorqueMax => Some(limits.t_max),
            _ => None,
        }
    }

    /// Whether a motor's self-reported full scale for `param`'s axis is the
    /// one this driver decodes with; `None` for a param that is not a scale.
    ///
    /// A mismatch is not a configuration preference to honour: the motor
    /// quantizes each field against its own full scale, so decoding with a
    /// different one scales every reading on that axis, silently and by a
    /// constant factor.
    pub fn scale_matches(self, param: MotorParam, reported: f64) -> Option<bool> {
        let expected = self.decode_full_scale(param)?;
        Some((reported - expected).abs() < SCALE_TOLERANCE)
    }

    /// Datasheet ratings for the models OpenArm deploys (DM-J4310/J4340/J8009
    /// manuals + docs.openarm.dev motor specifications); `None` where no
    /// verified datasheet numbers are recorded.
    /// A variant pair differs in its velocity scale, not its torque
    /// hardware: both members carry the same `t_max` in the firmware table
    /// above and the same figures on one datasheet, so they share ratings.
    pub fn ratings(self) -> Option<Ratings> {
        let (rated_nm, peak_nm) = match self {
            Self::DM4310 | Self::DM4310_48V => (3.0, 7.0),
            Self::DM4340 | Self::DM4340_48V => (9.0, 27.0),
            Self::DM8009 => (20.0, 40.0),
            _ => return None,
        };
        Some(
            Ratings::new(rated_nm, peak_nm)
                .expect("datasheet ratings are positive with peak above rated"),
        )
    }
}

/// The thresholds one motor is judged against, and what decided them: the
/// datasheet they started from and the trip point the motor's own registers
/// implied, so a caller can report which of the two won.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EffectiveRatings {
    pub ratings: Ratings,
    /// What the datasheet alone would have said, for `is_tightened`.
    datasheet: Ratings,
    /// The trip the motor's registers imply; `None` when it reported none
    /// this side of usable, in which case `ratings` is the datasheet.
    pub trip_nm: Option<f64>,
    /// The motor's trip, derated by [`TRIP_MARGIN`], lands at or below its
    /// continuous rating, so judging against it would warn during legal
    /// rated operation. The thresholds are left at the datasheet and the
    /// fact is surfaced, because a motor cutting out that close to its
    /// rating is a misconfiguration the operator should hear about rather
    /// than have rounded away.
    pub trip_too_low: bool,
}

impl EffectiveRatings {
    /// The thresholds to judge this motor against, decided from the registers
    /// it reported: its own configured trip where it reported usable ones,
    /// else the datasheet unchanged.
    ///
    /// A motor that answered nothing keeps the datasheet, the looser of the
    /// two, so a failed read never tightens a limit on evidence that was not
    /// gathered.
    ///
    /// A motor acts on what it is configured with, not on its datasheet, so a
    /// threshold at or above the configured trip cannot warn before the motor
    /// cuts out. The trip is derated by [`TRIP_MARGIN`] and only ever lowers
    /// `peak_nm`, so a misread trip can warn earlier but never later.
    pub fn from_registers(
        datasheet: Ratings,
        over_current: Option<f64>,
        torque_max: Option<f64>,
    ) -> Self {
        let trip_nm = over_current
            .zip(torque_max)
            .and_then(|(oc, tmax)| configured_trip_nm(oc, tmax));
        let tightened = trip_nm.map(|trip| datasheet.tightened_to(trip * TRIP_MARGIN));
        Self {
            ratings: match tightened {
                Some(Ok(ratings)) => ratings,
                _ => datasheet,
            },
            datasheet,
            trip_nm,
            trip_too_low: matches!(tightened, Some(Err(_))),
        }
    }

    /// Whether the motor's own configuration pulled the thresholds below the
    /// datasheet, i.e. it cuts out before the datasheet says it peaks.
    pub fn is_tightened(&self) -> bool {
        self.ratings.peak_nm() < self.datasheet.peak_nm()
    }
}

/// The torque at which a motor's over-current protection trips, from its
/// per-unit over-current setting and its full-scale torque. `None` for
/// readings outside the per-unit range, which mean the setting was not
/// understood rather than that the motor has no limit.
///
/// The per-unit reading is the motor's current limit as a fraction of its
/// maximum, and torque tracks current through the motor's constant, so the
/// same fraction of full-scale torque is where it trips.
fn configured_trip_nm(over_current_fraction: f64, torque_max_nm: f64) -> Option<f64> {
    let in_range = (0.0..=1.0).contains(&over_current_fraction) && over_current_fraction > 0.0;
    if !in_range || !torque_max_nm.is_finite() || torque_max_nm <= 0.0 {
        return None;
    }
    Some(over_current_fraction * torque_max_nm)
}

/// Motor status from a state frame's byte 0 high nibble. Any fault exits
/// Enable Mode on the motor itself: the joint goes limp until the fault is
/// cleared.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum MotorStatus {
    /// No state frame decoded yet (the cache's initial value).
    #[default]
    Unreported,
    Disabled,
    Enabled,
    Fault(FaultKind),
    /// A nibble value the protocol leaves undefined (2..=7, 0xF).
    Unknown(u8),
}

/// Fault kinds from the DM-J4310 manual's feedback-frame table: the complete
/// set the firmware defines (nibbles 8..=0xE). The enactic C++ reference
/// decodes no faults at all (it reads bytes 1..8 and discards byte 0), so
/// the manual, not the reference, is the authority here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FaultKind {
    Overvoltage,
    Undervoltage,
    Overcurrent,
    MosOvertemp,
    CoilOvertemp,
    CommLoss,
    Overload,
}

impl MotorStatus {
    fn from_nibble(nibble: u8) -> Self {
        match nibble {
            0x0 => Self::Disabled,
            0x1 => Self::Enabled,
            0x8 => Self::Fault(FaultKind::Overvoltage),
            0x9 => Self::Fault(FaultKind::Undervoltage),
            0xA => Self::Fault(FaultKind::Overcurrent),
            0xB => Self::Fault(FaultKind::MosOvertemp),
            0xC => Self::Fault(FaultKind::CoilOvertemp),
            0xD => Self::Fault(FaultKind::CommLoss),
            0xE => Self::Fault(FaultKind::Overload),
            n => Self::Unknown(n),
        }
    }

    /// The reported fault, if this status is one.
    pub fn fault(self) -> Option<FaultKind> {
        match self {
            Self::Fault(kind) => Some(kind),
            _ => None,
        }
    }

    /// What this status means to a health filter. `Unreported` has no
    /// mapping: it means no frame has been decoded, which is a statement
    /// about the cache rather than about the motor, so the caller reports
    /// silence instead of asking for a verdict on nothing.
    pub fn condition(self) -> Option<MotorCondition> {
        match self {
            Self::Unreported => None,
            Self::Disabled => Some(MotorCondition::Idle),
            Self::Enabled => Some(MotorCondition::Driving),
            Self::Fault(kind) => Some(MotorCondition::Faulted(kind.name())),
            Self::Unknown(_) => Some(MotorCondition::Unrecognised),
        }
    }
}

impl FaultKind {
    /// The operator-facing name of this protection, carried through health
    /// reports and alerts verbatim.
    pub fn name(self) -> &'static str {
        match self {
            Self::Overvoltage => "overvoltage",
            Self::Undervoltage => "undervoltage",
            Self::Overcurrent => "overcurrent",
            Self::MosOvertemp => "driver overtemperature",
            Self::CoilOvertemp => "winding overtemperature",
            Self::CommLoss => "communication loss",
            Self::Overload => "overload",
        }
    }
}

/// Damiao control mode; the values are the on-wire ids written to `CTRL_MODE`.
/// `pub` only to satisfy the sealed `Mode` trait's associated const; the
/// private `protocol` module keeps it out of the crate API.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControlMode {
    Mit = 1,
    PosForce = 4,
}

/// Torque-current limit as a per-unit value in `0..=1` (actual current divided
/// by the motor's max current). Constructing it is the range check.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct TorquePu(f64);

impl TorquePu {
    pub fn new(value: f64) -> Result<Self> {
        if !(0.0..=1.0).contains(&value) {
            return Err(CanError::TorqueOutOfRange(value));
        }
        Ok(Self(value))
    }
}

/// One decoded motor state frame.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub(crate) struct MotorState {
    pub position: f64,
    pub velocity: f64,
    pub torque: f64,
    pub status: MotorStatus,
    pub temp_mos_c: f64,
    pub temp_rotor_c: f64,
}

/// A command frame ready to write: all Damiao commands carry exactly 8 bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct OutFrame {
    pub id: u32,
    pub data: [u8; 8],
}

pub(crate) fn enable_frame(send_id: u32) -> OutFrame {
    command_frame(send_id, CMD_ENABLE)
}

pub(crate) fn disable_frame(send_id: u32) -> OutFrame {
    command_frame(send_id, CMD_DISABLE)
}

fn command_frame(send_id: u32, cmd: u8) -> OutFrame {
    OutFrame {
        id: send_id,
        data: [0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, cmd],
    }
}

/// Rejects NaN and infinities: quantization would silently turn them into a
/// full-scale command.
fn finite(value: f64) -> Result<f64> {
    if !value.is_finite() {
        return Err(CanError::NonFiniteCommand(value));
    }
    Ok(value)
}

/// MIT-mode command: PD to `q`/`dq` with gains `kp`/`kd` plus feedforward `tau`.
/// Values must be finite and are clamped to the motor's full-scale ranges
/// (kp `0..=500`, kd `0..=5`).
pub(crate) fn mit_frame(
    motor_type: MotorType,
    send_id: u32,
    kp: f64,
    kd: f64,
    q: f64,
    dq: f64,
    tau: f64,
) -> Result<OutFrame> {
    let lim = motor_type.limits();
    let kp_u = quantize(finite(kp)?, 0.0, 500.0, 12);
    let kd_u = quantize(finite(kd)?, 0.0, 5.0, 12);
    let q_u = quantize(finite(q)?, -lim.p_max, lim.p_max, 16);
    let dq_u = quantize(finite(dq)?, -lim.v_max, lim.v_max, 12);
    let tau_u = quantize(finite(tau)?, -lim.t_max, lim.t_max, 12);
    Ok(OutFrame {
        id: send_id,
        data: [
            (q_u >> 8) as u8,
            (q_u & 0xFF) as u8,
            (dq_u >> 4) as u8,
            (((dq_u & 0xF) << 4) | ((kp_u >> 8) & 0xF)) as u8,
            (kp_u & 0xFF) as u8,
            (kd_u >> 4) as u8,
            (((kd_u & 0xF) << 4) | ((tau_u >> 8) & 0xF)) as u8,
            (tau_u & 0xFF) as u8,
        ],
    })
}

/// POS_FORCE command: drive to `q_rad` under a speed limit (`0..=100` rad/s,
/// clamped) and the torque-current limit. Position and speed must be finite,
/// including after the position's cast to `f32` (this field is the one that
/// goes on the wire un-quantized, so an overflowing cast would encode
/// infinity).
pub(crate) fn pos_force_frame(
    send_id: u32,
    q_rad: f64,
    speed_rad_s: f64,
    torque: TorquePu,
) -> Result<OutFrame> {
    let pos_f32 = finite(q_rad)? as f32;
    if !pos_f32.is_finite() {
        return Err(CanError::NonFiniteCommand(q_rad));
    }
    let pos = pos_f32.to_le_bytes();
    let speed_u = (finite(speed_rad_s)?.clamp(0.0, 100.0) * 100.0) as u16;
    let torque_u = (torque.0 * 10000.0) as u16;
    let [speed_lo, speed_hi] = speed_u.to_le_bytes();
    let [torque_lo, torque_hi] = torque_u.to_le_bytes();
    Ok(OutFrame {
        id: send_id + POS_FORCE_ID_OFFSET,
        data: [
            pos[0], pos[1], pos[2], pos[3], speed_lo, speed_hi, torque_lo, torque_hi,
        ],
    })
}

/// State refresh request: asks the motor to emit a state frame without
/// commanding it. The reference implementation truncates the send id to one
/// byte here; for the sub-0x100 ids this addressing supports the bytes are
/// identical.
pub(crate) fn refresh_frame(send_id: u32) -> OutFrame {
    OutFrame {
        id: PARAM_CAN_ID,
        data: [
            (send_id & 0xFF) as u8,
            ((send_id >> 8) & 0xFF) as u8,
            0xCC,
            0,
            0,
            0,
            0,
            0,
        ],
    }
}

/// Registers the driver reads back from a motor. The values are the Damiao
/// register ids; the motor holds its own configured limits and constants
/// there, so a caller reads what this motor will actually do rather than
/// assuming a datasheet default.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MotorParam {
    /// Configured over-temperature trip, degrees C.
    OverTempLimit = 2,
    /// Configured over-current trip, as a fraction of the motor's maximum.
    OverCurrentLimit = 3,
    /// The id this motor replies to the host on.
    MasterId = 7,
    /// The id this motor is addressed by. A replacement motor ships with a
    /// factory default here, so it is the first thing to check after a swap.
    EscId = 8,
    /// Command timeout before the motor drops out of Enable Mode.
    Timeout = 9,
    /// Active control mode.
    ControlMode = 10,
    /// Gear reduction.
    GearRatio = 20,
    /// Full-scale position, the span the quantized position field is
    /// measured against.
    PositionMax = 21,
    /// Full-scale velocity, the span the quantized velocity field is
    /// measured against.
    VelocityMax = 22,
    /// Full-scale torque, the span every quantized torque field is measured
    /// against. Not a torque rating.
    TorqueMax = 23,
}

impl MotorParam {
    pub(crate) fn rid(self) -> u8 {
        self as u8
    }
}

/// Parameter query: asks one motor for one register. Read-only, so it is
/// safe on a disabled or faulted motor.
pub(crate) fn query_param_frame(send_id: u32, rid: u8) -> OutFrame {
    OutFrame {
        id: PARAM_CAN_ID,
        data: [
            (send_id & 0xFF) as u8,
            ((send_id >> 8) & 0xFF) as u8,
            0x33,
            rid,
            0,
            0,
            0,
            0,
        ],
    }
}

/// Decode a parameter reply addressed to `send_id`, as `(rid, value)`.
/// Registers in the Damiao integer ranges carry a little-endian u32; every
/// other register carries a little-endian f32.
pub(crate) fn parse_param_reply(send_id: u32, data: &[u8]) -> Option<(u8, f64)> {
    if data.len() < 8 || !is_param_frame(send_id, data) {
        return None;
    }
    let rid = data[3];
    let raw = [data[4], data[5], data[6], data[7]];
    let value = if rid_is_integer(rid) {
        f64::from(u32::from_le_bytes(raw))
    } else {
        f64::from(f32::from_le_bytes(raw))
    };
    Some((rid, value))
}

/// A parameter reply/echo for `send_id`: its id little-endian then the
/// query (0x33) or write (0x55) opcode. A state frame matching all three
/// bytes would put the motor at a physically unreachable full-scale
/// position, so this cannot reject a live state.
pub(crate) fn is_param_frame(send_id: u32, data: &[u8]) -> bool {
    data.len() >= 3
        && data[0] == (send_id & 0xFF) as u8
        && data[1] == ((send_id >> 8) & 0xFF) as u8
        && matches!(data[2], 0x33 | 0x55)
}

/// The registers Damiao encodes as u32 rather than f32 (reference:
/// enactic 1.2.8 `CanPacketDecoder::is_in_ranges`, the same three ranges).
/// Covers the id/mode block (MST_ID, ESC_ID, TIMEOUT, CTRL_MODE), the
/// version/mode block 13..=16, and 35..=36.
fn rid_is_integer(rid: u8) -> bool {
    matches!(rid, 7..=10 | 13..=16 | 35..=36)
}

/// Parameter write setting the motor's control mode.
pub(crate) fn ctrl_mode_frame(send_id: u32, mode: ControlMode) -> OutFrame {
    OutFrame {
        id: PARAM_CAN_ID,
        data: [
            (send_id & 0xFF) as u8,
            ((send_id >> 8) & 0xFF) as u8,
            0x55,
            RID_CTRL_MODE,
            mode as u8,
            0,
            0,
            0,
        ],
    }
}

/// Decode a motor state frame: byte 0's high nibble is the status/fault code
/// (the low nibble repeats the controller id, which `dispatch` already keys
/// on), bytes 1-5 the quantized position/velocity/torque, bytes 6-7 the MOS
/// and rotor temperatures in raw degrees C. Returns `None` for payloads
/// shorter than 8 bytes.
pub(crate) fn parse_state(motor_type: MotorType, data: &[u8]) -> Option<MotorState> {
    if data.len() < 8 {
        return None;
    }
    let lim = motor_type.limits();
    let q_u = (u16::from(data[1]) << 8) | u16::from(data[2]);
    let dq_u = (u16::from(data[3]) << 4) | (u16::from(data[4]) >> 4);
    let tau_u = (u16::from(data[4] & 0xF) << 8) | u16::from(data[5]);
    Some(MotorState {
        position: dequantize(q_u, -lim.p_max, lim.p_max, 16),
        velocity: dequantize(dq_u, -lim.v_max, lim.v_max, 12),
        torque: dequantize(tau_u, -lim.t_max, lim.t_max, 12),
        status: MotorStatus::from_nibble(data[0] >> 4),
        temp_mos_c: f64::from(data[6]),
        temp_rotor_c: f64::from(data[7]),
    })
}

/// Clamp `x` to `[min, max]`, then map linearly onto `0..=2^bits - 1`,
/// truncating toward zero (firmware convention; not rounded).
fn quantize(x: f64, min: f64, max: f64, bits: u32) -> u16 {
    let norm = (x.clamp(min, max) - min) / (max - min);
    (norm * f64::from((1u32 << bits) - 1)) as u16
}

fn dequantize(x: u16, min: f64, max: f64, bits: u32) -> f64 {
    f64::from(x) / f64::from((1u32 << bits) - 1) * (max - min) + min
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quantize_truncates_instead_of_rounding() {
        // DM4310 position span: (1.0 + 12.5) / 25.0 * 65535 = 35388.9; rounding
        // would give 35389.
        assert_eq!(quantize(1.0, -12.5, 12.5, 16), 35388);
    }

    #[test]
    fn quantize_clamps_to_full_scale() {
        assert_eq!(quantize(100.0, -12.5, 12.5, 16), 65535);
        assert_eq!(quantize(-100.0, -12.5, 12.5, 16), 0);
        assert_eq!(quantize(500.0, 0.0, 500.0, 12), 4095);
        assert_eq!(quantize(5.0, 0.0, 5.0, 12), 4095);
    }

    #[test]
    fn quantize_zero_lands_below_midpoint() {
        // trunc(0.5 * 65535) and trunc(0.5 * 4095): the firmware's zero point.
        assert_eq!(quantize(0.0, -12.5, 12.5, 16), 32767);
        assert_eq!(quantize(0.0, -45.0, 45.0, 12), 2047);
    }

    #[test]
    fn mit_frame_all_zero_command_dm8009() {
        // q=0x7FFF, dq=0x7FF, kp=0, kd=0, tau=0x7FF nibble-packed by hand.
        let f = mit_frame(MotorType::DM8009, 0x01, 0.0, 0.0, 0.0, 0.0, 0.0).unwrap();
        assert_eq!(f.id, 0x01);
        assert_eq!(f.data, [0x7F, 0xFF, 0x7F, 0xF0, 0x00, 0x00, 0x07, 0xFF]);
    }

    #[test]
    fn mit_frame_full_scale_command() {
        // Every field clamped high: all quantizers saturate.
        let f = mit_frame(MotorType::DM4310, 0x05, 501.0, 5.1, 13.0, 31.0, 11.0).unwrap();
        assert_eq!(f.data, [0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF]);
    }

    #[test]
    fn mit_frame_nibble_boundaries() {
        // DM4310: q=1.0 -> 35388 = 0x8A3C; kp=250.0 -> trunc(0.5*4095) = 2047 =
        // 0x7FF; kd/dq/tau at zero-point. Exercises every nibble splice.
        let f = mit_frame(MotorType::DM4310, 0x03, 250.0, 0.0, 1.0, 0.0, 0.0).unwrap();
        assert_eq!(f.data[0], 0x8A);
        assert_eq!(f.data[1], 0x3C);
        assert_eq!(f.data[2], 0x7F); // dq 0x7FF high byte
        assert_eq!(f.data[3], 0xF7); // dq low nibble | kp high nibble
        assert_eq!(f.data[4], 0xFF); // kp low byte
        assert_eq!(f.data[5], 0x00); // kd 0 high byte
        assert_eq!(f.data[6], 0x07); // kd low nibble | tau high nibble
        assert_eq!(f.data[7], 0xFF); // tau low byte
    }

    #[test]
    fn enable_and_disable_magic_frames() {
        assert_eq!(
            enable_frame(0x07).data,
            [0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFC]
        );
        assert_eq!(enable_frame(0x07).id, 0x07);
        assert_eq!(
            disable_frame(0x08).data,
            [0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFD]
        );
    }

    #[test]
    fn refresh_frame_layout() {
        let f = refresh_frame(0x07);
        assert_eq!(f.id, 0x7FF);
        assert_eq!(f.data, [0x07, 0x00, 0xCC, 0x00, 0x00, 0x00, 0x00, 0x00]);
    }

    #[test]
    fn ctrl_mode_frame_layout() {
        let f = ctrl_mode_frame(0x08, ControlMode::PosForce);
        assert_eq!(f.id, 0x7FF);
        assert_eq!(f.data, [0x08, 0x00, 0x55, 0x0A, 0x04, 0x00, 0x00, 0x00]);
        assert_eq!(ctrl_mode_frame(0x08, ControlMode::Mit).data[4], 0x01);
    }

    #[test]
    fn pos_force_frame_layout() {
        let f = pos_force_frame(0x08, 1.5, 5.0, TorquePu::new(0.5).unwrap()).unwrap();
        assert_eq!(f.id, 0x308);
        assert_eq!(f.data[..4], (1.5f32).to_le_bytes());
        assert_eq!(f.data[4..6], 500u16.to_le_bytes()); // 5.0 rad/s * 100
        assert_eq!(f.data[6..8], 5000u16.to_le_bytes()); // 0.5 pu * 10000
    }

    #[test]
    fn pos_force_frame_clamps_speed() {
        let f = pos_force_frame(0x08, 0.0, 150.0, TorquePu::new(1.0).unwrap()).unwrap();
        assert_eq!(f.data[4..6], 10000u16.to_le_bytes());
        assert_eq!(f.data[6..8], 10000u16.to_le_bytes());
        let f = pos_force_frame(0x08, 0.0, -3.0, TorquePu::new(0.0).unwrap()).unwrap();
        assert_eq!(f.data[4..6], 0u16.to_le_bytes());
        assert_eq!(f.data[6..8], 0u16.to_le_bytes());
    }

    #[test]
    fn mit_frame_rejects_non_finite_values() {
        let ty = MotorType::DM4310;
        assert!(mit_frame(ty, 0x01, f64::NAN, 0.0, 0.0, 0.0, 0.0).is_err());
        assert!(mit_frame(ty, 0x01, 0.0, f64::INFINITY, 0.0, 0.0, 0.0).is_err());
        assert!(mit_frame(ty, 0x01, 0.0, 0.0, f64::NEG_INFINITY, 0.0, 0.0).is_err());
        assert!(mit_frame(ty, 0x01, 0.0, 0.0, 0.0, f64::NAN, 0.0).is_err());
        assert!(mit_frame(ty, 0x01, 0.0, 0.0, 0.0, 0.0, f64::NAN).is_err());
    }

    #[test]
    fn pos_force_frame_rejects_non_finite_values() {
        let torque = TorquePu::new(0.5).unwrap();
        assert!(pos_force_frame(0x08, f64::NAN, 5.0, torque).is_err());
        assert!(pos_force_frame(0x08, f64::INFINITY, 5.0, torque).is_err());
        assert!(pos_force_frame(0x08, 0.0, f64::NAN, torque).is_err());
    }

    #[test]
    fn pos_force_frame_rejects_f32_overflow() {
        // Finite as f64 but infinity once cast to the wire's f32.
        let torque = TorquePu::new(0.5).unwrap();
        assert!(pos_force_frame(0x08, 1e39, 5.0, torque).is_err());
        assert!(pos_force_frame(0x08, -1e39, 5.0, torque).is_err());
    }

    #[test]
    fn torque_pu_rejects_out_of_range() {
        assert!(TorquePu::new(-0.01).is_err());
        assert!(TorquePu::new(1.01).is_err());
        assert!(TorquePu::new(f64::NAN).is_err());
        assert!(TorquePu::new(0.0).is_ok());
        assert!(TorquePu::new(1.0).is_ok());
    }

    #[test]
    fn parse_state_decodes_known_frame() {
        // DM4310, status nibble 0xA (overcurrent), q_u=0x8A3C, dq_u=0x7FF,
        // tau_u=0x7FF, T_MOS=0x30, T_Rotor=0x28.
        let data = [0xAA, 0x8A, 0x3C, 0x7F, 0xF7, 0xFF, 0x30, 0x28];
        let s = parse_state(MotorType::DM4310, &data).unwrap();
        assert!((s.position - dequantize(0x8A3C, -12.5, 12.5, 16)).abs() < 1e-12);
        assert!((s.velocity - dequantize(0x7FF, -30.0, 30.0, 12)).abs() < 1e-12);
        assert!((s.torque - dequantize(0x7FF, -10.0, 10.0, 12)).abs() < 1e-12);
        assert_eq!(s.status, MotorStatus::Fault(FaultKind::Overcurrent));
        assert_eq!(s.temp_mos_c, 48.0);
        assert_eq!(s.temp_rotor_c, 40.0);
    }

    #[test]
    fn parse_state_decodes_every_status_nibble() {
        let expect = [
            (0x0, MotorStatus::Disabled),
            (0x1, MotorStatus::Enabled),
            (0x8, MotorStatus::Fault(FaultKind::Overvoltage)),
            (0x9, MotorStatus::Fault(FaultKind::Undervoltage)),
            (0xA, MotorStatus::Fault(FaultKind::Overcurrent)),
            (0xB, MotorStatus::Fault(FaultKind::MosOvertemp)),
            (0xC, MotorStatus::Fault(FaultKind::CoilOvertemp)),
            (0xD, MotorStatus::Fault(FaultKind::CommLoss)),
            (0xE, MotorStatus::Fault(FaultKind::Overload)),
            (0x2, MotorStatus::Unknown(0x2)),
            (0x7, MotorStatus::Unknown(0x7)),
            (0xF, MotorStatus::Unknown(0xF)),
        ];
        for (nibble, status) in expect {
            let data = [nibble << 4, 0, 0, 0, 0, 0, 0, 0];
            assert_eq!(
                parse_state(MotorType::DM4310, &data).unwrap().status,
                status
            );
        }
        assert_eq!(MotorStatus::default(), MotorStatus::Unreported);
    }

    #[test]
    fn only_fault_statuses_report_a_fault() {
        assert_eq!(
            MotorStatus::Fault(FaultKind::Overload).fault(),
            Some(FaultKind::Overload)
        );
        for status in [
            MotorStatus::Unreported,
            MotorStatus::Disabled,
            MotorStatus::Enabled,
            MotorStatus::Unknown(0x3),
        ] {
            assert_eq!(status.fault(), None, "{status:?}");
        }
    }

    #[test]
    fn openarm_motor_ratings_sit_below_the_wire_full_scale() {
        // Every model OpenArm deploys has datasheet ratings, and the wire
        // full-scale t_max exceeds even peak torque: quantization headroom,
        // never an operating limit.
        for ty in [MotorType::DM8009, MotorType::DM4340, MotorType::DM4310] {
            let r = ty.ratings().expect("OpenArm model must have ratings");
            assert!(r.rated_nm() > 0.0, "{ty:?}");
            assert!(r.rated_nm() < r.peak_nm(), "{ty:?}");
            assert!(r.peak_nm() <= ty.limits().t_max, "{ty:?}");
        }
    }

    #[test]
    fn ratings_match_datasheets() {
        let expect = [
            (MotorType::DM8009, 20.0, 40.0),
            (MotorType::DM4340, 9.0, 27.0),
            (MotorType::DM4310, 3.0, 7.0),
        ];
        for (ty, rated, peak) in expect {
            let r = ty.ratings().unwrap();
            assert_eq!(r.rated_nm(), rated, "{ty:?}");
            assert_eq!(r.peak_nm(), peak, "{ty:?}");
        }
        assert_eq!(MotorType::DM10010.ratings(), None);
    }

    #[test]
    fn parse_state_rejects_short_frames() {
        assert_eq!(parse_state(MotorType::DM4310, &[0; 7]), None);
        assert_eq!(parse_state(MotorType::DM4310, &[]), None);
    }

    #[test]
    fn command_state_round_trip_within_one_quantum() {
        // Encode a command, feed the same quantized values back as a state
        // frame, and require the decode to land within one quantization step.
        let ty = MotorType::DM8009;
        let lim = ty.limits();
        let q_step = 2.0 * lim.p_max / 65535.0;
        let dq_step = 2.0 * lim.v_max / 4095.0;
        let tau_step = 2.0 * lim.t_max / 4095.0;
        for q in [-12.5, -3.7, -0.001, 0.0, 0.42, 7.9, 12.5] {
            for dq in [-45.0, -1.3, 0.0, 2.2, 45.0] {
                for tau in [-54.0, -8.05, 0.0, 0.5, 54.0] {
                    let f = mit_frame(ty, 0x01, 0.0, 0.0, q, dq, tau).unwrap();
                    // Rebuild the state layout from the command layout.
                    let q_u = (u16::from(f.data[0]) << 8) | u16::from(f.data[1]);
                    let dq_u = (u16::from(f.data[2]) << 4) | (u16::from(f.data[3]) >> 4);
                    let state = [
                        0x00,
                        (q_u >> 8) as u8,
                        (q_u & 0xFF) as u8,
                        (dq_u >> 4) as u8,
                        (((dq_u & 0xF) << 4) as u8) | (f.data[6] & 0x0F),
                        f.data[7],
                        0x00,
                        0x00,
                    ];
                    let s = parse_state(ty, &state).unwrap();
                    assert!((s.position - q).abs() <= q_step, "q={q} got {}", s.position);
                    assert!(
                        (s.velocity - dq).abs() <= dq_step,
                        "dq={dq} got {}",
                        s.velocity
                    );
                    assert!(
                        (s.torque - tau).abs() <= tau_step,
                        "tau={tau} got {}",
                        s.torque
                    );
                }
            }
        }
    }

    #[test]
    fn limits_match_firmware_table() {
        // Transcribed from Damiao MOTOR_LIMIT_PARAMS (enactic 1.2.8).
        let expect = [
            (MotorType::DM3507, 50.0, 5.0),
            (MotorType::DM4310, 30.0, 10.0),
            (MotorType::DM4310_48V, 50.0, 10.0),
            (MotorType::DM4340, 8.0, 28.0),
            (MotorType::DM4340_48V, 10.0, 28.0),
            (MotorType::DM6006, 45.0, 20.0),
            (MotorType::DM8006, 45.0, 40.0),
            (MotorType::DM8009, 45.0, 54.0),
            (MotorType::DM10010L, 25.0, 200.0),
            (MotorType::DM10010, 20.0, 200.0),
            (MotorType::DMH3510, 280.0, 1.0),
            (MotorType::DMH6215, 45.0, 10.0),
            (MotorType::DMG6220, 45.0, 10.0),
        ];
        for (ty, v_max, t_max) in expect {
            let lim = ty.limits();
            assert_eq!(lim.p_max, 12.5, "{ty:?}");
            assert_eq!(lim.v_max, v_max, "{ty:?}");
            assert_eq!(lim.t_max, t_max, "{ty:?}");
        }
    }
}

#[cfg(test)]
mod trip_tests {
    use super::*;

    #[test]
    fn a_configured_trip_below_the_datasheet_peak_tightens_it() {
        // Read from hardware: j3 and j4 are configured to trip at
        // 0.8 of a 28 Nm full scale, which is under their 27 Nm peak, so a
        // peak-based threshold would never fire before the motor cut out.
        let datasheet = MotorType::DM4340.ratings().unwrap();
        let effective = EffectiveRatings::from_registers(datasheet, Some(0.8), Some(28.0));
        assert!((effective.trip_nm.unwrap() - 22.4).abs() < 1e-9);
        // 22.4 Nm trip derated by TRIP_MARGIN. Asserted as the literal the
        // arm is actually judged against, so changing the margin has to be a
        // deliberate edit here rather than something the test absorbs.
        assert!((effective.ratings.peak_nm() - 20.16).abs() < 1e-9);
        assert!(
            effective.ratings.peak_nm() < effective.trip_nm.unwrap(),
            "a threshold at the trip leaves no lead time before the cutout"
        );
        assert_eq!(
            effective.ratings.rated_nm(),
            9.0,
            "the continuous rating is unchanged"
        );
        assert!(effective.is_tightened());
        assert!(!effective.trip_too_low);
    }

    #[test]
    fn the_datasheet_peak_caps_a_generous_trip() {
        // DM4310 as measured: 0.8 of a 10 Nm full scale trips at 8.0, whose
        // margin (7.2) still sits above the 7.0 datasheet peak, so the
        // datasheet stays in charge and nothing is raised.
        let datasheet = MotorType::DM4310.ratings().unwrap();
        let effective = EffectiveRatings::from_registers(datasheet, Some(0.8), Some(10.0));
        assert_eq!(
            effective.ratings.peak_nm(),
            7.0,
            "never raise a limit above datasheet"
        );
        assert!(!effective.is_tightened());
    }

    #[test]
    fn a_threshold_always_leaves_room_before_the_cutout() {
        // Every motor on this arm: the threshold must sit strictly under the
        // trip, or the motor cuts out on the same sample that crosses it.
        for (ty, tmax) in [
            (MotorType::DM8009, 54.0),
            (MotorType::DM4340, 28.0),
            (MotorType::DM4310, 10.0),
        ] {
            let effective =
                EffectiveRatings::from_registers(ty.ratings().unwrap(), Some(0.8), Some(tmax));
            let trip = effective.trip_nm.unwrap();
            assert!(
                effective.ratings.peak_nm() < trip,
                "{ty:?} threshold reaches the cutout"
            );
            assert!(
                effective.ratings.peak_nm() > effective.ratings.rated_nm(),
                "{ty:?} band collapsed"
            );
        }
    }

    #[test]
    fn a_trip_whose_derated_threshold_reaches_the_rating_is_reported_not_applied() {
        // Sustained operation at the rating is normal, so a threshold there
        // would warn during legal rated operation; the datasheet is kept and
        // the fact is surfaced.
        let datasheet = MotorType::DM4310.ratings().unwrap();
        // 0.4 of a 10 Nm scale trips at 4.0, derated to 3.6... above rated.
        let usable = EffectiveRatings::from_registers(datasheet, Some(0.4), Some(10.0));
        assert!((usable.ratings.peak_nm() - 3.6).abs() < 1e-9);
        assert!(!usable.trip_too_low);

        // 0.3 of a 10 Nm scale trips at 3.0, derated to 2.7: under the 3.0
        // continuous rating.
        let unusable = EffectiveRatings::from_registers(datasheet, Some(0.3), Some(10.0));
        assert_eq!(unusable.ratings, datasheet, "thresholds must not collapse");
        assert!(unusable.trip_too_low);
        assert!((unusable.trip_nm.unwrap() - 3.0).abs() < 1e-9);

        // The band above the rating whose derated threshold still reaches
        // it: a 3.2 Nm trip derates to 2.88, under the 3.0 rating, so it is
        // reported too, and any message must not claim the trip itself sits
        // below the rating.
        let banded = EffectiveRatings::from_registers(datasheet, Some(0.32), Some(10.0));
        assert_eq!(banded.ratings, datasheet);
        assert!(banded.trip_too_low);
        assert!(banded.trip_nm.unwrap() > datasheet.rated_nm());
    }

    #[test]
    fn an_out_of_range_over_current_reading_yields_no_trip() {
        assert_eq!(configured_trip_nm(0.0, 28.0), None);
        assert_eq!(configured_trip_nm(1.5, 28.0), None);
        assert_eq!(configured_trip_nm(f64::NAN, 28.0), None);
        assert_eq!(configured_trip_nm(0.8, 0.0), None);
    }

    #[test]
    fn registers_that_tighten_are_reported_as_having_done_so() {
        let datasheet = MotorType::DM4340.ratings().unwrap();
        let effective = EffectiveRatings::from_registers(datasheet, Some(0.8), Some(28.0));
        assert!(effective.is_tightened());
        assert_eq!(effective.datasheet, datasheet);
        let trip = effective.trip_nm.unwrap();
        assert!((trip - 22.4).abs() < 1e-9);
        assert!((effective.ratings.peak_nm() - trip * TRIP_MARGIN).abs() < 1e-9);
    }

    #[test]
    fn registers_that_do_not_tighten_still_report_their_trip() {
        // The operator is told where the motor cuts out even when the
        // datasheet is the stricter of the two.
        let datasheet = MotorType::DM4310.ratings().unwrap();
        let effective = EffectiveRatings::from_registers(datasheet, Some(0.8), Some(10.0));
        assert!(!effective.is_tightened());
        assert_eq!(effective.ratings, datasheet);
        assert!((effective.trip_nm.unwrap() - 8.0).abs() < 1e-9);
    }

    #[test]
    fn a_motor_that_reported_nothing_keeps_its_datasheet() {
        // A silent or unreadable register must never move a threshold: the
        // read failing is not evidence about the motor.
        let datasheet = MotorType::DM8009.ratings().unwrap();
        for registers in [
            (None, None),
            (Some(0.8), None),
            (None, Some(54.0)),
            (Some(1.5), Some(54.0)), // out of range: not understood
        ] {
            let effective = EffectiveRatings::from_registers(datasheet, registers.0, registers.1);
            assert_eq!(effective.ratings, datasheet, "{registers:?}");
            assert_eq!(effective.trip_nm, None, "{registers:?}");
            assert!(!effective.is_tightened(), "{registers:?}");
        }
    }
}

#[cfg(test)]
mod param_tests {
    use super::*;

    #[test]
    fn a_query_frame_addresses_the_motor_little_endian_with_the_read_opcode() {
        let f = query_param_frame(0x0102, 23);
        assert_eq!(f.id, 0x7FF);
        assert_eq!(f.data, [0x02, 0x01, 0x33, 23, 0, 0, 0, 0]);
    }

    #[test]
    fn an_integer_register_decodes_as_a_u32() {
        // ESC_ID = 8 carrying the value 7, as a motor reports its own id.
        let data = [0x01, 0x00, 0x33, 8, 7, 0, 0, 0];
        assert_eq!(parse_param_reply(0x01, &data), Some((8, 7.0)));
    }

    #[test]
    fn a_float_register_decodes_as_an_f32() {
        // TMAX = 23 carrying 28.0: the DM4340 full-scale torque as the
        // hardware reports it.
        let mut data = [0x01, 0x00, 0x33, 23, 0, 0, 0, 0];
        data[4..8].copy_from_slice(&28.0f32.to_le_bytes());
        assert_eq!(parse_param_reply(0x01, &data), Some((23, 28.0)));
    }

    #[test]
    fn misreading_an_integer_register_as_a_float_is_what_this_split_prevents() {
        // The raw bytes of the integer 8 reinterpreted as an f32 are a
        // denormal, so getting the split wrong prints 1.1e-44 for an id.
        let data = [0x01, 0x00, 0x33, 8, 8, 0, 0, 0];
        let (_, as_int) = parse_param_reply(0x01, &data).unwrap();
        assert_eq!(as_int, 8.0);
        let as_float = f64::from(f32::from_le_bytes([8, 0, 0, 0]));
        assert!(as_float < 1e-40, "the misread would be {as_float}");
    }

    #[test]
    fn a_write_echo_decodes_like_a_query_reply() {
        let data = [0x01, 0x00, 0x55, 10, 1, 0, 0, 0];
        assert_eq!(parse_param_reply(0x01, &data), Some((10, 1.0)));
    }

    #[test]
    fn replies_for_another_motor_or_malformed_payloads_are_refused() {
        let for_other = [0x02, 0x00, 0x33, 8, 7, 0, 0, 0];
        assert_eq!(parse_param_reply(0x01, &for_other), None);
        let short = [0x01, 0x00, 0x33, 8];
        assert_eq!(parse_param_reply(0x01, &short), None);
        let wrong_opcode = [0x01, 0x00, 0x77, 8, 7, 0, 0, 0];
        assert_eq!(parse_param_reply(0x01, &wrong_opcode), None);
    }

    #[test]
    fn every_param_this_driver_queries_has_the_type_its_decode_assumes() {
        // The split is enactic 1.2.8 is_in_ranges: ids and modes are u32,
        // physical quantities are f32. A param moved across the split would
        // decode ids as denormals or torques as garbage integers.
        let integer = [
            MotorParam::MasterId,
            MotorParam::EscId,
            MotorParam::Timeout,
            MotorParam::ControlMode,
        ];
        let float = [
            MotorParam::OverTempLimit,
            MotorParam::OverCurrentLimit,
            MotorParam::GearRatio,
            MotorParam::PositionMax,
            MotorParam::VelocityMax,
            MotorParam::TorqueMax,
        ];
        for p in integer {
            assert!(rid_is_integer(p.rid()), "{p:?}");
        }
        for p in float {
            assert!(!rid_is_integer(p.rid()), "{p:?}");
        }
    }
}

#[cfg(test)]
mod scale_tests {
    use super::*;

    #[test]
    fn the_arm_lineup_reports_the_torque_scale_this_driver_decodes_with() {
        // Read from both arms' TorqueMax registers: the firmware table this
        // driver decodes against is the motors' own, not a transcription
        // that happens to agree.
        for (ty, reported) in [
            (MotorType::DM8009, 54.0),
            (MotorType::DM4340, 28.0),
            (MotorType::DM4310, 10.0),
        ] {
            assert_eq!(
                ty.scale_matches(MotorParam::TorqueMax, reported),
                Some(true),
                "{ty:?}"
            );
            assert_eq!(
                ty.decode_full_scale(MotorParam::TorqueMax),
                Some(reported),
                "{ty:?}"
            );
        }
    }

    #[test]
    fn the_arm_lineup_decodes_at_the_scales_the_hardware_reports() {
        // Read off both arms with probe_arm: every motor's PositionMax,
        // VelocityMax and TorqueMax registers against the table this driver
        // decodes with. j3 and j4 report 10 rad/s, not the 8 of the other
        // DM4340 variant, and decoding them at 8 silently under-read their
        // measured velocity by 20% while over-commanding setpoints by 25%.
        let measured: [(f64, f64, f64); crate::ARM_DOF] = [
            (12.5, 45.0, 54.0),
            (12.5, 45.0, 54.0),
            (12.5, 10.0, 28.0),
            (12.5, 10.0, 28.0),
            (12.5, 30.0, 10.0),
            (12.5, 30.0, 10.0),
            (12.5, 30.0, 10.0),
        ];
        for (joint, (p_max, v_max, t_max)) in measured.iter().enumerate() {
            let ty = crate::ARM_MOTOR_TYPES[joint];
            for (param, reported) in [
                (MotorParam::PositionMax, *p_max),
                (MotorParam::VelocityMax, *v_max),
                (MotorParam::TorqueMax, *t_max),
            ] {
                assert_eq!(
                    ty.scale_matches(param, reported),
                    Some(true),
                    "j{} {param:?}",
                    joint + 1
                );
            }
        }
    }

    #[test]
    fn every_scale_register_maps_to_its_own_decode_axis() {
        // The decode table for the DM4310: position +-12.5 rad, velocity
        // +-30 rad/s, torque +-10 Nm. Crossing the axes would corrupt every
        // reading on both.
        let ty = MotorType::DM4310;
        assert_eq!(ty.decode_full_scale(MotorParam::PositionMax), Some(12.5));
        assert_eq!(ty.decode_full_scale(MotorParam::VelocityMax), Some(30.0));
        assert_eq!(ty.decode_full_scale(MotorParam::TorqueMax), Some(10.0));
        assert_eq!(ty.scale_matches(MotorParam::VelocityMax, 30.0), Some(true));
        assert_eq!(ty.scale_matches(MotorParam::VelocityMax, 8.0), Some(false));
    }

    #[test]
    fn a_non_scale_register_has_no_scale_to_match() {
        assert_eq!(
            MotorType::DM4310.decode_full_scale(MotorParam::GearRatio),
            None
        );
        assert_eq!(
            MotorType::DM4310.scale_matches(MotorParam::EscId, 8.0),
            None
        );
    }

    #[test]
    fn another_models_scale_does_not_pass_as_this_ones() {
        // The failure this guards is a replacement motor of the wrong model
        // answering on the right id, which decodes silently and wrongly.
        assert_eq!(
            MotorType::DM4310.scale_matches(MotorParam::TorqueMax, 28.0),
            Some(false)
        );
        assert_eq!(
            MotorType::DM4340.scale_matches(MotorParam::TorqueMax, 54.0),
            Some(false)
        );
        assert_eq!(
            MotorType::DM8009.scale_matches(MotorParam::TorqueMax, 10.0),
            Some(false)
        );
    }

    #[test]
    fn wire_float_noise_still_matches() {
        assert_eq!(
            MotorType::DM4340.scale_matches(MotorParam::TorqueMax, 28.000_01),
            Some(true)
        );
    }

    #[test]
    fn an_unreadable_scale_is_not_silently_treated_as_a_match() {
        assert_eq!(
            MotorType::DM4310.scale_matches(MotorParam::TorqueMax, f64::NAN),
            Some(false)
        );
        assert_eq!(
            MotorType::DM4310.scale_matches(MotorParam::TorqueMax, 0.0),
            Some(false)
        );
    }
}

#[cfg(test)]
mod variant_tests {
    use super::*;

    #[test]
    fn a_velocity_variant_keeps_its_siblings_torque_ratings() {
        // The arm node refuses to start a motor with no ratings, so a
        // variant missing from this table takes the arm down at bring-up.
        for (a, b) in [
            (MotorType::DM4310, MotorType::DM4310_48V),
            (MotorType::DM4340, MotorType::DM4340_48V),
        ] {
            assert_eq!(a.ratings(), b.ratings(), "{a:?} vs {b:?}");
            assert_eq!(
                a.limits().t_max,
                b.limits().t_max,
                "same torque scale is why they share ratings"
            );
            assert_ne!(a.limits().v_max, b.limits().v_max, "{a:?} vs {b:?}");
        }
    }

    #[test]
    fn every_motor_this_arm_uses_has_ratings() {
        for (joint, ty) in crate::ARM_MOTOR_TYPES.iter().enumerate() {
            assert!(ty.ratings().is_some(), "j{} ({ty:?})", joint + 1);
        }
    }
}

/// Differential fixture replay, without a CAN interface.
///
/// The frames are built by the same encoder the driver sends, so the whole
/// arm sweep is checked on every `cargo test` run. The bus-level replay in
/// `tests/vcan.rs` needs a vcan interface and skips without one; this does
/// not, so a scale or layout change cannot land against a green suite.
#[cfg(test)]
mod fixture_tests {
    use super::*;
    use crate::{ARM_DOF, ARM_MOTOR_TYPES, ARM_SEND_IDS};

    // The gripper halves of the shared sweep belong to the bus-level replay
    // in tests/vcan.rs; this consumer only encodes the arm.
    #[allow(dead_code)]
    mod sweep {
        include!("../tests/common/sweep.rs");
    }

    fn line(frame: &OutFrame) -> String {
        sweep::format_frame(true, 1, frame.id, &frame.data)
    }

    fn arm_sweep_lines() -> Vec<String> {
        let joint = |j: usize| (ARM_MOTOR_TYPES[j], ARM_SEND_IDS[j]);
        let mut out: Vec<String> = ARM_SEND_IDS
            .iter()
            .map(|&id| line(&enable_frame(id)))
            .collect();
        out.extend(ARM_SEND_IDS.iter().map(|&id| line(&refresh_frame(id))));
        for &(kp, kd, q, dq, tau) in sweep::ARM_MIT_SWEEP {
            out.extend((0..ARM_DOF).map(|j| {
                let (ty, id) = joint(j);
                line(&mit_frame(ty, id, kp, kd, q, dq, tau).expect("sweep value is finite"))
            }));
        }
        let [kp, kd, q, dq, tau] = sweep::ARM_MIT_INDEXED;
        out.extend((0..ARM_DOF).map(|j| {
            let (ty, id) = joint(j);
            line(&mit_frame(ty, id, kp[j], kd[j], q[j], dq[j], tau[j]).expect("finite"))
        }));
        out.extend(ARM_SEND_IDS.iter().map(|&id| line(&disable_frame(id))));
        out
    }

    #[test]
    fn the_arm_sweep_encodes_to_the_recorded_fixture() {
        let fixture = include_str!("../tests/fixtures/arm_mit.txt");
        let expected: Vec<&str> = fixture.lines().filter(|l| !l.trim().is_empty()).collect();
        let actual = arm_sweep_lines();
        assert_eq!(
            actual.len(),
            expected.len(),
            "frame count changed: the sweep or the command sequence moved"
        );
        for (i, (a, e)) in actual.iter().zip(&expected).enumerate() {
            assert_eq!(a, e, "fixture line {}", i + 1);
        }
    }

    #[test]
    fn j3_and_j4_encode_velocity_at_the_scale_the_hardware_reports() {
        // The j3/j4 scale is the one place this driver deliberately differs
        // from the enactic capture, so it gets its own assertion rather than
        // living only inside the whole-sweep diff.
        let dq = 0.1;
        let at_ten = mit_frame(MotorType::DM4340_48V, 3, 50.0, 1.0, 0.5, dq, 0.2).expect("finite");
        let at_eight = mit_frame(MotorType::DM4340, 3, 50.0, 1.0, 0.5, dq, 0.2).expect("finite");
        assert_ne!(at_ten.data, at_eight.data);
        assert_eq!(line(&at_ten), "FD:1 003 851E81319933380E");
    }
}
