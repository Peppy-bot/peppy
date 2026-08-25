//! The runtime codec against typed accessors compiled from the same
//! schemas, in process.
//!
//! For each fixture message the typed side builds the message the way a
//! generated node's serializer does: fields in declaration order, every
//! pointer target allocated at its field, into a default heap allocator,
//! framed with `write_message`. The codec must produce the identical bytes
//! from the fixture's JSON, decode the typed bytes back to that JSON, and
//! the typed readers must read the codec's bytes back to the same values.

use message_codec::MessageCodec;
use peppy_mcp_runtime::bridge::{bytes_to_base64, value_bytes, value_time};
use peppylib::encoding::convert_time;
use serde_json::Value;
use std::path::PathBuf;

#[allow(clippy::all, dead_code)]
mod everything_capnp {
    include!(concat!(env!("OUT_DIR"), "/everything_capnp.rs"));
}

#[allow(clippy::all, dead_code)]
mod frame_capnp {
    include!(concat!(env!("OUT_DIR"), "/frame_capnp.rs"));
}

fn fixture(name: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join(name);
    std::fs::read_to_string(&path).unwrap_or_else(|_| panic!("{} is readable", path.display()))
}

fn codec(name: &str) -> MessageCodec {
    let format = serde_json5::from_str(&fixture(&format!("{name}.json5"))).expect("format parses");
    MessageCodec::new(name, format).expect("the fixture format lays out")
}

fn frame_bytes(message: &capnp::message::Builder<capnp::message::HeapAllocator>) -> Vec<u8> {
    let mut bytes = Vec::new();
    capnp::serialize::write_message(&mut bytes, message).expect("frames");
    bytes
}

fn base64(value: &Value) -> Vec<u8> {
    value_bytes(value, "fixture").expect("fixture bytes are base64")
}

fn stamp(value: &Value) -> peppylib::encoding::CapnpTimestamp {
    convert_time(value_time(value, "fixture").expect("fixture times are canonical"))
}

fn set_timestamp(
    mut builder: everything_capnp::timestamp::Builder<'_>,
    stamp: peppylib::encoding::CapnpTimestamp,
) {
    builder.set_sec(stamp.sec);
    builder.set_nsec(stamp.nsec);
}

