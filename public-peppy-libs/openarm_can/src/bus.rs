//! SocketCAN transport with a per-motor state cache.
//!
//! Mirrors the Damiao bus conventions: no kernel CAN filters (frames are
//! dispatched in software by recv id), CAN-FD frames carry the bit-rate-switch
//! flag, and a receive pass waits `first_timeout_us` for the first frame then
//! drains the rest without waiting.

use std::num::NonZeroU32;
use std::os::fd::AsFd;
use std::time::{Duration, Instant};

use nix::errno::Errno;
use nix::poll::{PollFd, PollFlags, ppoll};
use nix::sys::time::TimeSpec;
use socketcan::id::FdFlags;
use socketcan::{
    CanAnyFrame, CanFdFrame, CanFdSocket, CanFrame, CanSocket, EmbeddedFrame, Frame, Socket,
    StandardId,
};

use crate::protocol::{
    self, ControlMode, MotorParam, MotorState, MotorStatus, MotorType, OutFrame,
};
use crate::{CanError, EnableFailure, Result, Unconfirmed};

/// Highest valid 11-bit standard CAN id.
const CAN_SFF_MAX: u32 = 0x7FF;

/// SO_RCVTIMEO on the bus socket: the reference implementation's 100us
/// stuck-read guard, kept so a blocking read can never outlive one kernel
/// timer tick even if the readiness poll misfires.
const RECV_BACKSTOP: Duration = Duration::from_micros(100);

/// How long a solicited exchange keeps collecting replies before calling the
/// motors that have not answered silent. Two orders of magnitude above the
/// ~1 ms a full arm's request/reply exchange occupies at 1 Mbps: these run
/// by hand or once per bring-up attempt, and a false negative refuses to
/// start a healthy arm or reports a register as unreadable.
const REPLY_WINDOW: Duration = Duration::from_millis(100);

/// Ceiling on one control-tick receive pass. The per-tick path holds the bus
/// lock the shutdown path needs and its caller has its own budget (the loop
/// period), so its bound is a backstop against a flooded bus rather than a
/// patience window: generous against a 1-2 ms pass, far under the bring-up
/// [`REPLY_WINDOW`] whose written justification is exactly that it never
/// runs per tick.
const TICK_RECV_WINDOW: Duration = Duration::from_millis(5);

/// What a receive pass does with the frames it reads, and whether it counts
/// against every slot's silence. A confirmation runs several receives inside
/// one logical pass, so counting is separate from decoding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Decode {
    /// Decode and count the pass: the normal per-tick receive.
    AsOnePass,
    /// Decode without counting: a receive inside an already-counted pass.
    WithoutCounting,
    /// Read and throw away (bring-up replies, parameter echoes).
    Discard,
}

/// One motor on the bus: its addressing, the last decoded state, and how
/// many decode passes have completed since that state arrived (the cache
/// otherwise presents a silent motor's last reading as current forever).
#[derive(Debug, Clone, Copy)]
pub(crate) struct MotorSlot {
    motor_type: MotorType,
    send_id: u32,
    recv_id: u32,
    state: MotorState,
    passes_since_state: u32,
}

impl MotorSlot {
    /// Checks both ids fit the 11-bit standard range; `extra_send_offset` is
    /// added to the send id for modes that address an id above it (POS_FORCE).
    pub fn new(
        motor_type: MotorType,
        send_id: u32,
        recv_id: u32,
        extra_send_offset: u32,
    ) -> Result<Self> {
        for id in [send_id, recv_id] {
            if id > CAN_SFF_MAX {
                return Err(CanError::InvalidCanId(id));
            }
        }
        if send_id + extra_send_offset > CAN_SFF_MAX {
            return Err(CanError::InvalidCanId(send_id + extra_send_offset));
        }
        Ok(Self {
            motor_type,
            send_id,
            recv_id,
            state: MotorState::default(),
            passes_since_state: 0,
        })
    }

