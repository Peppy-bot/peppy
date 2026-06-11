//! Adapter-side shared-memory support: the per-session POSIX provider, the
//! copy-into-SHM publish tier, and the loaned-buffer allocation that backs
//! [`crate::LoanedPayload`].
//!
//! ## The two publish tiers
//!
//! 1. **Plain `publish(Payload)`** — unchanged API. At/above
//!    [`SHM_PUBLISH_THRESHOLD_BYTES`] with SHM on, the payload bytes are copied
//!    ONCE into a loaned SHM buffer ([`ShmContext::payload_into_zbytes`]); that
//!    single copy replaces serialization plus two kernel copies through the
//!    loopback stack. Below the threshold, or with SHM off, the payload travels
//!    the heap path exactly as before.
//! 2. **Loaned payloads** — true zero-copy, opt-in per call site. The caller
//!    fills a buffer obtained from [`ShmContext::alloc`] (surfaced as
//!    `publisher.loan(len)`) and publishes it; the bytes are *born* in shared
//!    memory and never copied again.
//!
//! ## The locked-memory budget
//!
//! zenoh's SHM implementation `mlock`s every segment it creates — and every
//! remote segment it maps on the receive side — so `RLIMIT_MEMLOCK` is the
//! real capacity limit, not `/dev/shm`. Three consequences shape this module:
//!
//! * the soft limit is raised to the hard limit once per process (and before
//!   zenohd is spawned, so the router inherits it);
//! * the segment size targets [`SHM_SEGMENT_BYTES`] but is clamped to an
//!   eighth of the memlock budget, leaving room for the same process to map
//!   co-located producers' segments;
//! * the provider is created lazily on the first qualifying publish/loan, so
//!   subscriber-only sessions don't lock a segment they never write to.
//!
//! Hosts that want full-size segments set the memlock limit accordingly
//! (e.g. `LimitMEMLOCK=infinity` in the systemd unit, or `ulimit -l` ≥ 128
//! MiB); root and `CAP_IPC_LOCK` processes are unlimited.
//!
//! ## Fallbacks (never block, never error)
//!
//! Every SHM failure degrades to exactly today's behavior: provider creation
//! failure (memlock budget, exhausted `/dev/shm`) leaves the session on the
//! heap path with a warning; per-allocation exhaustion — even after the
//! policy's garbage-collect pass — falls back to the heap for that payload.
//! The allocation policy is [`GarbageCollect`] without a defragment stage
//! because the default POSIX backend in zenoh 1.9 is a talc allocator whose
//! `defragment()` is a no-op.

use crate::types::{LoanedInner, LoanedPayload, Payload};
use std::sync::{Arc, OnceLock};
use zenoh::Wait;
use zenoh::bytes::ZBytes;
use zenoh::shm::{
    AllocAlignment, GarbageCollect, PosixShmProviderBackend, ShmProvider, ShmProviderBuilder,
    ZShmMut,
};

/// Payloads at or above this size take the shared-memory path (both tiers).
/// Below a page, the loopback copy beats SHM's allocation + reference
/// bookkeeping; zenoh's published SHM benchmarks put the crossover in the
/// low-KB range. Validated against the latency bench's payload sweep — tune
/// here, not in config.
pub const SHM_PUBLISH_THRESHOLD_BYTES: usize = 4 * 1024;

/// Target POSIX segment size per publishing session: holds several in-flight
/// multi-MB payloads (camera frames) while two co-located sessions still fit
/// under Docker's default 64 MiB `/dev/shm`. The effective size is clamped
/// down on hosts with a small `RLIMIT_MEMLOCK` (see the module docs).
pub const SHM_SEGMENT_BYTES: usize = 32 * 1024 * 1024;

/// Floor for the memlock-clamped segment: below this, per-frame payloads
/// barely fit and the bookkeeping overhead dominates.
const SHM_SEGMENT_MIN_BYTES: usize = 1024 * 1024;

