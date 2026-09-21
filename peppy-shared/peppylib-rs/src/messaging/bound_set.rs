use config::runtime::{BoundMember, BoundProducers, ProducerRef};

/// The bound producer set of a `cardinality: "one_or_more"` consumer slot, in
/// plan order and never empty. Generated `bound_producers()` accessors of
/// `one_or_more` slots return it. The sibling cardinalities keep their own
/// shapes (`one` lends the sole `&ProducerRef`, `zero_or_one` an
/// `Option<&ProducerRef>`, `zero_or_more` returns a plain, possibly empty
/// `Vec<ProducerRef>`), so flipping a slot's cardinality changes the accessor's
/// type and surfaces every affected call site at compile time.
pub type NonEmptyProducers = super::NonEmpty<ProducerRef>;

/// The bound member set of a `cardinality: "one_or_more"` consumer slot: every
/// producer with the copy its instance belongs to, in plan order and never
/// empty. Generated `bound_members()` accessors of `one_or_more` slots return
/// it; those of `zero_or_more` slots return a plain, possibly empty
/// `Vec<BoundMember>`, the same split as [`NonEmptyProducers`].
pub type NonEmptyMembers = super::NonEmpty<BoundMember>;

/// Absolute producer-binding state for one consumer slot: the slot's complete
/// ordered producer set, in the order the plan listed it.
///
/// `sequence` orders `binding_update` deliveries so a delayed (stale) retry can
/// never roll the slot back; the listener rejects strictly-smaller sequences
/// and treats an equal sequence as an idempotent retry. Each delivery carries
/// the whole set and replaces it wholesale, so the producers a delivery omits
/// are gone from the slot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoundSetState {
    pub sequence: u64,
    pub producers: BoundProducers,
}

impl BoundSetState {
    /// A slot's boot state at sequence zero, carrying the boot config's binding
    /// for it. Live `binding_update` deliveries replace it at strictly larger
    /// sequences.
    pub fn seeded(producers: BoundProducers) -> Self {
        Self {
            sequence: 0,
            producers,
        }
    }
}