    pub fn state(&self) -> MotorState {
        self.state
    }

    /// Completed decode passes since this motor's last state frame. Saturates
    /// rather than wrapping, so a long-silent motor stays visibly silent.
    pub fn passes_since_state(&self) -> u32 {
        self.passes_since_state
    }

    pub fn motor_type(&self) -> MotorType {
        self.motor_type
    }

    pub fn send_id(&self) -> u32 {
        self.send_id
    }
}

enum CanSock {
    Classic(CanSocket),
    Fd(CanFdSocket),
}

impl CanSock {
    fn send(&self, frame: &OutFrame) -> Result<()> {
        let id = StandardId::new(frame.id as u16).expect("ids validated in MotorSlot::new");
        match self {
            Self::Classic(socket) => {
                let frame = CanFrame::new(id, &frame.data).expect("8 bytes fit a CAN frame");
                socket.write_frame(&frame)?;
            }
            Self::Fd(socket) => {
                let frame = CanFdFrame::with_flags(id, &frame.data, FdFlags::BRS)
                    .expect("8 bytes fit a CAN-FD frame");
                socket.write_frame(&frame)?;
            }
        }
        Ok(())
    }

    /// Reads one frame, waiting at most `timeout`. Returns `None` on timeout.
    fn recv(&self, timeout: Duration) -> Result<Option<CanAnyFrame>> {
        if !self.readable_within(timeout)? {
            return Ok(None);
        }
        let read = match self {
            Self::Classic(socket) => socket.read_frame().map(CanAnyFrame::from),
            Self::Fd(socket) => socket.read_frame(),
        };
        match read {
            Ok(frame) => Ok(Some(frame)),
            // The RECV_BACKSTOP timeout fired: poll readiness raced away.
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                Ok(None)
            }
            Err(e) => Err(e.into()),
        }
    }

    /// Waits for the socket to become readable via `ppoll`, which takes a
    /// nanosecond timeout; the crate-provided timed read only has millisecond
    /// resolution, which would flatten the firmware's microsecond receive
    /// windows. A signal during the wait reads as "nothing arrived", matching
    /// the reference implementation's select loop.
    fn readable_within(&self, timeout: Duration) -> Result<bool> {
        let socket = match self {
            Self::Classic(socket) => socket.as_raw_socket(),
            Self::Fd(socket) => socket.as_raw_socket(),
        };
        let mut fds = [PollFd::new(socket.as_fd(), PollFlags::POLLIN)];
        match ppoll(&mut fds, Some(TimeSpec::from(timeout)), None) {
            Ok(n) => Ok(n > 0),
            Err(Errno::EINTR) => Ok(false),
            Err(errno) => Err(std::io::Error::from(errno).into()),
        }
    }
}

/// A CAN interface with the motors that live on it.
pub(crate) struct MotorBus {
    socket: CanSock,
    slots: Vec<MotorSlot>,
}

impl MotorBus {
    pub fn open(interface: &str, enable_fd: bool, slots: Vec<MotorSlot>) -> Result<Self> {
        let open_err = |source| CanError::Open {
            interface: interface.to_owned(),
            source,
        };
        let socket = if enable_fd {
            CanSock::Fd(CanFdSocket::open(interface).map_err(open_err)?)
        } else {
            CanSock::Classic(CanSocket::open(interface).map_err(open_err)?)
        };
        // Backstop from the reference implementation: reads happen only after
        // a readiness poll, but if that invariant is ever wrong (kernel or
        // driver quirk), SO_RCVTIMEO bounds the blocking read instead of
        // letting it hang the caller's mutex forever. Kernel rounding makes
        // the effective bound one scheduler tick, not the literal value.
        let raw = match &socket {
            CanSock::Classic(s) => s.as_raw_socket(),
            CanSock::Fd(s) => s.as_raw_socket(),
        };
        raw.set_read_timeout(Some(RECV_BACKSTOP))
            .map_err(open_err)?;
        Ok(Self { socket, slots })
    }

