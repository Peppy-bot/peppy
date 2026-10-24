@0xa5c9659cafd96bfc;

# Daemon-to-daemon messages for a federated launch.
#
# Two kinds live here, and the distinction matters:
#
#   * The RESERVATION exchange, which the coordinator drives during preflight.
#     It is the serialization point for the whole launch: the coordinator holds
#     a reservation on every participant before anything is torn down, so two
#     coordinators racing the same machines are refused before either has
#     replaced a stack. Daemons never negotiate among themselves.
#
#   * RELATIONSHIP NOTIFICATIONS, which flow at runtime between the daemon that
#     owns an instance and daemons that hold a relationship with it. These are
#     best-effort and idempotent by construction: the owning daemon stays
#     authoritative and the notification only reports what already happened, so
#     a dropped one degrades to staleness rather than to disagreement.
#
# There is deliberately no "which daemons hold a slice of my launch" message.
# The stack-list response already carries `(launchId, coordinatorCoreNode)`, so
# rediscovery is the presence fan-out `stack list` already performs, filtered by
# launch id.

# Reserves one participant for one launch.
struct ParticipantReserveRequest {
    # Identity of the launch being reserved for.
    launchId @0 :Text;
    # The coordinator driving it. The participant watches this core node's
    # presence for as long as it holds the reservation, and releases it if the
    # coordinator disappears, so a coordinator that dies mid-launch cannot wedge
    # a machine until its next daemon restart.
    coordinatorCoreNode @1 :Text;
    # JSON5-encoded `DeploymentPins`, one per deployment placed on this
    # participant: the root node's pin plus the pin of every transitive node
    # dependency and every contract and pairing document in its closure. The
    # coordinator resolves the whole launch once and every participant runs
    # exactly those bytes, so a participant's own cache freshness, repository
    # priorities and exclusions never influence a federated launch. The
    # participant validates each pin here (a refusal costs nothing, since the
    # reservation is non-destructive) and materializes it at add time,
    # reusing its own copy on a content match and fetching the pinned commit
    # otherwise. It never falls back to resolving a name.
    #
    # Opaque text: the pin model lives in peppy, whose serde decoding is the
    # validation, and this crate has no business re-deriving it.
    #
    # A filesystem-backed repository entry never appears here. It names a
    # tree on the coordinator's own disk, so the plan refuses it for any
    # deployment placed off-coordinator.
    deploymentPinsJson5 @2 :List(Text);
}

# The reply of a participant that took the reservation. A participant that
# refuses gives an error reply: `precondition_failed` when it is already
# reserved for another launch or a launch already runs on it, `rejected`
# for a pin that does not validate.
struct ParticipantReserveResponse {
    # The participant's peppy version, compared against the coordinator's own
    # so a mixed-version federation is refused before any stack is touched.
    # Same string the info service reports, so there is one source of truth for
    # "what version is that daemon".
    peppyVersion @0 :Text;
    # The participant's root entity instance id, folded into the coordinator's
    # global instance-id uniqueness check.
    rootInstanceId @1 :Text;
}

# The whole payload of `participant_release`, which names a launch and nothing
# else: the verb is the service name, not a field.
#
# It releases a reservation the coordinator obtained but no longer needs, either
# because a later participant refused or because the launch finished.
# Idempotent: releasing a reservation that is not held succeeds; only a
# reservation held for a DIFFERENT launch is refused, because the caller has no
# standing to release that one.
struct LaunchScopedRequest {
    launchId @0 :Text;
}