/// Loaned buffers are 8-byte aligned so typed in-place encodings (capnp
/// segments are 8-byte words) can build directly into a loan.
const SHM_ALLOC_ALIGNMENT: AllocAlignment = AllocAlignment::ALIGN_8_BYTES;
/// [`SHM_ALLOC_ALIGNMENT`] as a byte count, for size padding (zenoh's
/// `MemoryLayout` rejects sizes that aren't a multiple of the alignment).
const SHM_ALLOC_ALIGN_BYTES: usize = 8;

/// Raises this process's soft `RLIMIT_MEMLOCK` to the hard limit, once.
/// Called at adapter construction when the `shm` knob is on — BEFORE zenohd is
/// spawned, so the router process inherits the raised limit and can map node
/// segments in `router+shm` mode — and again (idempotent) before sizing the
/// provider segment.
#[cfg(unix)]
pub(crate) fn raise_memlock_limit() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        // SAFETY: plain libc rlimit calls on a stack value.
        unsafe {
            let mut lim = libc::rlimit {
                rlim_cur: 0,
                rlim_max: 0,
            };
            if libc::getrlimit(libc::RLIMIT_MEMLOCK, &mut lim) == 0 && lim.rlim_cur < lim.rlim_max {
                lim.rlim_cur = lim.rlim_max;
                let _ = libc::setrlimit(libc::RLIMIT_MEMLOCK, &lim);
            }
        }
    });
}

#[cfg(not(unix))]
pub(crate) fn raise_memlock_limit() {}

/// The hard memlock limit in bytes, `None` when unlimited or unreadable.
#[cfg(unix)]
fn memlock_hard_limit() -> Option<u64> {
    // SAFETY: plain libc rlimit call on a stack value.
    unsafe {
        let mut lim = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        if libc::getrlimit(libc::RLIMIT_MEMLOCK, &mut lim) != 0 {
            return None;
        }
        if lim.rlim_max == libc::RLIM_INFINITY {
            return None;
        }
        Some(lim.rlim_max)
    }
}

#[cfg(not(unix))]
fn memlock_hard_limit() -> Option<u64> {
    None
}

/// The segment size this host can sustain: the [`SHM_SEGMENT_BYTES`] target,
/// clamped to an eighth of the memlock budget. A fraction — not the whole —
/// because the budget is also spent on the receive side: zenoh caches (and
/// keeps locked) a mapping of every co-located producer's segment this
/// process ever receives from, plus its own metadata/watchdog segments; an
/// eighth keeps even a many-sessions process (the integration test shape)
/// inside the budget. Payloads larger than the clamped segment fall back to
/// the network path (warned once at provider creation).
///
/// `PEPPY_SHM_SEGMENT_BYTES` overrides the computed size (still clamped to
/// the floor/target window). It is a diagnostic/bench hook for processes
/// that know their own segment-mapping shape — NOT a supported config knob;
/// the supported way to get full-size segments is raising the memlock limit.
fn effective_segment_bytes() -> usize {
    raise_memlock_limit();
    let bytes = match std::env::var("PEPPY_SHM_SEGMENT_BYTES")
        .ok()
        .and_then(|value| value.trim().parse::<usize>().ok())
    {
        Some(parsed) => parsed,
        None => match memlock_hard_limit() {
            None => SHM_SEGMENT_BYTES,
            Some(hard) => usize::try_from(hard / 8).unwrap_or(SHM_SEGMENT_BYTES),
        },
    };
    aligned_segment_bytes(bytes)
}

/// Clamps a candidate segment size into the floor/target window and rounds it
/// DOWN to the alloc alignment: zenoh's `MemoryLayout` rejects any size that
/// is not a multiple of its alignment, so an odd memlock limit (or env
/// override) must not be able to fail provider creation — that failure is
/// cached for the whole session.
fn aligned_segment_bytes(bytes: usize) -> usize {
    bytes.clamp(SHM_SEGMENT_MIN_BYTES, SHM_SEGMENT_BYTES) / SHM_ALLOC_ALIGN_BYTES
        * SHM_ALLOC_ALIGN_BYTES
}

