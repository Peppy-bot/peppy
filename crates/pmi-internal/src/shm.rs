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
//! (e.g. `LimitMEMLOCK=infinity` in the systemd unit, or `ulimit -l` ≥ 256
//! MiB — eight times the 32 MiB target, per the budget split above). Where
//! the limit cannot be raised, `shm.segment_bytes` in `peppy_config.json5`
//! claims an explicit per-session size instead; the setter owns the budget
//! arithmetic (at most `hard_limit / (publishing sessions + 1)`, with
//! headroom for zenoh's metadata segments).
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
/// Called at adapter construction when `shm.enabled` is on — BEFORE zenohd is
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
                if libc::setrlimit(libc::RLIMIT_MEMLOCK, &lim) != 0 {
                    let err = std::io::Error::last_os_error();
                    tracing::debug!(
                        %err,
                        requested_soft_limit = lim.rlim_cur,
                        hard_limit = lim.rlim_max,
                        "failed to raise RLIMIT_MEMLOCK to the hard limit"
                    );
                }
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

/// Where a segment size came from, so provider creation can log the right
/// thing: a memlock clamp is a host limitation worth a warning, an explicit
/// config override is an operator decision worth only a provenance line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SegmentSource {
    /// The budget (or its absence) allows the full [`SHM_SEGMENT_BYTES`].
    Target,
    /// `hard RLIMIT_MEMLOCK / 8` forced the size below the target.
    MemlockClamped,
    /// `shm.segment_bytes` was set; `requested` is the pre-clamp,
    /// pre-alignment value so the log can show what the operator asked for.
    ConfigOverride { requested: usize },
}

/// The segment size this host can sustain, and where it came from. Without an
/// override: the [`SHM_SEGMENT_BYTES`] target, clamped to an eighth of the
/// memlock budget. A fraction — not the whole — because the budget is also
/// spent on the receive side: zenoh caches (and keeps locked) a mapping of
/// every co-located producer's segment this process ever receives from, plus
/// its own metadata/watchdog segments; an eighth keeps even a many-sessions
/// process (the integration test shape) inside the budget. Payloads larger
/// than the segment fall back to the network path (logged once at provider
/// creation).
///
/// An explicit `shm.segment_bytes` bypasses the memlock math — the setter
/// owns the budget arithmetic — but is still clamped to the floor/target
/// window and alignment-rounded, so an override can never fail provider
/// creation by construction. It is the escape hatch for hosts that can't
/// raise the memlock limit and for benches that know their own
/// segment-mapping shape; production sizing raises the memlock limit, which
/// scales with the session count where a fixed override does not.
fn resolve_segment_bytes(
    override_bytes: Option<usize>,
    memlock_hard: Option<u64>,
) -> (usize, SegmentSource) {
    if let Some(requested) = override_bytes {
        return (
            aligned_segment_bytes(requested),
            SegmentSource::ConfigOverride { requested },
        );
    }
    let bytes = match memlock_hard {
        None => SHM_SEGMENT_BYTES,
        Some(hard) => usize::try_from(hard / 8).unwrap_or(SHM_SEGMENT_BYTES),
    };
    let bytes = aligned_segment_bytes(bytes);
    let source = if bytes < SHM_SEGMENT_BYTES {
        SegmentSource::MemlockClamped
    } else {
        SegmentSource::Target
    };
    (bytes, source)
}

/// [`resolve_segment_bytes`] against this process's actual memlock limit,
/// raising the soft limit to the hard limit first.
fn effective_segment_bytes(override_bytes: Option<usize>) -> (usize, SegmentSource) {
    raise_memlock_limit();
    resolve_segment_bytes(override_bytes, memlock_hard_limit())
}

/// Clamps a candidate segment size into the floor/target window and rounds it
/// DOWN to the alloc alignment: zenoh's `MemoryLayout` rejects any size that
/// is not a multiple of its alignment, so an odd memlock limit (or config
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
    /// `shm.segment_bytes` from the session's config, `None` for auto-sizing.
    segment_bytes_override: Option<usize>,
}

