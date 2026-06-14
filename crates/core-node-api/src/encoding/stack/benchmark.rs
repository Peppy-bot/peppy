//! Encoding types for the `stack_benchmark` action (streaming, with feedback).
//!
//! The benchmark measures the per-interface messaging latency wiring each node
//! to its direct dependencies, against the already-running stack. See
//! `benchmark.capnp` for the wire schema.

use capnp::message::Builder;

use crate::benchmark_capnp;
use crate::{NonEmptyPayload, Payload, Result};

use crate::encoding::{
    capnp_list_len, decode_message, encode_message, encode_message_non_empty, optional_text,
};

/// Default timed samples per interface when the goal sends 0 (Cap'n Proto
/// leaves an unset `UInt32` at 0).
pub const DEFAULT_SAMPLES: u32 = 200;
/// Default per-sample probe/observe timeout in milliseconds when the goal sends 0.
pub const DEFAULT_PER_SAMPLE_TIMEOUT_MS: u64 = 2_000;

fn with_default<T: PartialEq + Copy>(value: T, zero: T, default: T) -> T {
    if value == zero { default } else { value }
}

// ---------------------------------------------------------------------------
// Goal
// ---------------------------------------------------------------------------

/// Goal message for the `stack_benchmark` action.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StackBenchmarkGoal {
    /// Timed samples per interface, after warmup.
    pub samples: u32,
    /// Warmup samples per interface, discarded before measuring (0 = none).
    pub warmup: u32,
    /// Per-sample probe/observe timeout in milliseconds.
    pub per_sample_timeout_ms: u64,
}

impl StackBenchmarkGoal {
    pub fn new(samples: u32, warmup: u32, per_sample_timeout_ms: u64) -> Self {
        Self {
            samples,
            warmup,
            per_sample_timeout_ms,
        }
    }

    pub fn encode(&self) -> Result<Payload> {
        let mut builder = Builder::new_default();
        {
            let mut goal = builder.init_root::<benchmark_capnp::stack_benchmark_goal::Builder>();
            goal.set_samples(self.samples);
            goal.set_warmup(self.warmup);
            goal.set_per_sample_timeout_ms(self.per_sample_timeout_ms);
        }
        encode_message(&builder)
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        let reader = decode_message(data)?;
        let goal = reader.get_root::<benchmark_capnp::stack_benchmark_goal::Reader>()?;
        Ok(Self {
            samples: with_default(goal.get_samples(), 0, DEFAULT_SAMPLES),
            warmup: goal.get_warmup(),
            per_sample_timeout_ms: with_default(
                goal.get_per_sample_timeout_ms(),
                0,
                DEFAULT_PER_SAMPLE_TIMEOUT_MS,
            ),
        })
    }
}

// ---------------------------------------------------------------------------
// Goal response
// ---------------------------------------------------------------------------

/// Response to the benchmark goal request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StackBenchmarkGoalResponse {
    pub accepted: bool,
    pub rejection_reason: Option<String>,
}

impl StackBenchmarkGoalResponse {
    pub fn accepted() -> Self {
        Self {
            accepted: true,
            rejection_reason: None,
        }
    }

    pub fn rejected(reason: impl Into<String>) -> Self {
        Self {
            accepted: false,
            rejection_reason: Some(reason.into()),
        }
    }

    pub fn encode(&self) -> Result<Payload> {
        let mut builder = Builder::new_default();
        {
            let mut response =
                builder.init_root::<benchmark_capnp::stack_benchmark_goal_response::Builder>();
            response.set_accepted(self.accepted);
            if let Some(ref reason) = self.rejection_reason {
                response.set_rejection_reason(reason);
            }
        }
        encode_message(&builder)
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        let reader = decode_message(data)?;
        let response =
            reader.get_root::<benchmark_capnp::stack_benchmark_goal_response::Reader>()?;
        Ok(Self {
            accepted: response.get_accepted(),
            rejection_reason: optional_text(response.get_rejection_reason()?.to_str()?),
        })
    }
}

// ---------------------------------------------------------------------------
// Feedback
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BenchmarkFeedbackStep {
    Enumerating,
    Probing,
    TopicDelivery,
    Aggregating,
}