# Tells a reserved participant to replace its stack slice: tear down whatever it
# is running, clear it, and record that the slice now belongs to this launch.
#
# Separate from the reservation on purpose: reserving is NON-DESTRUCTIVE and
# happens before the coordinator knows whether every participant will accept, so
# folding the teardown into it would replace a stack on machine A for a launch
# that machine B is about to refuse. This is the commit point, sent only once
# every participant is reserved. The launch id must match the reservation the
# participant holds, which is what stops a stale coordinator from wiping a
# machine out from under the launch that owns it.
struct ParticipantSliceBeginRequest {
    launchId @0 :Text;
    # Host paths the container instances placed on this participant bind, each
    # resolved by the coordinator against that instance's arguments: only the
    # coordinator holds the whole plan, and a participant is handed one instance
    # at a time.
    #
    # The participant creates the missing ones and registers them with its
    # container runtime while its slice is empty, which is the only moment that
    # is free: registering a host path can restart the runtime VM, and paying
    # that once a container of this launch is running would kill it.
    mountSources @1 :List(Text);
    # `true` adds to a slice of the same launch the participant already holds,
    # as a join does; `false` replaces whatever slice it holds.
    append @2 :Bool;
    lifecycleWatchers @3 :List(InstanceWatchers);
}

# The machines watching one local source instance: the complete set, sent
# once per slice change.
struct InstanceWatchers {
    instanceId @0 :Text;
    coreNodes @1 :List(Text);
}

# The reply of a participant that began its slice: what it created on the
# way. A participant that refuses gives an error reply, with a failure kind
# and a message.
struct ParticipantSliceBeginResponse {
    # The entries of `mountSources` that did not exist and were created here.
    # The coordinator relays them into the launch output attributed to this
    # machine: an auto-created bind source is the only warning an operator gets
    # that a bind meant to name an existing file was a typo, and it must not go
    # silent because the machine it happened on is not the one being watched.
    autoCreatedMountSources @0 :List(Text);
}

# Remove only these instances from a reserved launch's existing slice.
# Missing instances are already removed, so cleanup can be retried safely.
struct ParticipantInstancesRemoveRequest {
    launchId @0 :Text;
    instanceIds @1 :List(Text);
}

# Replace set slots of instances this machine runs in a reserved launch's slice.
# Each entry carries one slot's whole member set, after a join grew it or a
# removal shrank it; the receiver delivers it to its own instance.
struct ParticipantSetsUpdateRequest {
    launchId @0 :Text;
    sets @1 :List(SlotSet);
}

# One instance's slot and the set it holds from now on.
struct SlotSet {
    instanceId @0 :Text;
    linkId @1 :Text;
    members :union {
        # Discriminant zero: a message that never set `members` decodes as
        # `unset`, which the decoder refuses.
        unset @2 :Void;
        # A producer-binding slot's members, in plan order.
        producers @3 :List(BoundMember);
        # An observer slot's observed pairings, in plan order.
        observed @4 :List(ObservationMember);
    }
}

# One member of a producer-binding slot: the producer instance and the copy
# its instance belongs to, empty for an instance run outside a copy.
struct BoundMember {
    producer @0 :InstanceAddress;
    copy @1 :Text;
    instanceIdInCopy @2 :Text;
    # The id the copy's fragment wrote for the instance, which the copy runs
    # as `<copy>_<instanceIdInCopy>`. Set exactly when `copy` is; the receiver
    # refuses a set that names one without the other.
}

# One pairing an observer slot taps: the source instance and its pairing slot.
# Identical to `node.capnp:ObservationMember`, restated because each schema file
# is compiled on its own.
struct ObservationMember {
    source @0 :InstanceAddress;
    sourceLinkId @1 :Text;
    # The pair's other end when the member is pinned to one pair. Absent, the
    # member observes every pair of the source's slot.
    peer @2 :ObservedPeer;
}

# One pair's other end, as an observed member names it. Identical to
# `node.capnp:ObservedPeer`, restated because each schema file is compiled on
# its own.
struct ObservedPeer {
    instance @0 :InstanceAddress;
    linkId @1 :Text;
}

# The reply of a federation exchange that the receiver did. Shared by
# `pair_commit`, `participant_release`, `participant_instances_remove` and
# `participant_sets_update`: delivery of the reply is the acknowledgement,
# and the verb is the service name.
#
# A receiver that does not do it gives an error reply, with a failure kind
# and a message, and the refusal is load-bearing, not decoration. A
# `pair_commit` refusal makes the sender revert its own half, so a pair is
# never left established on one machine and absent on the other; its kind
# `resource_unavailable` says the receiver's slot is starting a restart or
# has no process to pair, or its own node did not take the delivery, so the
# pair may be asked for again later. A `participant_release` refusal tells a
# coordinator its reservation was taken over rather than merely absent.
struct FederationAck {
}