impl LazyShm {
    pub(crate) fn new(segment_bytes_override: Option<usize>) -> Arc<Self> {
        Arc::new(Self {
            cell: OnceLock::new(),
            segment_bytes_override,
        })
    }

    fn get(&self) -> Option<&Arc<ShmContext>> {
        self.cell
            .get_or_init(|| ShmContext::create(self.segment_bytes_override))
            .as_ref()
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
    fn create(segment_bytes_override: Option<usize>) -> Option<Arc<Self>> {
        let (segment_bytes, source) = effective_segment_bytes(segment_bytes_override);
        match source {
            SegmentSource::Target => {}
            SegmentSource::MemlockClamped => tracing::warn!(
                segment_bytes,
                target_bytes = SHM_SEGMENT_BYTES,
                "RLIMIT_MEMLOCK clamps the shared-memory segment below its target; \
                 payloads larger than the segment fall back to the network path \
                 (raise the memlock limit, e.g. LimitMEMLOCK= in the systemd unit, \
                 or set shm.segment_bytes in peppy_config.json5)"
            ),
            // Unconditional, even when the override equals the target: the
            // line answers "who sized this segment"; a clamped or alignment-
            // rounded override shows as requested_bytes != segment_bytes.
            SegmentSource::ConfigOverride { requested } => tracing::info!(
                segment_bytes,
                target_bytes = SHM_SEGMENT_BYTES,
                requested_bytes = requested,
                "shared-memory segment size set by shm.segment_bytes"
            ),
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
        if len == 0 {
            tracing::trace!("zero-length SHM loans fall back to the heap path");
            return None;
        }

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
        let shm = LazyShm::new(None);
        let loan = ShmContext::loan(Some(&shm), SHM_PUBLISH_THRESHOLD_BYTES);
        assert!(loan.is_shm(), "threshold-sized loan must be SHM-backed");
        assert_eq!(loan.len(), SHM_PUBLISH_THRESHOLD_BYTES);

        let small = ShmContext::loan(Some(&shm), SHM_PUBLISH_THRESHOLD_BYTES - 1);
        assert!(!small.is_shm(), "sub-threshold loan stays on the heap");
    }

    /// An overridden session must serve loans bigger than the auto-sized
    /// segment would on a small-memlock host (2 MiB > the 1 MiB floor the
    /// common 8 MiB hard limit clamps to) — the end-to-end point of the
    /// `shm.segment_bytes` knob. Only discriminates a dropped override where
    /// the auto segment is small; the shrinking test below covers the
    /// unlimited-memlock hosts.
    #[test]
    fn lazy_state_honors_segment_override() {
        let shm = LazyShm::new(Some(2 * 1024 * 1024));
        let len = 3 * 1024 * 1024 / 2;
        let loan = ShmContext::loan(Some(&shm), len);
        assert!(loan.is_shm(), "loan within the overridden segment is SHM");
        assert_eq!(loan.len(), len);
    }

    /// The complement of the test above for hosts whose auto-sized segment is
    /// large (unlimited memlock — the common CI shape): an override SHRINKS
    /// the segment, so a loan that exceeds it must fall back to the heap. A
    /// dropped override would hand out a 32 MiB auto segment here and serve
    /// the loan from SHM.
    #[test]
    fn lazy_state_override_bounds_loans() {
        let (auto_bytes, _) = effective_segment_bytes(None);
        if auto_bytes <= 2 * 1024 * 1024 {
            // Auto sizing is already at or below the loan size on this host;
            // the test above discriminates here instead.
            return;
        }
        let shm = LazyShm::new(Some(SHM_SEGMENT_MIN_BYTES));
        let loan = ShmContext::loan(Some(&shm), 2 * 1024 * 1024);
        assert!(
            !loan.is_shm(),
            "loan beyond the overridden segment must fall back to the heap"
        );
    }

    /// The segment is sized to the host's memlock budget but never leaves the
    /// configured floor/target window.
    #[test]
    fn segment_size_respects_memlock_budget() {
        let (bytes, _source) = effective_segment_bytes(None);
        assert!((SHM_SEGMENT_MIN_BYTES..=SHM_SEGMENT_BYTES).contains(&bytes));
        if let Some(hard) = memlock_hard_limit() {
            assert!(bytes as u64 <= hard.max(SHM_SEGMENT_MIN_BYTES as u64));
        }
    }

    /// No override, no limit: the full target, and no clamp reported (a
    /// spurious `MemlockClamped` here would warn on every unlimited host).
    #[test]
    fn no_override_no_limit_yields_target() {
        assert_eq!(
            resolve_segment_bytes(None, None),
            (SHM_SEGMENT_BYTES, SegmentSource::Target)
        );
    }

    /// A budget of eight targets or more sustains the full segment: nothing
    /// was clamped, so nothing may be reported as clamped.
    #[test]
    fn no_override_huge_limit_still_reports_target() {
        assert_eq!(
            resolve_segment_bytes(None, Some(1024 * 1024 * 1024)),
            (SHM_SEGMENT_BYTES, SegmentSource::Target)
        );
        // The boundary: exactly eight targets.
        assert_eq!(
            resolve_segment_bytes(None, Some(8 * SHM_SEGMENT_BYTES as u64)),
            (SHM_SEGMENT_BYTES, SegmentSource::Target)
        );
    }

    /// Small budgets clamp to an eighth (floored at the minimum) and say so.
    /// 8 MiB is the common systemd default — the shape this knob exists for.
    #[test]
    fn no_override_small_limit_clamps() {
        assert_eq!(
            resolve_segment_bytes(None, Some(8 * 1024 * 1024)),
            (SHM_SEGMENT_MIN_BYTES, SegmentSource::MemlockClamped)
        );
        assert_eq!(
            resolve_segment_bytes(None, Some(64 * 1024 * 1024)),
            (8 * 1024 * 1024, SegmentSource::MemlockClamped)
        );
    }

    /// An override bypasses the memlock math entirely — the setter owns the
    /// budget arithmetic — and carries the requested value for the log.
    #[test]
    fn override_respected_within_window_and_bypasses_memlock() {
        assert_eq!(
            resolve_segment_bytes(Some(4 * 1024 * 1024), Some(8 * 1024 * 1024)),
            (
                4 * 1024 * 1024,
                SegmentSource::ConfigOverride {
                    requested: 4 * 1024 * 1024
                }
            )
        );
    }

    /// Out-of-window overrides are clamped, never honored into a size that
    /// could fail provider creation; `requested` keeps the original value.
    #[test]
    fn override_clamped_into_window() {
        assert_eq!(
            resolve_segment_bytes(Some(512 * 1024), None),
            (
                SHM_SEGMENT_MIN_BYTES,
                SegmentSource::ConfigOverride {
                    requested: 512 * 1024
                }
            )
        );
        assert_eq!(
            resolve_segment_bytes(Some(64 * 1024 * 1024), None),
            (
                SHM_SEGMENT_BYTES,
                SegmentSource::ConfigOverride {
                    requested: 64 * 1024 * 1024
                }
            )
        );
    }

    /// An unaligned override is rounded down to the alloc alignment.
    #[test]
    fn override_rounded_down_to_alignment() {
        assert_eq!(
            resolve_segment_bytes(Some(2_000_001), None),
            (
                2_000_000,
                SegmentSource::ConfigOverride {
                    requested: 2_000_001
                }
            )
        );
    }

    /// An override equal to the target still reports `ConfigOverride`, so the
    /// provenance line fires even when the value changes nothing — it answers
    /// "who sized this segment", not "did the size change".
    #[test]
    fn override_equal_to_target_keeps_config_source() {
        assert_eq!(
            resolve_segment_bytes(Some(SHM_SEGMENT_BYTES), Some(8 * 1024 * 1024)),
            (
                SHM_SEGMENT_BYTES,
                SegmentSource::ConfigOverride {
                    requested: SHM_SEGMENT_BYTES
                }
            )
        );
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