/// A session's lazily-created shared-memory state. `Some` cell content means
/// the provider was created (or definitively failed); the cell is filled on
/// the first qualifying publish or loan, so subscriber-only sessions never
/// spend locked memory on a segment they wouldn't write to.
pub(crate) struct LazyShm {
    cell: OnceLock<Option<Arc<ShmContext>>>,
}

impl LazyShm {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            cell: OnceLock::new(),
        })
    }

    fn get(&self) -> Option<&Arc<ShmContext>> {
        self.cell.get_or_init(ShmContext::create).as_ref()
    }
}

/// A session's shared-memory provider over one fixed-size POSIX segment.
/// Shared (`Arc`) with every declared publisher and response token of the
/// session via [`LazyShm`].
pub(crate) struct ShmContext {
    provider: ShmProvider<PosixShmProviderBackend>,
}

impl ShmContext {
    /// Creates the provider, or `None` (with a warning) when the segment
    /// cannot be created — the session then runs entirely on the heap path,
    /// which is exactly the pre-SHM behavior.
    fn create() -> Option<Arc<Self>> {
        let segment_bytes = effective_segment_bytes();
        if segment_bytes < SHM_SEGMENT_BYTES {
            tracing::warn!(
                segment_bytes,
                target_bytes = SHM_SEGMENT_BYTES,
                "RLIMIT_MEMLOCK clamps the shared-memory segment below its target; \
                 payloads larger than the segment fall back to the network path \
                 (raise the memlock limit, e.g. LimitMEMLOCK= in the systemd unit, \
                 for full-size segments)"
            );
        }
        // The backend is built with the alloc alignment: a provider only
        // serves allocations whose alignment is compatible with its backend
        // layout, so requesting 8-byte allocs from a default (1-byte) backend
        // fails with `ProviderIncompatibleLayout`.
        match PosixShmProviderBackend::builder((segment_bytes, SHM_ALLOC_ALIGNMENT)).wait() {
            Ok(backend) => Some(Arc::new(Self {
                provider: ShmProviderBuilder::backend(backend).wait(),
            })),
            Err(err) => {
                tracing::warn!(
                    %err,
                    segment_bytes,
                    "could not create the shared-memory segment; \
                     this session falls back to the network path"
                );
                None
            }
        }
    }

    /// Allocates a writable SHM buffer of exactly `len` bytes, or `None` when
    /// the pool is exhausted even after the policy's garbage-collect pass.
    fn alloc(&self, len: usize) -> Option<ZShmMut> {
        // `MemoryLayout` rejects sizes that aren't a multiple of the
        // alignment, so allocate padded and shrink back to the exact length
        // (resizing within chunk capacity always succeeds).
        let padded = len.div_ceil(SHM_ALLOC_ALIGN_BYTES) * SHM_ALLOC_ALIGN_BYTES;
        match self
            .provider
            .alloc((padded, SHM_ALLOC_ALIGNMENT))
            .with_policy::<GarbageCollect>()
            .wait()
        {
            Ok(mut buf) => {
                if padded != len {
                    use zenoh::shm::OwnedShmBuf;
                    buf.try_resize(std::num::NonZeroUsize::new(len)?)?;
                }
                Some(buf)
            }
            Err(err) => {
                tracing::debug!(?err, len, "SHM allocation failed; using the heap path");
                None
            }
        }
    }

