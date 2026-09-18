//! Shared control-loop primitives for the openarm control nodes.
//!
//! - [`pacer`]: fixed-rate pacing for a control loop, with overrun accounting.
//! - [`filters`]: scalar signal filters ([`LowPassFilter`](filters::LowPassFilter),
//!   [`ButterworthFilter`](filters::ButterworthFilter),
//!   [`HampelFilter`](filters::HampelFilter)).
//! - [`time`]: a configured rate or seconds value as a `Duration` to wait on.
//! - [`minimum_jerk`]: the quintic profile and the duration a velocity budget
//!   implies for it.
//! - [`motor_health`]: judging one motor's load, temperature and condition
//!   against its ratings.
//! - [`positive_finite`]: a positive finite float, carried as proof it was checked.
//! - [`servo`]: the tolerances that decide when a Cartesian goal is reached.
//! - [`throttle`]: admitting a repeating event at most once per window.
//!
//! Each fallible operation names its own failure, so a signature says exactly
//! what can go wrong with it rather than what can go wrong anywhere in the
//! crate.
//!
//! The bimanual backbone (openarm_backbone) and the real arm
//! (openarm_arm) both pace their real-time control loops with
//! [`Pacer`](pacer::Pacer); this is their one tested implementation. A home for
//! further control primitives as they are factored out of the nodes.

pub mod filters;
pub mod minimum_jerk;
pub mod motor_health;
pub mod pacer;
pub mod positive_finite;
pub mod servo;
pub mod throttle;
pub mod time;
