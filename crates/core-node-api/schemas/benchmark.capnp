@0xbe27c4a91f3d6e58;

# `stack_benchmark` action: measures the per-interface messaging latency that
# wires each node to its direct dependencies, against the already-running stack.
# See docs/src/content/docs/advanced_guides/stack_benchmark.mdx.

struct StackBenchmarkGoal {
    # Timed samples per interface, after warmup. 0 = daemon default.
    samples @0 :UInt32;
    # Warmup samples per interface, discarded before measuring (0 = none).
    warmup @1 :UInt32;
    # Whether to also run the clock-independent synthetic topic transport baseline.
    includeSyntheticBaseline @2 :Bool;
    # Per-sample probe/observe timeout in milliseconds. 0 = daemon default.
    perSampleTimeoutMs @3 :UInt64;
}

struct StackBenchmarkGoalResponse {
    # Whether the benchmark goal was accepted.
    accepted @0 :Bool;
    # Rejection reason if not accepted (empty if accepted).
    rejectionReason @1 :Text;
}

enum BenchmarkFeedbackStep {
    enumerating @0;
    probing @1;
    topicDelivery @2;
    synthetic @3;
    aggregating @4;
}

struct StackBenchmarkFeedback {
    # The stream type: "stdout" or "stderr".
    stream @0 :Text;
    # The line of progress output.
    line @1 :Text;
    # The phase this feedback is from.
    step @2 :BenchmarkFeedbackStep;
}

enum InterfaceKind {
    topic @0;
    service @1;
    action @2;
}

enum MeasurementKind {
    # Round-trip Probe to a service (messaging path, excludes the handler).
    serviceProbe @0;
    # Round-trip Probe to an action's goal service (no goal is created).
    actionProbe @1;
    # Real producer -> consumer one-way delivery latency on live traffic.
    topicDelivery @2;
    # Synthetic transport-path proxy on a reserved benchmark-only key.
    topicSynthetic @3;
}

enum ClockConfidence {
    # Not a one-way measurement (round-trip probe or synthetic baseline).
    notApplicable @0;
    # Producer shares the core node's host; one-way latency is exact.
    sameHost @1;
    # Cross-host, corrected with the producer's measured clock offset.
    crossHostCorrected @2;
    # Cross-host, but the corrected delta was implausible and was suppressed.
    crossHostFlagged @3;
}

struct InterfaceLatency {
    fromNode @0 :Text;
    fromTag @1 :Text;
    toNode @2 :Text;
    toTag @3 :Text;
    interfaceName @4 :Text;
    kind @5 :InterfaceKind;
    measurement @6 :MeasurementKind;
    clockConfidence @7 :ClockConfidence;
    # Aggregated statistics over the timed samples, in nanoseconds.
    p50Ns @8 :UInt64;
    p90Ns @9 :UInt64;
    meanNs @10 :UInt64;
    count @11 :UInt64;
    # Raw per-sample nanosecond timings (for the renderer / baseline).
    samplesNs @12 :List(UInt64);
    # Optional human-readable note (e.g. why a cross-host delta was flagged).
    note @13 :Text;
}

struct StackBenchmarkResult {
    # Whether the benchmark completed (per-interface failures are encoded as rows).
    success @0 :Bool;
    # Error message if the benchmark itself failed (empty on success).
    errorMessage @1 :Text;
    # One row per measured interface.
    rows @2 :List(InterfaceLatency);
}