/// Builds the `everything` message from `value` with the typed accessors,
/// in the order the generated serializer assigns fields.
fn typed_everything(value: &Value) -> Vec<u8> {
    use everything_capnp::message;

    let field = |name: &str| &value[name];
    let optional = |name: &str| value.get(name);
    let strings = |name: &str| -> Vec<&str> {
        field(name)
            .as_array()
            .expect("array")
            .iter()
            .map(|item| item.as_str().expect("string"))
            .collect()
    };

    let mut message = capnp::message::Builder::new_default();
    {
        let mut root = message.init_root::<message::Builder<'_>>();
        root.set_flag(field("flag").as_bool().unwrap());
        root.set_label(field("label").as_str().unwrap());
        root.set_blob(&base64(field("blob")));
        set_timestamp(root.reborrow().init_stamp(), stamp(field("stamp")));
        root.set_tiny(field("tiny").as_u64().unwrap() as u8);
        root.set_small(field("small").as_u64().unwrap() as u16);
        root.set_medium(field("medium").as_u64().unwrap() as u32);
        root.set_big(field("big").as_str().unwrap().parse().unwrap());
        root.set_tiny_signed(field("tiny_signed").as_i64().unwrap() as i8);
        root.set_small_signed(field("small_signed").as_i64().unwrap() as i16);
        root.set_medium_signed(field("medium_signed").as_i64().unwrap() as i32);
        root.set_big_signed(field("big_signed").as_str().unwrap().parse().unwrap());
        root.set_ratio(field("ratio").as_f64().unwrap() as f32);
        root.set_precise(field("precise").as_f64().unwrap());
        if let Some(note) = optional("note") {
            root.set_note(note.as_str().unwrap());
        }
        if let Some(attachment) = optional("attachment") {
            root.set_attachment(&base64(attachment));
        }
        if let Some(seen_at) = optional("seen_at") {
            set_timestamp(root.reborrow().init_seen_at(), stamp(seen_at));
        }
        root.set_checksum(&base64(field("checksum")));
        root.set_pixels(&base64(field("pixels")));
        {
            let gains = field("gains").as_array().unwrap();
            let mut list = root.reborrow().init_gains(3);
            for (index, gain) in gains.iter().enumerate() {
                list.set(index as u32, gain.as_f64().unwrap() as f32);
            }
        }
        {
            let offsets = field("offsets").as_array().unwrap();
            let mut list = root.reborrow().init_offsets(2);
            for (index, offset) in offsets.iter().enumerate() {
                list.set(index as u32, offset.as_i64().unwrap() as i16);
            }
        }
        {
            let flags = field("flags").as_array().unwrap();
            let mut list = root.reborrow().init_flags(flags.len() as u32);
            for (index, flag) in flags.iter().enumerate() {
                list.set(index as u32, flag.as_bool().unwrap());
            }
        }
        {
            let counters = strings("counters");
            let mut list = root.reborrow().init_counters(counters.len() as u32);
            for (index, counter) in counters.iter().enumerate() {
                list.set(index as u32, counter.parse::<u64>().unwrap());
            }
        }
        {
            let deltas = strings("deltas");
            let mut list = root.reborrow().init_deltas(deltas.len() as u32);
            for (index, delta) in deltas.iter().enumerate() {
                list.set(index as u32, delta.parse::<i64>().unwrap());
            }
        }
        {
            let weights = field("weights").as_array().unwrap();
            let mut list = root.reborrow().init_weights(weights.len() as u32);
            for (index, weight) in weights.iter().enumerate() {
                list.set(index as u32, weight.as_f64().unwrap());
            }
        }
        {
            let tags = strings("tags");
            let mut list = root.reborrow().init_tags(tags.len() as u32);
            for (index, tag) in tags.iter().enumerate() {
                list.set(index as u32, *tag);
            }
        }
        {
            let chunks = field("chunks").as_array().unwrap();
            let mut list = root.reborrow().init_chunks(chunks.len() as u32);
            for (index, chunk) in chunks.iter().enumerate() {
                list.set(index as u32, &base64(chunk));
            }
        }
        {
            let pose = field("pose");
            let mut builder = root.reborrow().init_pose();
            builder.set_x_m(pose["x_m"].as_f64().unwrap());
            builder.set_y_m(pose["y_m"].as_f64().unwrap());
            builder.set_frame(pose["frame"].as_str().unwrap());
        }
        {
            let profile = field("profile");
            let mut builder = root.reborrow().init_profile();
            builder.set_gamma(profile["gamma"].as_f64().unwrap());
            let mut white_balance = builder.reborrow().init_white_balance();
            white_balance.set_red(profile["white_balance"]["red"].as_f64().unwrap() as f32);
            white_balance.set_blue(profile["white_balance"]["blue"].as_f64().unwrap() as f32);
        }
        {
            let samples = field("samples").as_array().unwrap();
            let mut list = root.reborrow().init_samples(samples.len() as u32);
            for (index, sample) in samples.iter().enumerate() {
                let mut element = list.reborrow().get(index as u32);
                element.set_offset(sample["offset"].as_i64().unwrap() as i16);
                element.set_value(sample["value"].as_f64().unwrap());
                element.set_label(sample["label"].as_str().unwrap());
                set_timestamp(
                    element.reborrow().init_taken_at(),
                    stamp(&sample["taken_at"]),
                );
                let history = sample["history"].as_array().unwrap();
                let mut history_list = element.reborrow().init_history(history.len() as u32);
                for (index, entry) in history.iter().enumerate() {
                    history_list.set(index as u32, entry.as_u64().unwrap() as u32);
                }
            }
        }
        if let Some(maybe_pose) = optional("maybe_pose") {
            let mut builder = root.reborrow().init_maybe_pose();
            builder.set_x_m(maybe_pose["x_m"].as_f64().unwrap());
        }
        if let Some(maybe_tags) = optional("maybe_tags") {
            let tags: Vec<&str> = maybe_tags
                .as_array()
                .unwrap()
                .iter()
                .map(|tag| tag.as_str().unwrap())
                .collect();
            let mut list = root.reborrow().init_maybe_tags(tags.len() as u32);
            for (index, tag) in tags.iter().enumerate() {
                list.set(index as u32, *tag);
            }
        }
    }
    frame_bytes(&message)
}