impl BenchmarkFeedbackStep {
    fn to_capnp(self) -> benchmark_capnp::BenchmarkFeedbackStep {
        use benchmark_capnp::BenchmarkFeedbackStep as W;
        match self {
            Self::Enumerating => W::Enumerating,
            Self::Probing => W::Probing,
            Self::TopicDelivery => W::TopicDelivery,
            Self::Aggregating => W::Aggregating,
        }
    }

    fn from_capnp(value: benchmark_capnp::BenchmarkFeedbackStep) -> Self {
        use benchmark_capnp::BenchmarkFeedbackStep as W;
        match value {
            W::Enumerating => Self::Enumerating,
            W::Probing => Self::Probing,
            W::TopicDelivery => Self::TopicDelivery,
            W::Aggregating => Self::Aggregating,
        }
    }
}

/// One line of progress feedback from the benchmark.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StackBenchmarkFeedback {
    pub stream: String,
    pub line: String,
    pub step: BenchmarkFeedbackStep,
}

impl StackBenchmarkFeedback {
    pub fn stdout(line: impl Into<String>, step: BenchmarkFeedbackStep) -> Self {
        Self {
            stream: "stdout".to_string(),
            line: line.into(),
            step,
        }
    }

    pub fn stderr(line: impl Into<String>, step: BenchmarkFeedbackStep) -> Self {
        Self {
            stream: "stderr".to_string(),
            line: line.into(),
            step,
        }
    }

    pub fn is_stderr(&self) -> bool {
        self.stream == "stderr"
    }

    pub fn encode(&self) -> Result<NonEmptyPayload> {
        let mut builder = Builder::new_default();
        {
            let mut feedback =
                builder.init_root::<benchmark_capnp::stack_benchmark_feedback::Builder>();
            feedback.set_stream(&self.stream);
            feedback.set_line(&self.line);
            feedback.set_step(self.step.to_capnp());
        }
        encode_message_non_empty(&builder)
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        let reader = decode_message(data)?;
        let feedback = reader.get_root::<benchmark_capnp::stack_benchmark_feedback::Reader>()?;
        Ok(Self {
            stream: feedback.get_stream()?.to_str()?.to_owned(),
            line: feedback.get_line()?.to_str()?.to_owned(),
            step: BenchmarkFeedbackStep::from_capnp(feedback.get_step()?),
        })
    }
}

// ---------------------------------------------------------------------------
// Result
// ---------------------------------------------------------------------------

/// The kind of interface a row measures.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InterfaceKind {
    Topic,
    Service,
    Action,
}

impl InterfaceKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Topic => "topic",
            Self::Service => "service",
            Self::Action => "action",
        }
    }

    fn to_capnp(self) -> benchmark_capnp::InterfaceKind {
        use benchmark_capnp::InterfaceKind as W;
        match self {
            Self::Topic => W::Topic,
            Self::Service => W::Service,
            Self::Action => W::Action,
        }
    }

    fn from_capnp(value: benchmark_capnp::InterfaceKind) -> Self {
        use benchmark_capnp::InterfaceKind as W;
        match value {
            W::Topic => Self::Topic,
            W::Service => Self::Service,
            W::Action => Self::Action,
        }
    }
}

/// How a row's latency was measured.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MeasurementKind {
    ServiceProbe,
    ActionProbe,
    TopicDelivery,
    /// Synthetic round-trip for a topic edge: a `Probe` to the producer node's
    /// always-on framework service, reply sized from the topic's message
    /// schema. The real topic is never published and no handler runs.
    NodeProbe,
}