    /// Tier 1: converts an outgoing payload into the `ZBytes` handed to zenoh,
    /// copying it into shared memory when it qualifies (SHM on, length at or
    /// above the threshold, pool not exhausted). All publish-shaped paths —
    /// topics, declared publishers, service requests and replies — funnel
    /// through here so every call site gets the same tiering.
    pub(crate) fn payload_into_zbytes(shm: Option<&Arc<LazyShm>>, payload: Payload) -> ZBytes {
        if payload.len() >= SHM_PUBLISH_THRESHOLD_BYTES
            && let Some(ctx) = shm.and_then(|s| s.get())
            && let Some(mut buf) = ctx.alloc(payload.len())
        {
            let mut at = 0;
            for slice in payload.slices() {
                buf[at..at + slice.len()].copy_from_slice(slice);
                at += slice.len();
            }
            return ZBytes::from(buf);
        }
        payload.into_zbytes()
    }

    /// Allocates a loan for `publisher.loan(len)`: SHM-backed when the length
    /// qualifies and the pool has room, heap-backed otherwise. Identical caller
    /// code either way — the loan only decides where the bytes are born.
    pub(crate) fn loan(shm: Option<&Arc<LazyShm>>, len: usize) -> LoanedPayload {
        if len >= SHM_PUBLISH_THRESHOLD_BYTES
            && let Some(ctx) = shm.and_then(|s| s.get())
            && let Some(buf) = ctx.alloc(len)
        {
            return LoanedPayload::from_shm(buf);
        }
        LoanedPayload::heap(len)
    }

    /// Converts a filled loan into the `ZBytes` to publish. An SHM-backed loan
    /// is handed over without copying (this is the zero-copy publish); a
    /// heap-backed loan goes through the plain tier-1 path, which may still
    /// recover a copy-into-SHM if the pool has freed up since the loan.
    pub(crate) fn loaned_into_zbytes(shm: Option<&Arc<LazyShm>>, loaned: LoanedPayload) -> ZBytes {
        match loaned.into_inner() {
            LoanedInner::Shm(buf) => ZBytes::from(buf),
            LoanedInner::Heap(vec) => {
                Self::payload_into_zbytes(shm, Payload::from_bytes(bytes::Bytes::from(vec)))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The provider must come up and hand out SHM-backed loans of qualifying
    /// sizes — if this fails, every integration-level SHM assertion fails
    /// with it, so the root cause surfaces here first.
    #[test]
    fn lazy_state_creates_provider_and_loans_shm_buffers() {
        let shm = LazyShm::new();
        let loan = ShmContext::loan(Some(&shm), SHM_PUBLISH_THRESHOLD_BYTES);
        assert!(loan.is_shm(), "threshold-sized loan must be SHM-backed");
        assert_eq!(loan.len(), SHM_PUBLISH_THRESHOLD_BYTES);

        let small = ShmContext::loan(Some(&shm), SHM_PUBLISH_THRESHOLD_BYTES - 1);
        assert!(!small.is_shm(), "sub-threshold loan stays on the heap");
    }

    /// The segment is sized to the host's memlock budget but never leaves the
    /// configured floor/target window.
    #[test]
    fn segment_size_respects_memlock_budget() {
        let bytes = effective_segment_bytes();
        assert!((SHM_SEGMENT_MIN_BYTES..=SHM_SEGMENT_BYTES).contains(&bytes));
        if let Some(hard) = memlock_hard_limit() {
            assert!(bytes as u64 <= hard.max(SHM_SEGMENT_MIN_BYTES as u64));
        }
    }

    /// An odd candidate (a hand-set override, a byte-precise memlock limit)
    /// must be rounded to the alloc alignment, not passed through to fail
    /// provider creation.
    #[test]
    fn segment_size_is_always_alloc_aligned() {
        for candidate in [
            2_000_001,
            SHM_SEGMENT_MIN_BYTES + 7,
            usize::MAX,
            0,
            5 * 1024 * 1024,
        ] {
            let bytes = aligned_segment_bytes(candidate);
            assert_eq!(bytes % SHM_ALLOC_ALIGN_BYTES, 0, "candidate {candidate}");
            assert!((SHM_SEGMENT_MIN_BYTES..=SHM_SEGMENT_BYTES).contains(&bytes));
        }
    }
}