fn assert_everything_round_trips(value: &Value) {
    let codec = codec("everything");
    let typed = typed_everything(value);
    let runtime = codec.encode(value).expect("the fixture encodes");
    assert_eq!(
        runtime, typed,
        "the codec writes the bytes the typed builder writes"
    );
    assert_eq!(
        codec.decode(&typed).expect("the typed bytes decode"),
        *value,
        "the codec reads the typed bytes back to the fixture"
    );

    // The typed readers see the codec's bytes as the fixture's values.
    let message = capnp::serialize::read_message(runtime.as_slice(), Default::default())
        .expect("the codec's bytes are a framed message");
    let root = message
        .get_root::<everything_capnp::message::Reader<'_>>()
        .expect("root reads");
    assert_eq!(root.get_flag(), value["flag"].as_bool().unwrap());
    assert_eq!(
        root.get_label().unwrap().to_str().unwrap(),
        value["label"].as_str().unwrap()
    );
    assert_eq!(bytes_to_base64(root.get_blob().unwrap()), value["blob"]);
    assert_eq!(
        root.get_big(),
        value["big"].as_str().unwrap().parse::<u64>().unwrap()
    );
    assert_eq!(root.has_note(), value.get("note").is_some());
    assert_eq!(root.has_seen_at(), value.get("seen_at").is_some());
    assert_eq!(root.has_maybe_pose(), value.get("maybe_pose").is_some());
    assert_eq!(root.has_maybe_tags(), value.get("maybe_tags").is_some());
    let samples = root.get_samples().unwrap();
    assert_eq!(
        samples.len() as usize,
        value["samples"].as_array().unwrap().len()
    );
    for (index, sample) in value["samples"].as_array().unwrap().iter().enumerate() {
        let element = samples.get(index as u32);
        assert_eq!(
            element.get_offset(),
            sample["offset"].as_i64().unwrap() as i16
        );
        assert_eq!(
            element.get_label().unwrap().to_str().unwrap(),
            sample["label"].as_str().unwrap()
        );
        let taken_at = element.get_taken_at().unwrap();
        let expected = stamp(&sample["taken_at"]);
        assert_eq!(
            (taken_at.get_sec(), taken_at.get_nsec()),
            (expected.sec, expected.nsec)
        );
    }
}

#[test]
fn everything_with_every_optional_present_matches_the_typed_builder() {
    let value: Value = serde_json::from_str(&fixture("everything.full.json")).expect("json");
    assert_everything_round_trips(&value);
}

#[test]
fn everything_with_every_optional_absent_and_lists_empty_matches_the_typed_builder() {
    let value: Value = serde_json::from_str(&fixture("everything.minimal.json")).expect("json");
    assert_everything_round_trips(&value);
}

#[test]
fn a_camera_frame_matches_the_typed_builder() {
    use frame_capnp::message;

    let (width, height) = (640u16, 480u16);
    let pixels: Vec<u8> = (0..width as usize * height as usize * 3)
        .map(|i| (i % 251) as u8)
        .collect();
    let value = serde_json::json!({
        "frame": bytes_to_base64(&pixels),
        "encoding": "rgb8",
        "width": width,
        "height": height,
        "stamp": "2026-08-24T12:00:00.000000000Z",
    });

    let mut message = capnp::message::Builder::new_default();
    {
        let mut root = message.init_root::<message::Builder<'_>>();
        root.set_frame(&pixels);
        root.set_encoding("rgb8");
        root.set_width(width);
        root.set_height(height);
        let stamp = stamp(&value["stamp"]);
        let mut builder = root.reborrow().init_stamp();
        builder.set_sec(stamp.sec);
        builder.set_nsec(stamp.nsec);
    }
    let typed = frame_bytes(&message);

    let codec = codec("frame");
    let runtime = codec.encode(&value).expect("the frame encodes");
    assert_eq!(
        runtime, typed,
        "a frame larger than one segment lays out identically"
    );
    assert_eq!(codec.decode(&typed).expect("decodes"), value);
}