impl MeasurementKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ServiceProbe => "service-probe",
            Self::ActionProbe => "action-probe",
            Self::TopicDelivery => "topic-delivery",
            Self::NodeProbe => "node-probe",
        }
    }

    /// Whether this is a synthetic, handler-free round-trip probe (as opposed
    /// to an observe-only measurement of real traffic). Drives which report
    /// table a row lands in.
    pub fn is_synthetic_probe(self) -> bool {
        match self {
            Self::ServiceProbe | Self::ActionProbe | Self::NodeProbe => true,
            Self::TopicDelivery => false,
        }
    }

    fn to_capnp(self) -> benchmark_capnp::MeasurementKind {
        use benchmark_capnp::MeasurementKind as W;
        match self {
            Self::ServiceProbe => W::ServiceProbe,
            Self::ActionProbe => W::ActionProbe,
            Self::TopicDelivery => W::TopicDelivery,
            Self::NodeProbe => W::NodeProbe,
        }
    }

    fn from_capnp(value: benchmark_capnp::MeasurementKind) -> Self {
        use benchmark_capnp::MeasurementKind as W;
        match value {
            W::ServiceProbe => Self::ServiceProbe,
            W::ActionProbe => Self::ActionProbe,
            W::TopicDelivery => Self::TopicDelivery,
            W::NodeProbe => Self::NodeProbe,
        }
    }
}

/// Confidence in a one-way (topic-delivery) measurement's clock alignment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClockConfidence {
    NotApplicable,
    SameHost,
    CrossHostCorrected,
    CrossHostFlagged,
}

impl ClockConfidence {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NotApplicable => "—",
            Self::SameHost => "same-host",
            Self::CrossHostCorrected => "corrected",
            Self::CrossHostFlagged => "flagged",
        }
    }

    fn to_capnp(self) -> benchmark_capnp::ClockConfidence {
        use benchmark_capnp::ClockConfidence as W;
        match self {
            Self::NotApplicable => W::NotApplicable,
            Self::SameHost => W::SameHost,
            Self::CrossHostCorrected => W::CrossHostCorrected,
            Self::CrossHostFlagged => W::CrossHostFlagged,
        }
    }

    fn from_capnp(value: benchmark_capnp::ClockConfidence) -> Self {
        use benchmark_capnp::ClockConfidence as W;
        match value {
            W::NotApplicable => Self::NotApplicable,
            W::SameHost => Self::SameHost,
            W::CrossHostCorrected => Self::CrossHostCorrected,
            W::CrossHostFlagged => Self::CrossHostFlagged,
        }
    }
}

/// One measured interface edge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InterfaceLatency {
    pub from_node: String,
    pub from_tag: String,
    pub to_node: String,
    pub to_tag: String,
    pub interface_name: String,
    /// The consumer's dependency link this edge was measured through. Two rows
    /// can share producer + interface but differ only by this link.
    pub link_id: String,
    /// `Some("name:tag")` when this edge was resolved through interface
    /// conformance (consumer `depends_on.interfaces`, producer `conforms_to`);
    /// `None` for a direct `depends_on.nodes` edge.
    pub via_interface: Option<String>,
    pub kind: InterfaceKind,
    pub measurement: MeasurementKind,
    pub clock_confidence: ClockConfidence,
    pub p50_ns: u64,
    pub p90_ns: u64,
    pub mean_ns: u64,
    pub count: u64,
    pub samples_ns: Vec<u64>,
    pub note: Option<String>,
}

impl InterfaceLatency {
    /// A short `from:tag {arrow} to:tag/interface` label for rendering. The
    /// arrow distinguishes the dependency kind: `➔` for an interface-conformance
    /// edge (matching `stack list`), `→` for a direct node dependency.
    pub fn edge_label(&self) -> String {
        let arrow = if self.via_interface.is_some() {
            "➔"
        } else {
            "→"
        };
        format!(
            "{}:{} {arrow} {}:{}/{}",
            self.from_node, self.from_tag, self.to_node, self.to_tag, self.interface_name
        )
    }
}

/// Result message for the `stack_benchmark` action.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StackBenchmarkResult {
    pub success: bool,
    pub error_message: Option<String>,
    pub rows: Vec<InterfaceLatency>,
}

impl StackBenchmarkResult {
    pub fn success(rows: Vec<InterfaceLatency>) -> Self {
        Self {
            success: true,
            error_message: None,
            rows,
        }
    }

    pub fn failure(error_message: impl Into<String>) -> Self {
        Self {
            success: false,
            error_message: Some(error_message.into()),
            rows: Vec::new(),
        }
    }

