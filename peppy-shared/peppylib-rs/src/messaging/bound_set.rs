use config::runtime::ProducerRef;

/// The bound producer set of a `cardinality: "one_or_more"` consumer slot:
/// an ordered, immutable view over the slot's runtime-resolved producers
/// that is never empty by construction. Generated `bound_producers()`
/// accessors of `one_or_more` slots return this instead of a plain slice so
/// the launch-validated "at least one" guarantee lives in the type rather
/// than in a comment: [`first`](Self::first) is infallible and there is no
/// empty branch to write. The sibling cardinalities keep their own shapes
/// (`one` returns the sole `&ProducerRef` directly, `zero_or_one` an
/// `Option<&ProducerRef>`, `zero_or_more` a plain, possibly empty
/// `&[ProducerRef]`), so flipping a slot's cardinality
/// changes the accessor's type and surfaces every affected call site at
/// compile time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NonEmptyProducers<'a> {
    producers: &'a [ProducerRef],
}

// `is_empty` is deliberately absent: the constructor rejects empty slices,
// so it would be a constant `false`.
#[allow(clippy::len_without_is_empty)]
impl<'a> NonEmptyProducers<'a> {
    /// Wraps `producers` as a non-empty set, or `None` when the slice is
    /// empty. Runtime callers go through
    /// [`Processor::non_empty_bound_producers`], which validated the slot's
    /// cardinality at node startup; this checked constructor exists so the
    /// invariant cannot be sidestepped elsewhere.
    ///
    /// [`Processor::non_empty_bound_producers`]: crate::runtime::Processor::non_empty_bound_producers
    pub fn new(producers: &'a [ProducerRef]) -> Option<Self> {
        if producers.is_empty() {
            return None;
        }
        Some(Self { producers })
    }

    /// The first producer in application declaration order. Infallible: the
    /// set is never empty, so unlike `slice::first` there is no `Option` to
    /// unwrap.
    pub fn first(&self) -> &'a ProducerRef {
        &self.producers[0]
    }

    /// Iterates the members in application declaration order.
    pub fn iter(&self) -> std::slice::Iter<'a, ProducerRef> {
        self.producers.iter()
    }

    /// Number of members, always at least 1.
    pub fn len(&self) -> usize {
        self.producers.len()
    }

    /// The members as a plain slice, for slice-shaped APIs such as
    /// [`TopicMessenger::subscribe_bound_set`](super::TopicMessenger::subscribe_bound_set)
    /// and order assertions in tests.
    pub fn as_slice(&self) -> &'a [ProducerRef] {
        self.producers
    }
}

impl<'a> IntoIterator for NonEmptyProducers<'a> {
    type Item = &'a ProducerRef;
    type IntoIter = std::slice::Iter<'a, ProducerRef>;

    fn into_iter(self) -> Self::IntoIter {
        self.producers.iter()
    }
}

impl<'a> IntoIterator for &NonEmptyProducers<'a> {
    type Item = &'a ProducerRef;
    type IntoIter = std::slice::Iter<'a, ProducerRef>;

    fn into_iter(self) -> Self::IntoIter {
        self.producers.iter()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn producers() -> Vec<ProducerRef> {
        vec![
            ProducerRef::new("core-1234", "front_camera"),
            ProducerRef::new("core-1234", "rear_camera"),
        ]
    }

    #[test]
    fn empty_slice_is_rejected_at_construction() {
        assert_eq!(NonEmptyProducers::new(&[]), None);
    }

    #[test]
    fn first_iter_len_and_as_slice_preserve_declaration_order() {
        let producers = producers();
        let set = NonEmptyProducers::new(&producers).expect("two members are non-empty");

        assert_eq!(set.first(), &producers[0], "first() is the declared head");
        assert_eq!(set.len(), 2);
        assert_eq!(set.as_slice(), &producers[..]);
        assert_eq!(
            set.iter().collect::<Vec<_>>(),
            producers.iter().collect::<Vec<_>>(),
            "iteration follows application declaration order"
        );
    }

    #[test]
    fn for_loops_work_by_value_and_by_reference() {
        let producers = producers();
        let set = NonEmptyProducers::new(&producers).expect("two members are non-empty");

        let mut seen = Vec::new();
        for member in &set {
            seen.push(member.instance_id.as_str());
        }
        // The set is `Copy`, so consuming it in a by-value loop leaves the
        // original binding usable.
        for member in set {
            seen.push(member.instance_id.as_str());
        }
        assert_eq!(
            seen,
            ["front_camera", "rear_camera", "front_camera", "rear_camera"]
        );
    }
}