    pub fn slots(&self) -> &[MotorSlot] {
        &self.slots
    }

    pub fn send(&mut self, frame: &OutFrame) -> Result<()> {
        self.socket.send(frame)
    }

    pub fn enable_all(&mut self) -> Result<()> {
        self.send_to_each(protocol::enable_frame)
    }

    pub fn disable_all(&mut self) -> Result<()> {
        self.send_to_each(protocol::disable_frame)
    }

    /// Writes the control mode to every motor (a parameter write on the
    /// shared param id).
    pub fn set_control_mode(&mut self, mode: ControlMode) -> Result<()> {
        self.send_to_each(|send_id| protocol::ctrl_mode_frame(send_id, mode))
    }

    /// Requests a state frame from every motor without commanding it.
    pub fn refresh_all(&mut self) -> Result<()> {
        self.send_to_each(protocol::refresh_frame)
    }

    /// Sends every frame even when one fails, reporting the first error.
    pub fn send_all(&mut self, frames: impl IntoIterator<Item = OutFrame>) -> Result<()> {
        first_error(frames.into_iter().map(|frame| self.socket.send(&frame)))
    }

    fn send_to_each(&mut self, frame_for: impl Fn(u32) -> OutFrame) -> Result<()> {
        first_error(
            self.slots
                .iter()
                .map(|slot| frame_for(slot.send_id))
                .map(|frame| self.socket.send(&frame)),
        )
    }

    /// Receives and decodes state frames into the cache: waits up to
    /// `first_timeout_us` for the first frame, then drains without waiting.
    /// Frames from unknown ids and undecodable payloads are ignored.
    pub fn recv_all(&mut self, first_timeout_us: u32) -> Result<()> {
        self.recv_until(
            first_timeout_us,
            Decode::AsOnePass,
            Instant::now() + TICK_RECV_WINDOW,
        )
    }

    /// Same receive pass as [`recv_all`](Self::recv_all) but discards every
    /// frame. Use to consume bus traffic that must not land in the state
    /// cache (bring-up replies, parameter echoes).
    pub fn drain(&mut self, first_timeout_us: u32) -> Result<()> {
        self.recv_loop(first_timeout_us, Decode::Discard)
    }

    /// Enable every motor and confirm each one acknowledges it, retrying the
    /// enable for stragglers. Blocking: bring-up is sequential, and a
    /// blocking settle keeps the whole attempt atomic rather than droppable
    /// half way through.
    ///
    /// A one-shot enable can fail to take. The motor then answers every poll
    /// at full rate with no error flag while ignoring commands, which is
    /// indistinguishable from a healthy motor until someone watches the
    /// metal, so the acknowledgment has to be read back rather than assumed.
    ///
    /// Confirmation is causal under two bus invariants this crate is built
    /// on: this process is the interface's sole commander (the nodes hold
    /// per-component instance locks), and a motor replies within
    /// milliseconds or not at all. The wire carries no correlation token,
    /// so those invariants, the drain, and the settle are what tie each
    /// collected frame to this attempt.
    pub fn enable_and_confirm(
        &mut self,
        attempts: NonZeroU32,
        settle: Duration,
        recv_timeout_us: u32,
    ) -> std::result::Result<(), EnableFailure> {
        let mut last = Unconfirmed::default();
        for attempt in 1..=attempts.get() {
            self.enable_all().map_err(EnableFailure::Can)?;
            std::thread::sleep(settle);
            last = self
                .read_confirmations(recv_timeout_us)
                .map_err(EnableFailure::Can)?;
            if last.is_empty() {
                return Ok(());
            }
            tracing::warn!("enable attempt {attempt}/{}: {last}", attempts.get());
        }
        Err(EnableFailure::Unconfirmed(last))
    }

