"""How long an SO-101 hop keeps trusting what it was told.

Two independent judgments, kept together because they compose: a consumer
that passes a sample through both may act on it for the sum of the two.

The values are family policy, not per-node choices. The two ends of a link
must agree on them, or the composed stop latency changes without either end
saying so, and nothing in either node can see the disagreement.
"""

from __future__ import annotations

# How far from now an inbound wire stamp may sit before the sample is refused.
# Judged against the producer's own stamp, so it catches what arrival time
# cannot: a backlog drained after a consumer stall arrives fresh but carries
# old stamps, and a clock fault arrives stamped in the future.
WIRE_AGE_LIMIT_S = 0.25

# How long a consumer keeps acting on the last sample it accepted before
# treating the producer as gone. Judged against arrival time, so it answers
# the question no stamp can: is anyone still talking to me. The SO-101 leader
# arm has no engage switch, so on a leader-driven stack this silence is the
# whole deadman rather than a backstop behind one.
COMMAND_SILENCE_LIMIT_S = 0.25
