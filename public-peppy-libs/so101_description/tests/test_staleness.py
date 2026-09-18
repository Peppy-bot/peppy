"""The family's staleness policy: the values themselves are the contract."""

from so101_description import staleness


def test_both_limits_are_positive_and_finite():
    # A non-positive or infinite limit disarms the gate it belongs to: the
    # first would refuse everything, the second would accept a backlog of
    # any age as current.
    for limit in (staleness.WIRE_AGE_LIMIT_S, staleness.COMMAND_SILENCE_LIMIT_S):
        assert 0.0 < limit < float("inf")


def test_the_limits_bound_the_worst_case_stop():
    # A consumer that passes a sample through both gates may act on it for
    # the sum, so the composed figure is the one an operator sizing risk
    # cares about. Pinned so a change to either limit surfaces here.
    composed = staleness.WIRE_AGE_LIMIT_S + staleness.COMMAND_SILENCE_LIMIT_S
    assert composed == 0.5