    /// One enable-confirmation pass: drains pending enable ACKs, solicits a
    /// state frame per motor, and collects replies until every motor has
    /// answered or [`REPLY_WINDOW`] closes.
    ///
    /// Repeated receives are the point: one receive returns at the first
    /// momentary gap on the bus, and the requests alone occupy longer than
    /// that on a full arm, so the last motors' replies are still in flight
    /// when it gives up. Reading once would fail healthy motors. The whole
    /// pass counts as a single silence pass, so a motor that answers early
    /// in a long window does not leave bring-up looking stale.
    fn read_confirmations(&mut self, recv_timeout_us: u32) -> Result<Unconfirmed> {
        self.drain(recv_timeout_us)?;
        self.refresh_state(recv_timeout_us)?;
        Ok(unconfirmed(&self.slots))
    }

    /// Solicit a state frame from every motor and collect the replies as one
    /// silence pass, so the cache reflects a whole fresh pass rather than
    /// whichever motors answered first. `recv_timeout_us` is each wait for
    /// the next reply and must be positive: the pass repeats it until every
    /// motor answers or [`REPLY_WINDOW`] closes, and a zero wait would spin
    /// that whole window on the CPU while holding the bus lock.
    pub fn refresh_state(&mut self, recv_timeout_us: u32) -> Result<()> {
        if recv_timeout_us == 0 {
            return Err(CanError::ZeroReceiveTimeout);
        }
        // Already-queued frames predate the solicitation below and are not
        // replies to it; they are swept out first, so the pass holds only
        // frames the refresh produced.
        self.drain(0)?;
        self.refresh_all()?;
        begin_decode_pass(&mut self.slots);
        let deadline = Instant::now() + REPLY_WINDOW;
        while self.slots.iter().any(|slot| slot.passes_since_state != 0)
            && Instant::now() < deadline
        {
            self.recv_until(recv_timeout_us, Decode::WithoutCounting, deadline)?;
        }
        Ok(())
    }

    /// Read one register from every motor, in slot order: `None` where a
    /// motor did not answer inside [`REPLY_WINDOW`]. Read-only, so it is
    /// safe on motors that are disabled, faulted, or that must not move.
    /// Replies are matched by reply id and register id, the whole
    /// correlation the wire offers; freshness rests on the same sole
    /// commander and prompt-reply invariants as
    /// [`enable_and_confirm`](Self::enable_and_confirm). `recv_timeout_us`
    /// is each wait for the next reply and must be positive, exactly as in
    /// [`refresh_state`](Self::refresh_state).
    pub fn read_param(
        &mut self,
        param: MotorParam,
        recv_timeout_us: u32,
    ) -> Result<Vec<Option<f64>>> {
        if recv_timeout_us == 0 {
            return Err(CanError::ZeroReceiveTimeout);
        }
        self.drain(recv_timeout_us)?;
        let rid = param.rid();
        let send_ids: Vec<u32> = self.slots.iter().map(|slot| slot.send_id).collect();
        first_error(
            send_ids
                .iter()
                .map(|&send_id| protocol::query_param_frame(send_id, rid))
                .map(|frame| self.socket.send(&frame)),
        )?;

        let mut values: Vec<Option<f64>> = vec![None; send_ids.len()];
        let deadline = Instant::now() + REPLY_WINDOW;
        let receive_timeout = Duration::from_micros(recv_timeout_us.into());
        let mut timeout = receive_timeout;
        while values.iter().any(Option::is_none) && Instant::now() < deadline {
            // The deadline bounds every wait, the gaps between frames
            // included, and this loop holds the bus lock.
            let remaining = deadline.saturating_duration_since(Instant::now());
            let Some(frame) = self.socket.recv(timeout.min(remaining))? else {
                timeout = receive_timeout;
                continue;
            };
            timeout = Duration::ZERO;
            let (extended, id, data) = match &frame {
                CanAnyFrame::Normal(f) => (f.is_extended(), f.raw_id(), f.data().to_vec()),
                CanAnyFrame::Fd(f) => (f.is_extended(), f.raw_id(), f.data().to_vec()),
                CanAnyFrame::Remote(_) | CanAnyFrame::Error(_) => continue,
            };
            if extended {
                continue;
            }
            // Keyed on the motor's own reply id, not the payload alone: a
            // query this host sent carries the same payload signature, and
            // SocketCAN echoes it to every other socket on the interface.
            let Some(i) = self.slots.iter().position(|slot| slot.recv_id == id) else {
                continue;
            };
            if let Some((replied, value)) = protocol::parse_param_reply(send_ids[i], &data)
                && replied == rid
            {
                values[i] = Some(value);
            }
        }
        Ok(values)
    }

