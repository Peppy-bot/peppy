//! Benchmark "sized probe" body codec.
//!
//! A `ServiceQueryKind::Probe` is normally empty and the transport adapter
//! auto-answers it empty, never running the producer's handler — shared by
//! liveness, discovery, node removal, and `stack benchmark`. To let the
//! benchmark measure REAL-payload latency without running the handler, its
//! probe carries a small body: a magic prefix, the desired response size,
//! then zero padding up to the real request size. The adapter parses it and
//! replies with that many bytes (still no handler). Everything else is
//! unaffected: an empty or unrecognized body — every liveness/discovery
//! probe — replies empty, exactly as before.
//!
//! Lives in pmi (not peppylib) because probe replies are produced inside the
//! adapters' query dispatch, so a producer answers probes even while user
//! code holds its request loop.

use bytes::Bytes;

/// Marks a probe body as a benchmark sized-probe. Liveness/discovery probes send
/// empty bodies, so the magic's absence means "reply empty, as before".
const SIZED_PROBE_MAGIC: [u8; 4] = *b"PBSZ";
/// Header = magic (4 bytes) + desired response size (little-endian `u32`).
const SIZED_PROBE_HEADER_LEN: usize = 8;
/// Upper bound on the reply size a sized probe may request. The size field
/// arrives off the network, so without a cap any peer could make a producer
/// allocate up to `u32::MAX` (4 GiB) per probe. A request above the cap is
/// treated like any other unrecognized body: not a sized probe, reply empty.
pub const MAX_PROBE_REPLY_SIZE: u32 = 64 * 1024 * 1024;

/// Build a sized-probe request body: the header followed by zero padding to
/// `request_size` total bytes (at least the header, so the response size always
/// survives). The producer replies with `response_size` bytes; sizes above
/// [`MAX_PROBE_REPLY_SIZE`] are rejected by the receiving adapter (it replies
/// empty).
pub fn build_sized_probe_request(request_size: usize, response_size: u32) -> Bytes {
    let total = request_size.max(SIZED_PROBE_HEADER_LEN);
    let mut buf = vec![0u8; total];
    buf[..4].copy_from_slice(&SIZED_PROBE_MAGIC);
    buf[4..8].copy_from_slice(&response_size.to_le_bytes());
    Bytes::from(buf)
}

/// Parse a probe request body: `Some(response_size)` for a benchmark sized-probe,
/// `None` for the empty bodies liveness/discovery send (→ reply empty) and for
/// sizes above [`MAX_PROBE_REPLY_SIZE`]. Both adapters interpret probes through
/// [`probe_response_body`], so the bound is enforced for every transport here.
pub(crate) fn parse_sized_probe_request(body: &[u8]) -> Option<u32> {
    if body.len() < SIZED_PROBE_HEADER_LEN || body[..4] != SIZED_PROBE_MAGIC {
        return None;
    }
    let size = u32::from_le_bytes([body[4], body[5], body[6], body[7]]);
    (size <= MAX_PROBE_REPLY_SIZE).then_some(size)
}

/// Build the body of a probe reply: `response_size` zero bytes for a sized
/// probe, empty for everything else. Shared by every adapter so probe
/// semantics cannot drift between transports.
pub(crate) fn probe_response_body(request_body: &[u8]) -> Bytes {
    match parse_sized_probe_request(request_body) {
        Some(size) => Bytes::from(vec![0u8; size as usize]),
        None => Bytes::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_pads_to_size_and_carries_response_size() {
        let p = build_sized_probe_request(64, 4096);
        assert_eq!(p.len(), 64);
        assert_eq!(parse_sized_probe_request(&p), Some(4096));
        assert_eq!(probe_response_body(&p).len(), 4096);
    }

    #[test]
    fn small_request_still_fits_the_header() {
        let p = build_sized_probe_request(0, 7);
        assert_eq!(p.len(), SIZED_PROBE_HEADER_LEN);
        assert_eq!(parse_sized_probe_request(&p), Some(7));
    }

    #[test]
    fn empty_or_unmarked_body_is_not_a_sized_probe() {
        assert_eq!(parse_sized_probe_request(&[]), None);
        assert_eq!(parse_sized_probe_request(b"hello world"), None);
        assert!(probe_response_body(&[]).is_empty());
        assert!(probe_response_body(b"hello world").is_empty());
    }

    #[test]
    fn reply_size_above_the_cap_is_rejected() {
        let at_cap = build_sized_probe_request(0, MAX_PROBE_REPLY_SIZE);
        assert_eq!(
            parse_sized_probe_request(&at_cap),
            Some(MAX_PROBE_REPLY_SIZE)
        );
        let over_cap = build_sized_probe_request(0, MAX_PROBE_REPLY_SIZE + 1);
        assert_eq!(parse_sized_probe_request(&over_cap), None);
        assert!(probe_response_body(&over_cap).is_empty());
    }
}