    pub fn encode(&self) -> Result<Payload> {
        let mut builder = Builder::new_default();
        {
            let mut result =
                builder.init_root::<benchmark_capnp::stack_benchmark_result::Builder>();
            result.set_success(self.success);
            if let Some(ref error_message) = self.error_message {
                result.set_error_message(error_message);
            }

            let row_count = capnp_list_len(self.rows.len(), "StackBenchmarkResult.rows")?;
            let mut rows = result.reborrow().init_rows(row_count);
            for (i, row) in self.rows.iter().enumerate() {
                let mut r = rows.reborrow().get(i as u32);
                r.set_from_node(&row.from_node);
                r.set_from_tag(&row.from_tag);
                r.set_to_node(&row.to_node);
                r.set_to_tag(&row.to_tag);
                r.set_interface_name(&row.interface_name);
                r.set_link_id(&row.link_id);
                if let Some(ref via) = row.via_interface {
                    r.set_via_interface(via);
                }
                r.set_kind(row.kind.to_capnp());
                r.set_measurement(row.measurement.to_capnp());
                r.set_clock_confidence(row.clock_confidence.to_capnp());
                r.set_p50_ns(row.p50_ns);
                r.set_p90_ns(row.p90_ns);
                r.set_mean_ns(row.mean_ns);
                r.set_count(row.count);
                if let Some(ref note) = row.note {
                    r.set_note(note);
                }
                let sample_count =
                    capnp_list_len(row.samples_ns.len(), "InterfaceLatency.samples_ns")?;
                let mut samples = r.reborrow().init_samples_ns(sample_count);
                for (j, &sample) in row.samples_ns.iter().enumerate() {
                    samples.set(j as u32, sample);
                }
            }
        }
        encode_message(&builder)
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        let reader = decode_message(data)?;
        let result = reader.get_root::<benchmark_capnp::stack_benchmark_result::Reader>()?;

        let rows_reader = result.get_rows()?;
        let mut rows = Vec::with_capacity(rows_reader.len() as usize);
        for i in 0..rows_reader.len() {
            let r = rows_reader.get(i);
            let samples_reader = r.get_samples_ns()?;
            let mut samples_ns = Vec::with_capacity(samples_reader.len() as usize);
            for j in 0..samples_reader.len() {
                samples_ns.push(samples_reader.get(j));
            }
            rows.push(InterfaceLatency {
                from_node: r.get_from_node()?.to_str()?.to_owned(),
                from_tag: r.get_from_tag()?.to_str()?.to_owned(),
                to_node: r.get_to_node()?.to_str()?.to_owned(),
                to_tag: r.get_to_tag()?.to_str()?.to_owned(),
                interface_name: r.get_interface_name()?.to_str()?.to_owned(),
                link_id: r.get_link_id()?.to_str()?.to_owned(),
                via_interface: optional_text(r.get_via_interface()?.to_str()?),
                kind: InterfaceKind::from_capnp(r.get_kind()?),
                measurement: MeasurementKind::from_capnp(r.get_measurement()?),
                clock_confidence: ClockConfidence::from_capnp(r.get_clock_confidence()?),
                p50_ns: r.get_p50_ns(),
                p90_ns: r.get_p90_ns(),
                mean_ns: r.get_mean_ns(),
                count: r.get_count(),
                samples_ns,
                note: optional_text(r.get_note()?.to_str()?),
            });
        }

        Ok(Self {
            success: result.get_success(),
            error_message: optional_text(result.get_error_message()?.to_str()?),
            rows,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn goal_roundtrips() {
        let goal = StackBenchmarkGoal::new(500, 50, 1_500);
        let bytes = goal.encode().expect("encode");
        assert_eq!(StackBenchmarkGoal::decode(&bytes).expect("decode"), goal);
    }

    #[test]
    fn goal_applies_defaults_for_zero_fields() {
        let goal = StackBenchmarkGoal::new(0, 0, 0);
        let bytes = goal.encode().expect("encode");
        let decoded = StackBenchmarkGoal::decode(&bytes).expect("decode");
        assert_eq!(decoded.samples, DEFAULT_SAMPLES);
        assert_eq!(decoded.warmup, 0); // 0 warmup is meaningful (no warmup)
        assert_eq!(decoded.per_sample_timeout_ms, DEFAULT_PER_SAMPLE_TIMEOUT_MS);
    }

    #[test]
    fn goal_response_roundtrips() {
        let accepted = StackBenchmarkGoalResponse::accepted();
        let bytes = accepted.encode().expect("encode");
        assert_eq!(
            StackBenchmarkGoalResponse::decode(&bytes).expect("decode"),
            accepted
        );

        let rejected = StackBenchmarkGoalResponse::rejected("already running");
        let bytes = rejected.encode().expect("encode");
        assert_eq!(
            StackBenchmarkGoalResponse::decode(&bytes).expect("decode"),
            rejected
        );
    }

    #[test]
    fn feedback_roundtrips_all_steps() {
        for step in [
            BenchmarkFeedbackStep::Enumerating,
            BenchmarkFeedbackStep::Probing,
            BenchmarkFeedbackStep::TopicDelivery,
            BenchmarkFeedbackStep::Aggregating,
        ] {
            let fb = StackBenchmarkFeedback::stdout("measuring", step);
            let bytes = fb.encode().expect("encode").into_inner();
            assert_eq!(
                StackBenchmarkFeedback::decode(bytes.as_ref()).expect("decode"),
                fb
            );
        }
        let err = StackBenchmarkFeedback::stderr("oops", BenchmarkFeedbackStep::Probing);
        assert!(err.is_stderr());
    }

    fn sample_row() -> InterfaceLatency {
        InterfaceLatency {
            from_node: "planner".to_string(),
            from_tag: "v1".to_string(),
            to_node: "camera".to_string(),
            to_tag: "v2".to_string(),
            interface_name: "frames".to_string(),
            link_id: "prov".to_string(),
            via_interface: Some("camera_iface:v2".to_string()),
            kind: InterfaceKind::Topic,
            measurement: MeasurementKind::TopicDelivery,
            clock_confidence: ClockConfidence::SameHost,
            p50_ns: 1_000,
            p90_ns: 2_000,
            mean_ns: 1_200,
            count: 3,
            samples_ns: vec![900, 1_000, 2_000],
            note: Some("ok".to_string()),
        }
    }

    #[test]
    fn result_roundtrips_with_rows_and_samples() {
        let result = StackBenchmarkResult::success(vec![
            sample_row(),
            InterfaceLatency {
                kind: InterfaceKind::Service,
                measurement: MeasurementKind::ServiceProbe,
                clock_confidence: ClockConfidence::NotApplicable,
                via_interface: None,
                samples_ns: vec![],
                note: None,
                ..sample_row()
            },
            InterfaceLatency {
                measurement: MeasurementKind::NodeProbe,
                clock_confidence: ClockConfidence::NotApplicable,
                ..sample_row()
            },
        ]);
        let bytes = result.encode().expect("encode");
        assert_eq!(
            StackBenchmarkResult::decode(&bytes).expect("decode"),
            result
        );
    }

    #[test]
    fn synthetic_probe_classification_drives_table_split() {
        assert!(MeasurementKind::ServiceProbe.is_synthetic_probe());
        assert!(MeasurementKind::ActionProbe.is_synthetic_probe());
        assert!(MeasurementKind::NodeProbe.is_synthetic_probe());
        assert!(!MeasurementKind::TopicDelivery.is_synthetic_probe());
    }

    #[test]
    fn result_empty_roundtrips() {
        let result = StackBenchmarkResult::success(vec![]);
        let bytes = result.encode().expect("encode");
        let decoded = StackBenchmarkResult::decode(&bytes).expect("decode");
        assert!(decoded.success);
        assert!(decoded.rows.is_empty());
    }

    #[test]
    fn result_failure_roundtrips() {
        let result = StackBenchmarkResult::failure("no running stack");
        let bytes = result.encode().expect("encode");
        let decoded = StackBenchmarkResult::decode(&bytes).expect("decode");
        assert!(!decoded.success);
        assert_eq!(decoded.error_message.as_deref(), Some("no running stack"));
    }

    #[test]
    fn result_decode_rejects_malformed() {
        assert!(StackBenchmarkResult::decode(b"not capnp").is_err());
    }
}
