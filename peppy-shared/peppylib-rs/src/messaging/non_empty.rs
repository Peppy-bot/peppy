//! [`NonEmpty`]: the ordered set a `cardinality: "one_or_more"` slot's
//! generated accessor returns, never empty by construction.

/// An ordered snapshot of a `one_or_more` slot's members, in plan order, that is
/// never empty by construction, so the "at least one" guarantee lives in the
/// type: [`first`](Self::first) is infallible and there is no empty branch to
/// write. A producer-binding slot's accessor returns it as
/// [`NonEmptyProducers`](super::NonEmptyProducers), an observer slot's as
/// [`NonEmptyObservedSources`](super::NonEmptyObservedSources).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NonEmpty<T> {
    members: Vec<T>,
}

// `len` is always at least 1; clippy asks for an `is_empty` beside it.
#[allow(clippy::len_without_is_empty)]
impl<T> NonEmpty<T> {
    /// Wraps `members` as a non-empty set, or `None` when the list is empty.
    /// Runtime callers go through the `one_or_more` accessors
    /// ([`Processor::non_empty_bound_producers`],
    /// [`ObservationSlotSet::non_empty_sources`]), which read a slot whose
    /// cardinality admits no empty set; this checked constructor exists so the
    /// invariant cannot be sidestepped elsewhere.
    ///
    /// [`Processor::non_empty_bound_producers`]: crate::runtime::Processor::non_empty_bound_producers
    /// [`ObservationSlotSet::non_empty_sources`]: crate::runtime::ObservationSlotSet::non_empty_sources
    pub fn new(members: Vec<T>) -> Option<Self> {
        if members.is_empty() {
            return None;
        }
        Some(Self { members })
    }

    /// The first member in plan order. Infallible: the set is never empty.
    pub fn first(&self) -> &T {
        &self.members[0]
    }

    /// Iterates the members in plan order.
    pub fn iter(&self) -> std::slice::Iter<'_, T> {
        self.members.iter()
    }

    /// Number of members, always at least 1.
    pub fn len(&self) -> usize {
        self.members.len()
    }

    /// The members as a plain slice, for the slice-shaped APIs a node author
    /// reaches through the generated `bound_producers()`.
    pub fn as_slice(&self) -> &[T] {
        &self.members
    }
}

impl<T> IntoIterator for NonEmpty<T> {
    type Item = T;
    type IntoIter = std::vec::IntoIter<T>;

    fn into_iter(self) -> Self::IntoIter {
        self.members.into_iter()
    }
}

impl<'a, T> IntoIterator for &'a NonEmpty<T> {
    type Item = &'a T;
    type IntoIter = std::slice::Iter<'a, T>;

    fn into_iter(self) -> Self::IntoIter {
        self.members.iter()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::messaging::ProducerRef;

    fn producers() -> Vec<ProducerRef> {
        vec![
            ProducerRef::new("core-1234", "front_camera"),
            ProducerRef::new("core-1234", "rear_camera"),
        ]
    }

    #[test]
    fn an_empty_list_is_rejected_at_construction() {
        assert_eq!(NonEmpty::<ProducerRef>::new(Vec::new()), None);
    }

    #[test]
    fn first_iter_len_and_as_slice_preserve_plan_order() {
        let expected = producers();
        let set = NonEmpty::new(producers()).expect("two members are non-empty");

        assert_eq!(set.first(), &expected[0], "first() is the plan's head");
        assert_eq!(set.len(), 2);
        assert_eq!(set.as_slice(), &expected[..]);
        assert_eq!(
            set.iter().collect::<Vec<_>>(),
            expected.iter().collect::<Vec<_>>(),
            "iteration follows plan order"
        );
    }

    /// The by-value loop consumes the set, so it comes last.
    #[test]
    fn for_loops_work_by_reference_and_by_value() {
        let set = NonEmpty::new(producers()).expect("two members are non-empty");

        let mut seen = Vec::new();
        for member in &set {
            seen.push(member.instance_id.clone());
        }
        for member in set {
            seen.push(member.instance_id);
        }
        assert_eq!(
            seen,
            ["front_camera", "rear_camera", "front_camera", "rear_camera"]
        );
    }
}