    fn recv_loop(&mut self, first_timeout_us: u32, decode: Decode) -> Result<()> {
        self.recv_until(first_timeout_us, decode, Instant::now() + REPLY_WINDOW)
    }

    /// One receive pass, abandoned at `deadline`. A busy bus can keep frames
    /// pending indefinitely, and this runs holding the bus lock the shutdown
    /// path needs, so the pass is bounded rather than draining to quiet.
    fn recv_until(
        &mut self,
        first_timeout_us: u32,
        decode: Decode,
        deadline: Instant,
    ) -> Result<()> {
        if decode == Decode::AsOnePass {
            begin_decode_pass(&mut self.slots);
        }
        // The deadline bounds the first wait exactly as it bounds the drain.
        let mut timeout = Duration::from_micros(first_timeout_us.into())
            .min(deadline.saturating_duration_since(Instant::now()));
        while let Some(frame) = self.socket.recv(timeout)? {
            timeout = Duration::ZERO;
            if decode != Decode::Discard {
                self.dispatch(&frame);
            }
            if Instant::now() >= deadline {
                break;
            }
        }
        Ok(())
    }

    fn dispatch(&mut self, frame: &CanAnyFrame) {
        // Remote and error frames can never be motor state; the Damiao
        // protocol replies with plain data frames only.
        let (extended, id, data) = match frame {
            CanAnyFrame::Normal(f) => (f.is_extended(), f.raw_id(), f.data()),
            CanAnyFrame::Fd(f) => (f.is_extended(), f.raw_id(), f.data()),
            CanAnyFrame::Remote(_) | CanAnyFrame::Error(_) => return,
        };
        decode_into_slots(&mut self.slots, extended, id, data);
    }
}

/// Drives every send in a group command and keeps the first error. Stopping at
/// the first failure would leave the group half applied: a partial disable has
/// some joints limp and some holding their last command (the arm folds
/// asymmetrically), and a partial refresh blinds the joints it skipped. The
/// reference implementation always addresses every motor.
fn first_error(results: impl Iterator<Item = Result<()>>) -> Result<()> {
    results.fold(Ok(()), Result::and)
}

/// Counts a decode pass against every slot up front, so a slot that decodes
/// nothing this pass (or a pass cut short by an I/O error) reads as silent.
/// Saturating: a long-silent motor must stay visibly silent, not wrap.
fn begin_decode_pass(slots: &mut [MotorSlot]) {
    for slot in slots {
        slot.passes_since_state = slot.passes_since_state.saturating_add(1);
    }
}

/// Which motors failed to confirm enable in the pass just completed, split
/// by what the bus showed: one that answered with the wrong status refused
/// the enable, one that never answered is silent. Different faults with
/// different operator actions, so they are not collapsed. Reads
/// `passes_since_state` as the evidence that a slot's status came from this
/// pass rather than an earlier one.
fn unconfirmed(slots: &[MotorSlot]) -> Unconfirmed {
    let answered = |slot: &MotorSlot| slot.passes_since_state == 0;
    Unconfirmed {
        refused: slots
            .iter()
            .filter(|slot| answered(slot) && slot.state.status != MotorStatus::Enabled)
            .map(|slot| slot.send_id)
            .collect(),
        silent: slots
            .iter()
            .filter(|slot| !answered(slot))
            .map(|slot| slot.send_id)
            .collect(),
    }
}

