//! Prints one arm's per-motor identity, configured limits, and live state.
//!
//! Read-only: it sends parameter queries and state requests and nothing
//! else, so it neither enables nor commands a motor and cannot move the arm.
//! Safe on a disabled, faulted, or partly dead robot; the motors need only
//! power and a reachable bus.
//!
//! Run it against both arms to compare a suspect joint with its opposite
//! number, and again after a repair to confirm the joint came back with the
//! same identity and limits as its sibling:
//!
//! `cargo run --example probe_arm -- left_arm`
//!
//! A motor that answers nothing is not on the bus at all: unpowered,
//! unplugged, wrong id, or dead. A motor that answers with an unexpected
//! `esc id` or `mstr id` is a replacement still carrying its factory
//! defaults, and one whose `mstr id` differs from its siblings' pattern will
//! act on commands while its replies go to an id nobody reads.

use openarm_can::{ARM_DOF, ARM_MOTOR_TYPES, ArmCan, EffectiveRatings, MotorParam, MotorStatus};

/// Generous windows: this runs once, by hand, and a slow reply is worth
/// waiting for.
const RECV_TIMEOUT_US: u32 = 2000;

fn main() {
    let Some(interface) = std::env::args().nth(1) else {
        eprintln!("usage: probe_arm <can-interface>");
        std::process::exit(2);
    };
    let mut arm = match ArmCan::open(&interface, true) {
        Ok(arm) => arm,
        Err(e) => {
            eprintln!("open {interface}: {e}");
            std::process::exit(1);
        }
    };

    // A register that could not be read is not evidence about the motor, so
    // an unread register is tracked separately and reported: the headline
    // must never claim scales were checked when none were read.
    let mut unread: Vec<String> = Vec::new();
    let mut read = |arm: &mut ArmCan, param| match arm.read_param(param, RECV_TIMEOUT_US) {
        Ok(values) => {
            unread.extend(
                (0..ARM_DOF)
                    .filter(|&j| values[j].is_none())
                    .map(|j| format!("j{} {param:?}", j + 1)),
            );
            values
        }
        Err(e) => {
            eprintln!("read {param:?}: {e}");
            unread.push(format!("all {param:?}: {e}"));
            [None; ARM_DOF]
        }
    };
    let esc_id = read(&mut arm, MotorParam::EscId);
    let master_id = read(&mut arm, MotorParam::MasterId);
    let timeout = read(&mut arm, MotorParam::Timeout);
    let control_mode = read(&mut arm, MotorParam::ControlMode);
    let gear_ratio = read(&mut arm, MotorParam::GearRatio);
    let over_current = read(&mut arm, MotorParam::OverCurrentLimit);
    let over_temp = read(&mut arm, MotorParam::OverTempLimit);
    let position_max = read(&mut arm, MotorParam::PositionMax);
    let velocity_max = read(&mut arm, MotorParam::VelocityMax);
    let torque_max = read(&mut arm, MotorParam::TorqueMax);

    // A state request is not a command: it asks the motor to report itself.
    let state_read_ok = match arm.refresh_state(RECV_TIMEOUT_US) {
        Ok(()) => true,
        Err(e) => {
            eprintln!("state read: {e}");
            false
        }
    };
    let state = arm.get_state();

    println!("{interface}");
    println!(
        "  {:<4} {:<8} {:<7} {:<7} {:<8} {:<5} {:<5} {:<6} {:<7} {:<9} {:<9} status",
        "",
        "model",
        "esc id",
        "mstr id",
        "timeout",
        "mode",
        "gear",
        "oc",
        "ot (C)",
        "tmax (Nm)",
        "trip (Nm)"
    );
    // What each motor's registers say its thresholds should be, decided by
    // the same call the arm node makes so this preview cannot drift from the
    // thresholds the node will judge the motor against.
    let effective: Vec<_> = (0..ARM_DOF)
        .map(|joint| {
            ARM_MOTOR_TYPES[joint].ratings().map(|datasheet| {
                EffectiveRatings::from_registers(datasheet, over_current[joint], torque_max[joint])
            })
        })
        .collect();

    for joint in 0..ARM_DOF {
        // Shed the float noise a wire value picks up without touching the
        // integer part: trimming digits off "40.0000" would print 4.
        let show = |v: &Option<f64>| match v {
            Some(value) if value.fract().abs() < 1e-6 => format!("{}", value.round()),
            Some(value) => format!("{value:.3}"),
            None => "--".to_string(),
        };
        let trip = match effective[joint].and_then(|e| e.trip_nm) {
            Some(nm) => format!("{nm:.2}"),
            None => "--".to_string(),
        };
        let status = match state.statuses[joint] {
            MotorStatus::Unreported => "SILENT (no reply)".to_string(),
            other => format!("{other:?}"),
        };
        println!(
            "  j{:<3} {:<8} {:<7} {:<7} {:<8} {:<5} {:<5} {:<6} {:<7} {:<9} {:<9} {}",
            joint + 1,
            format!("{:?}", ARM_MOTOR_TYPES[joint]),
            show(&esc_id[joint]),
            show(&master_id[joint]),
            show(&timeout[joint]),
            show(&control_mode[joint]),
            show(&gear_ratio[joint]),
            show(&over_current[joint]),
            show(&over_temp[joint]),
            show(&torque_max[joint]),
            trip,
            status,
        );
    }

    println!("\n  thresholds the health filter would use:");
    for joint in 0..ARM_DOF {
        let Some(effective) = effective[joint] else {
            continue;
        };
        let note = if effective.is_tightened() {
            " (tightened: the motor trips below its datasheet peak)"
        } else {
            ""
        };
        let (rated_nm, peak_nm) = (effective.ratings.rated_nm(), effective.ratings.peak_nm());
        println!(
            "    j{}: warn near {rated_nm:.1} Nm, critical at {peak_nm:.1} Nm{note}",
            joint + 1
        );
    }

    // A motor quantizes each field against its own full scale. If any of the
    // three is not the scale the driver decodes with, every reading on that
    // axis is off by a constant factor, so the readings and every threshold
    // built on them are meaningless until the two agree.
    let scales = [
        (MotorParam::PositionMax, &position_max, "position", "rad"),
        (MotorParam::VelocityMax, &velocity_max, "velocity", "rad/s"),
        (MotorParam::TorqueMax, &torque_max, "torque", "Nm"),
    ];
    let mismatched: Vec<String> = (0..ARM_DOF)
        .flat_map(|joint| {
            let model = ARM_MOTOR_TYPES[joint];
            scales
                .iter()
                .filter_map(move |(param, readings, axis, unit)| {
                    let reported = readings[joint]?;
                    (model.scale_matches(*param, reported) == Some(false)).then(|| {
                    format!(
                        "j{}: reports {reported} {unit} {axis} full scale, decoded as {} {unit}",
                        joint + 1,
                        model.decode_full_scale(*param).expect("scale register")
                    )
                })
                })
        })
        .collect();
    if !mismatched.is_empty() {
        println!("\n  SCALE MISMATCH (readings on that axis are wrong):");
        for line in &mismatched {
            println!("    {line}");
        }
    }

    let silent: Vec<String> = (0..ARM_DOF)
        .filter(|&j| state.statuses[j] == MotorStatus::Unreported)
        .map(|j| format!("j{}", j + 1))
        .collect();
    if !silent.is_empty() {
        println!("\n  SILENT: {}", silent.join(", "));
    }
    let misconfigured: Vec<String> = (0..ARM_DOF)
        .filter(|&j| effective[j].is_some_and(|e| e.trip_too_low))
        .map(|j| {
            format!(
                "j{}: cuts out at {:.2} Nm, too close to its {:.1} Nm continuous rating to warn early",
                j + 1,
                effective[j].and_then(|e| e.trip_nm).unwrap_or(f64::NAN),
                effective[j]
                    .map(|e| e.ratings.rated_nm())
                    .unwrap_or(f64::NAN)
            )
        })
        .collect();
    if !misconfigured.is_empty() {
        println!("\n  TRIP TOO CLOSE TO RATING (judged against the datasheet instead):");
        for line in &misconfigured {
            println!("    {line}");
        }
    }
    if !unread.is_empty() {
        println!("\n  UNREAD REGISTERS (nothing was verified for these):");
        println!("    {}", unread.join(", "));
    }
    if state_read_ok && silent.is_empty() && mismatched.is_empty() && unread.is_empty() {
        println!("\n  every motor answered, on the expected decode scales");
        if misconfigured.is_empty() {
            return;
        }
    }
    std::process::exit(1);
}
