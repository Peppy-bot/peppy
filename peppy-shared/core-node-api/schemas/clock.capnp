@0xa8f5c2b9e4d3f1ab;

# Clock-synchronization service messages.
#
# Timestamps are u64 nanoseconds since the Unix epoch. Clients perform an
# NTP-style 4-timestamp exchange:
#   t0 = client_send_time   (stamped before send)
#   t1 = server_recv_time   (stamped on the server when the request arrives)
#   t2 = server_send_time   (stamped on the server just before reply)
#   t3 = client_recv_time   (stamped by the client on receive — never on the wire)
#
# offset = ((t1 - t0) + (t2 - t3)) / 2
# delay  = (t3 - t0) - (t2 - t1)

struct ClockRequest {
    clientSendTime @0 :UInt64;
}

struct ClockResponse {
    clientSendTime @0 :UInt64;
    serverRecvTime @1 :UInt64;
    serverSendTime @2 :UInt64;
}

# A one-way snapshot published periodically on the `clock` topic. Subscribers
# treat each tick as "the core node says it's now `time`". Unlike the request/
# response service, no NTP exchange happens here — the value is stale by one
# one-way network delay on read.
struct ClockTick {
    time @0 :UInt64;
}

# Request to a node's `clock_offset` service. The node, on receipt, performs a
# ClockRequest/ClockResponse exchange against the core node and reports the
# result. The request itself is empty.
struct ClockOffsetRequest {
}

# A node's measured clock offset relative to the core node, from an NTP-style
# exchange. `offsetNs` is signed: `node_local + offsetNs ≈ core_node_time`.
# `roundTripDelayNs` is the measured RTT, used to bound the offset's accuracy
# and to self-diagnose (a large delay means a low-confidence offset).
struct ClockOffsetResponse {
    offsetNs @0 :Int64;
    roundTripDelayNs @1 :UInt64;
    # The clock domain the reporting instance reads, as `name@coreNode`, or
    # empty for wall time. An offset is only meaningful against the timeline it
    # was measured on, and an instance reading a simulated domain reports zero:
    # every peer it may connect to reads that same domain, so their instants
    # are already comparable.
    domain @2 :Text;
}

# One clock domain a daemon hosts: a name, the machine its publisher runs on,
# and the lifetime it was minted for. Two domains are the same timeline only
# when all three agree, which is why the incarnation travels.
struct ClockDomainInfo {
    name @0 :Text;
    coreNode @1 :Text;
    incarnation @2 :UInt64;
    # The instance supplying this domain, on `coreNode`.
    publisherInstanceId @3 :Text;
    # The launch that started the publisher, and the daemon that drove it.
    # They travel as a pair: both carry a value, or both are empty, which is
    # how a publisher started by `peppy node run` reports having no launch.
    launchId @4 :Text;
    coordinatorCoreNode @5 :Text;
    # Whether the domain has published an instant yet. A domain whose
    # publisher stopped keeps its last one and reports `false` here only if it
    # never published at all.
    ready @6 :Bool;
    # The last instant this daemon saw on the domain, or 0 for none.
    lastTickNs @7 :UInt64;
}

# One instance on the answering daemon that reads a domain, wherever that
# domain is hosted.
struct ClockConsumerInfo {
    instanceId @0 :Text;
    domainName @1 :Text;
    domainCoreNode @2 :Text;
    incarnation @3 :UInt64;
}

struct ClockListRequest {
}

struct ClockListResponse {
    # The domains whose publisher runs on the answering daemon.
    domains @0 :List(ClockDomainInfo);
    # The instances on the answering daemon that read a domain, including
    # domains hosted elsewhere.
    consumers @1 :List(ClockConsumerInfo);
}