# One instance, addressed by the `(coreNode, instanceId)` pair. Identical to
# `node.capnp:InstanceAddress` and decoding to the same `ProducerRef`; it is
# restated here only because each schema file is compiled on its own, so this
# file cannot import that one.
struct InstanceAddress {
    coreNode @0 :Text;
    instanceId @1 :Text;
}

# Asks a peer daemon to record its half of a cross-daemon pair and deliver the
# pin to its own endpoint.
#
# Pairing is symmetric, but a registry is not shared: each daemon records the
# pairs its own instances are in, and only a daemon can deliver a pin to a node
# it hosts. A same-daemon pair is committed and delivered in one place; a
# cross-daemon one needs the second half committed where the second node lives,
# and this is that request.
#
# Sent by the daemon starting the LATER endpoint, which is the side that has
# just validated the pair. The receiving side does not re-derive the pairing
# rules: it cannot read the sender's manifests, and the launch coordinator
# already checked both against each other before anything started.
struct PairCommitRequest {
    pairingName @0 :Text;
    pairingTag @1 :Text;
    # The endpoint that lives on the RECEIVING daemon. Its `coreNode` is the
    # receiver's own name, and the receiver REFUSES the commit if it is not:
    # the rule this whole schema is built on is that a daemon never infers
    # placement from an absent field, and "local means whoever opened the
    # envelope" is exactly that inference. Stating it also makes a request
    # mis-routed to the wrong machine fail loudly instead of pairing a
    # same-named instance that happens to live there.
    local @2 :InstanceAddress;
    localLinkId @3 :Text;
    localRole @4 :Text;
    # The endpoint on the SENDING daemon, which the receiver pins its node to.
    peer @5 :InstanceAddress;
    peerLinkId @6 :Text;
    peerRole @7 :Text;
    # The declared cardinality of the peer's slot, one of the four spellings
    # a manifest writes: the receiver holds the far half of every pair that
    # slot takes part in, and only a scalar slot is taken by one pair.
    peerCardinality @8 :Text;
    # The copy the peer's instance belongs to; empty for an instance run
    # outside a copy.
    peerCopy @9 :Text;
    # The pair a restarted peer held before its restart and asks for again,
    # which the receiver makes only while its own endpoint still keeps it;
    # false for a pair a start's link names.
    held @10 :Bool;
}

# Best-effort notification from the daemon that owns an instance to a daemon
# holding a relationship with it. Idempotent: the receiver converges on the
# reported state, so a duplicate changes nothing and a lost one leaves the
# receiver stale rather than wrong.
struct RelationshipNotification {
    # The instance whose lifecycle moved, and the daemon that owns it. Two
    # daemons can host same-named instances, so the pair is the identity.
    instance @0 :InstanceAddress;
    event :union {
        # The instance reached Running under a fresh incarnation. Observing
        # daemons advance their incarnation counter for this source and
        # redeliver its pin, which is what makes an observer drop and redeclare
        # its subscription across a source restart.
        reachedRunning @1 :Void;
        # The instance's process exited on its own; its restart policy may
        # start it again. A daemon holding a pair with it dissolves that pair
        # locally and keeps it held, so a restart of either endpoint asks for
        # it again. Dissolution stays authoritative on the daemon that owns
        # the dead instance; this only propagates it.
        #
        # An unreachable daemon cannot send this, which is exactly why a node
        # whose correctness depends on freshness owns a staleness watchdog
        # rather than trusting the framework to notice a partition.
        exited @2 :Void;
        # The instance was stopped on purpose (`peppy node stop`, a removal or
        # a teardown) and does not run again. A daemon holding a pair with it
        # dissolves that pair and forgets it.
        stopped @3 :Void;
    }
}

struct RelationshipNotificationAck {
    # No fields: the notification is best-effort and the receiver converges on
    # whatever it is told, so there is nothing for it to answer. Delivery of a
    # well-formed reply IS the ack, the same contract `health` uses. Kept as a
    # struct so the ack can grow a field without a wire break.
}
