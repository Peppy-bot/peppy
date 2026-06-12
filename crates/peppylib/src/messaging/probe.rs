//! Benchmark "sized probe" body codec.
//!
//! A `ServiceQueryKind::Probe` is normally empty and the producer auto-answers
//! empty, never running the handler — shared by liveness, discovery, node
//! removal, and `stack benchmark`. To let the benchmark measure REAL-payload
//! latency without running the handler, its probe carries a small body: a magic
//! prefix, the desired response size, then zero padding up to the real request
//! size. The producer's request loop parses it and replies with that many bytes
//! (still no handler). Everything else is unaffected: an empty or unrecognized
//! body — every liveness/discovery probe, and any producer built before this —
//! replies empty, exactly as before.

use std::time::Duration;

use crate::types::Payload;

/// One timed probe round-trip, as observed by the caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProbeSample {
    /// Round-trip time of the probe query.
    pub elapsed: Duration,
    /// Actual reply payload length (lets the caller detect a producer that did
    /// not honor the requested `response_size`).
    pub response_bytes: usize,
    /// Whether the reply payload arrived through a shared-memory segment —
    /// receive-side ground truth from the received buffer's SHM backing. This
    /// describes the **reply leg** only: the request leg is observable only on
    /// the producer's side and can take the other path when the request and
    /// response sizes straddle the SHM publish threshold.
    pub response_shm: bool,
}

/// Marks a probe body as a benchmark sized-probe. Liveness/discovery probes send
/// empty bodies, so the magic's absence means "reply empty, as before".
const SIZED_PROBE_MAGIC: [u8; 4] = *b"PBSZ";
/// Header = magic (4 bytes) + desired response size (little-endian `u32`).
const SIZED_PROBE_HEADER_LEN: usize = 8;

/// Build a sized-probe request body: the header followed by zero padding to
/// `request_size` total bytes (at least the header, so the response size always
/// survives). The producer replies with `response_size` bytes.
pub(crate) fn build_sized_probe_request(request_size: usize, response_size: u32) -> Payload {
    let total = request_size.max(SIZED_PROBE_HEADER_LEN);
    let mut buf = vec![0u8; total];
    buf[..4].copy_from_slice(&SIZED_PROBE_MAGIC);
    buf[4..8].copy_from_slice(&response_size.to_le_bytes());
    Payload::from(buf)
}

/// Parse a probe request body: `Some(response_size)` for a benchmark sized-probe,
/// `None` for the empty bodies liveness/discovery send (→ reply empty).
pub(crate) fn parse_sized_probe_request(body: &[u8]) -> Option<u32> {
    if body.len() < SIZED_PROBE_HEADER_LEN || body[..4] != SIZED_PROBE_MAGIC {
        return None;
    }
    Some(u32::from_le_bytes([body[4], body[5], body[6], body[7]]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_pads_to_size_and_carries_response_size() {
        let p = build_sized_probe_request(64, 4096);
        assert_eq!(p.as_ref().len(), 64);
        assert_eq!(parse_sized_probe_request(p.as_ref()), Some(4096));
    }

    #[test]
    fn small_request_still_fits_the_header() {
        let p = build_sized_probe_request(0, 7);
        assert_eq!(p.as_ref().len(), SIZED_PROBE_HEADER_LEN);
        assert_eq!(parse_sized_probe_request(p.as_ref()), Some(7));
    }

    #[test]
    fn empty_or_unmarked_body_is_not_a_sized_probe() {
        assert_eq!(parse_sized_probe_request(&[]), None);
        assert_eq!(parse_sized_probe_request(b"hello world"), None);
    }
}
