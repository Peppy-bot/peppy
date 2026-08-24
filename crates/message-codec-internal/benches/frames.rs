//! Conversion cost of camera-sized frames through the codec, next to the
//! JPEG transcoding the MCP runtime performs on the same frame.
//!
//! Run with `cargo bench -p message-codec`. Each measurement is the median
//! of repeated runs on one frame; the report prints the per-frame cost of
//! encoding and decoding through the codec and of encoding the same pixels
//! as JPEG, so the ratio between them is visible at a glance.

use image::codecs::jpeg::JpegEncoder;
use image::{ExtendedColorType, ImageEncoder};
use message_codec::MessageCodec;
use peppy_mcp_runtime::bridge::bytes_to_base64;
use serde_json::{Value, json};
use std::time::{Duration, Instant};

const FRAME_FORMAT: &str = r#"{
    frame: { $type: "array", $items: "u8" },
    encoding: "string",
    width: "u16",
    height: "u16",
    stamp: "time",
}"#;

const ROUNDS: usize = 20;

fn median(mut samples: Vec<Duration>) -> Duration {
    samples.sort_unstable();
    samples[samples.len() / 2]
}

fn measure(rounds: usize, mut run: impl FnMut()) -> Duration {
    median(
        (0..rounds)
            .map(|_| {
                let started = Instant::now();
                run();
                started.elapsed()
            })
            .collect(),
    )
}

fn rgb8_frame(width: usize, height: usize) -> Vec<u8> {
    (0..width * height * 3).map(|i| (i % 251) as u8).collect()
}

fn frame_value(pixels: &[u8], width: u16, height: u16) -> Value {
    json!({
        "frame": bytes_to_base64(pixels),
        "encoding": "rgb8",
        "width": width,
        "height": height,
        "stamp": "2026-08-24T12:00:00.000000000Z",
    })
}

fn jpeg_bytes(pixels: &[u8], width: u32, height: u32) -> Vec<u8> {
    let mut out = Vec::new();
    JpegEncoder::new_with_quality(&mut out, 80)
        .write_image(pixels, width, height, ExtendedColorType::Rgb8)
        .expect("the frame encodes as JPEG");
    out
}

fn main() {
    let format = serde_json5::from_str(FRAME_FORMAT).expect("the frame format parses");
    let codec = MessageCodec::new("frame_bench", format).expect("the frame format lays out");

    println!(
        "{:<12} {:>12} {:>12} {:>12} {:>12}",
        "frame", "wire bytes", "encode", "decode", "jpeg q80"
    );
    for (width, height) in [(640u16, 480u16), (1280, 720), (1920, 1080)] {
        let pixels = rgb8_frame(width as usize, height as usize);
        let value = frame_value(&pixels, width, height);
        let wire = codec.encode(&value).expect("the frame encodes");

        let encode = measure(ROUNDS, || {
            std::hint::black_box(codec.encode(&value).expect("encodes"));
        });
        let decode = measure(ROUNDS, || {
            std::hint::black_box(codec.decode(&wire).expect("decodes"));
        });
        let jpeg = measure(ROUNDS, || {
            std::hint::black_box(jpeg_bytes(&pixels, width as u32, height as u32));
        });
        println!(
            "{:<12} {:>12} {:>12.2?} {:>12.2?} {:>12.2?}",
            format!("{width}x{height}"),
            wire.len(),
            encode,
            decode,
            jpeg
        );
    }
}
