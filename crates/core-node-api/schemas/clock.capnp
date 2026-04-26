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

enum ClockSource {
    wall @0;
    # Reserved for future use; encoders MUST NOT emit these today.
    # sim    @1;
    # replay @2;
}

struct ClockRequest {
    clientSendTime @0 :UInt64;
}

struct ClockResponse {
    clientSendTime @0 :UInt64;
    serverRecvTime @1 :UInt64;
    serverSendTime @2 :UInt64;
    clockSource    @3 :ClockSource;
}

# A one-way snapshot published periodically on the `clock` topic. Subscribers
# treat each tick as "the core node says it's now `time`". Unlike the request/
# response service, no NTP exchange happens here — the value is stale by one
# one-way network delay on read.
struct ClockTick {
    time        @0 :UInt64;
    clockSource @1 :ClockSource;
}
