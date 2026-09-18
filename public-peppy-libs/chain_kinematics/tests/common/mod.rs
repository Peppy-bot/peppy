//! The two real robots the integration suites drive: a 5-DOF SO-101 and a
//! 7-DOF OpenArm, loaded through the public API exactly as a consumer would.
//!
//! Each integration test is its own crate, so items unused by one suite are
//! expected; the allow keeps clippy's -D warnings gate meaningful for the rest.
#![allow(dead_code)]

use chain_kinematics::{Chain, ChainSpec, JointSelection};

pub const SO101: &str = include_str!("../fixtures/so101_5dof.urdf");
pub const OPENARM: &str = include_str!("../fixtures/openarm_7dof.urdf");

pub const SO101_JOINTS: [&str; 5] = [
    "shoulder_pan",
    "shoulder_lift",
    "elbow_flex",
    "wrist_flex",
    "wrist_roll",
];

pub fn so101() -> Chain<5> {
    let robot = urdf_rs::read_from_string(SO101).expect("parse SO-101");
    Chain::<5>::from_urdf(
        &robot,
        &ChainSpec {
            base_link: None,
            tip_link: "gripper_frame_link",
            joints: JointSelection::Named(&SO101_JOINTS),
        },
    )
    .expect("SO-101 is a five-joint chain")
}

pub fn openarm() -> Chain<7> {
    let robot = urdf_rs::read_from_string(OPENARM).expect("parse OpenArm");
    Chain::<7>::from_urdf(
        &robot,
        &ChainSpec {
            base_link: Some("openarm_left_link0"),
            tip_link: "openarm_left_link7",
            joints: JointSelection::PathOrder,
        },
    )
    .expect("OpenArm is a seven-joint chain")
}

/// A configuration spread across each joint's range, deterministically.
pub fn sample<const N: usize>(chain: &Chain<N>, k: usize) -> [f64; N] {
    let limits = chain.limits();
    std::array::from_fn(|i| {
        let t = ((k + 1) as f64 * 0.618_033_988_749_894_9 * (i + 1) as f64).fract();
        limits[i].lo + t * (limits[i].hi - limits[i].lo)
    })
}

/// SplitMix64: a tiny deterministic generator for uniform test draws, so a
/// suite's verdict is a function of the code alone.
pub struct SplitMix64(pub u64);

impl SplitMix64 {
    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Uniform in [0, 1): the top 53 bits, a full mantissa's worth.
    pub fn unit(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 * (1.0 / (1u64 << 53) as f64)
    }

    /// A configuration drawn uniformly from the joint box.
    pub fn config<const N: usize>(&mut self, chain: &Chain<N>) -> [f64; N] {
        let limits = chain.limits();
        let mut q = [0.0; N];
        for (value, limit) in q.iter_mut().zip(limits.iter()) {
            *value = limit.lo + self.unit() * (limit.hi - limit.lo);
        }
        q
    }
}