/// Decodes a received data frame into the matching slot's state cache.
/// Rejected without decoding: extended-id frames (motors address with 11-bit
/// ids only; `raw_id` strips the EFF flag, so without this gate a foreign
/// 29-bit frame whose low bits collide with a recv id would masquerade as
/// state) and parameter replies (the motor mirrors 0x7FF writes/queries back
/// on its state id; decoded as state, the echo reads as a violent full-scale
/// position).
fn decode_into_slots(slots: &mut [MotorSlot], extended: bool, id: u32, data: &[u8]) {
    if extended {
        return;
    }
    let Some(slot) = slots.iter_mut().find(|slot| slot.recv_id == id) else {
        return;
    };
    if protocol::is_param_frame(slot.send_id, data) {
        return;
    }
    if let Some(state) = protocol::parse_state(slot.motor_type, data) {
        slot.state = state;
        slot.passes_since_state = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gripper_slot() -> MotorSlot {
        MotorSlot::new(MotorType::DM4310, 0x08, 0x18, 0x300).unwrap()
    }

    /// DM4310 state: q at +p_max, dq at -v_max, tau at +t_max.
    const STATE: [u8; 8] = [0x00, 0xFF, 0xFF, 0x00, 0x0F, 0xFF, 0x30, 0x28];

    /// Every group send routes through `first_error`; this pins its two
    /// guarantees: the iterator is driven to the end (every motor addressed
    /// even after a failure) and the first error is the one reported.
    #[test]
    fn first_error_drives_every_send_and_reports_the_first_failure() {
        let attempted = std::cell::Cell::new(0u32);
        let outcome = first_error(
            [
                Ok(()),
                Err(CanError::InvalidCanId(1)),
                Err(CanError::InvalidCanId(2)),
                Ok(()),
            ]
            .into_iter()
            .inspect(|_| attempted.set(attempted.get() + 1)),
        );
        assert_eq!(attempted.get(), 4, "a failed send must not skip the rest");
        assert!(matches!(outcome, Err(CanError::InvalidCanId(1))));
    }

    #[test]
    fn first_error_of_all_ok_is_ok() {
        assert!(first_error([Ok(()), Ok(())].into_iter()).is_ok());
    }

    #[test]
    fn state_frame_updates_the_matching_slot() {
        let mut slots = [gripper_slot()];
        decode_into_slots(&mut slots, false, 0x18, &STATE);
        assert_eq!(slots[0].state().position, 12.5);
    }

    #[test]
    fn extended_id_frames_are_rejected() {
        // A 29-bit id whose low bits collide with the recv id must not
        // masquerade as motor state.
        let mut slots = [gripper_slot()];
        decode_into_slots(&mut slots, true, 0x18, &STATE);
        assert_eq!(slots[0].state(), MotorState::default());
    }

    #[test]
    fn param_reply_echoes_are_rejected() {
        // The CTRL_MODE write (0x55) and query (0x33) echoes carry the send
        // id little-endian then the opcode; decoded as state they would read
        // as a full-scale position.
        let mut slots = [gripper_slot()];
        for opcode in [0x55, 0x33] {
            decode_into_slots(
                &mut slots,
                false,
                0x18,
                &[0x08, 0x00, opcode, 0x0A, 4, 0, 0, 0],
            );
            assert_eq!(slots[0].state(), MotorState::default());
        }
    }

    #[test]
    fn echo_shaped_frame_for_a_different_motor_still_decodes() {
        // The reply signature is keyed to this slot's send id; the same bytes
        // with a mismatched id prefix are treated as (implausible) state.
        let mut slots = [gripper_slot()];
        decode_into_slots(
            &mut slots,
            false,
            0x18,
            &[0x07, 0x00, 0x55, 0x0A, 4, 0, 0, 0],
        );
        assert_ne!(slots[0].state(), MotorState::default());
    }

    #[test]
    fn unknown_ids_are_ignored() {
        let mut slots = [gripper_slot()];
        decode_into_slots(&mut slots, false, 0x19, &STATE);
        assert_eq!(slots[0].state(), MotorState::default());
    }

    #[test]
    fn silence_counts_decode_passes_and_a_state_frame_resets_it() {
        let mut slots = [gripper_slot()];
        begin_decode_pass(&mut slots);
        begin_decode_pass(&mut slots);
        assert_eq!(slots[0].passes_since_state(), 2);
        decode_into_slots(&mut slots, false, 0x18, &STATE);
        assert_eq!(
            slots[0].passes_since_state(),
            0,
            "a decoded state frame proves the motor is live"
        );
        begin_decode_pass(&mut slots);
        // A frame that is filtered out (unknown id) is not proof of life.
        decode_into_slots(&mut slots, false, 0x19, &STATE);
        assert_eq!(slots[0].passes_since_state(), 1);
    }

    #[test]
    fn silence_saturates_instead_of_wrapping() {
        let mut slots = [gripper_slot()];
        slots[0].passes_since_state = u32::MAX;
        begin_decode_pass(&mut slots);
        assert_eq!(slots[0].passes_since_state(), u32::MAX);
    }

    #[test]
    fn confirmation_separates_a_refusal_from_silence() {
        // Enabled state frame (nibble 0x1) for the gripper slot, plus a
        // Disabled (0x0) and a faulted (0xE) one.
        let enabled = [0x10, 0x7F, 0xFF, 0x7F, 0xF7, 0xFF, 0x30, 0x28];
        let disabled = [0x00, 0x7F, 0xFF, 0x7F, 0xF7, 0xFF, 0x30, 0x28];
        let faulted = [0xE0, 0x7F, 0xFF, 0x7F, 0xF7, 0xFF, 0x30, 0x28];

        let mut slots = [gripper_slot()];
        begin_decode_pass(&mut slots);
        assert_eq!(
            unconfirmed(&slots),
            Unconfirmed {
                refused: vec![],
                silent: vec![0x08]
            },
            "a motor that never answered is silent, not a refusal"
        );

        for frame in [disabled, faulted] {
            let mut slots = [gripper_slot()];
            begin_decode_pass(&mut slots);
            decode_into_slots(&mut slots, false, 0x18, &frame);
            assert_eq!(
                unconfirmed(&slots),
                Unconfirmed {
                    refused: vec![0x08],
                    silent: vec![]
                },
                "a motor answering with the wrong status refused the enable"
            );
        }

        let mut slots = [gripper_slot()];
        begin_decode_pass(&mut slots);
        decode_into_slots(&mut slots, false, 0x18, &enabled);
        assert!(unconfirmed(&slots).is_empty());
    }

    #[test]
    fn a_stale_enabled_status_does_not_confirm_a_now_silent_motor() {
        // The status cache persists across passes, so confirmation must key
        // on this pass's evidence: a motor that confirmed earlier and then
        // dropped out must not read as still confirmed.
        let enabled = [0x10, 0x7F, 0xFF, 0x7F, 0xF7, 0xFF, 0x30, 0x28];
        let mut slots = [gripper_slot()];
        begin_decode_pass(&mut slots);
        decode_into_slots(&mut slots, false, 0x18, &enabled);
        assert!(unconfirmed(&slots).is_empty());

        begin_decode_pass(&mut slots);
        assert_eq!(
            unconfirmed(&slots),
            Unconfirmed {
                refused: vec![],
                silent: vec![0x08]
            }
        );
    }
}
