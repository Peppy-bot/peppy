//! `peppylib.codec`: the codec API of peppy in Python.
//!
//! The parser of contract documents ([`PyContract`]), the codec of a message
//! format between a `dict` of canonical JSON values and the wire bytes
//! ([`PyMessageCodec`]), and the clients of any service, action and topic of
//! a contract, addressed to one producer ([`PyServiceClient`],
//! [`PyActionClient`], [`PyTopicConsumer`]). Each is a thin wrapper of the
//! `peppy-codec` crate, so a Python node encodes the bytes that a generated
//! node encodes.
//!
//! This build of peppylib embeds no Cap'n Proto compiler: the module
//! `peppylib.codec` hands the codec the compiler that peppy deploys beside it
//! for a node whose manifest asks for the codec API
//! ([`carry_compiler_from_file`]), and `CAPNP_BINARY_PATH` names another.

use config::failure_kind::MemberSection;
use config::node::{InterfaceKind, MessageFormat, SchemaType};
use peppy_codec::consumer::{
    ActionClient, ConsumerError, ConsumerIdentity, GoalHandle, GoalOutcome, GoalRequests,
    MemberBinding, MemberError, ServiceClient, TopicConsumer, contract_topic_codec,
};
use peppy_codec::contract::{MessageSide, PeppyContract, PeppyContractParser};
use peppy_codec::{CodecError, ConversionError, MessageCodec};
use peppylib::messaging::{CancelState, DEFAULT_FEEDBACK_BOUND, FeedbackBound};
use pyo3::exceptions::{PyKeyError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::PyBytes;
use serde_json::Value;
use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;
use tokio::sync::Mutex;

use crate::messaging::{
    PyMessengerHandle, PyProducerRef, duration_from_secs_f64, optional_duration_from_secs_f64,
    to_py_err,
};

mod exceptions {
    pyo3::create_exception!(
        peppylib,
        ContractError,
        pyo3::exceptions::PyValueError,
        "A text is not a contract document: the message names the key that \
         the parser refused."
    );

    pyo3::create_exception!(
        peppylib,
        CodecError,
        pyo3::exceptions::PyRuntimeError,
        "A message format does not lay out on the wire, or the process has no \
         Cap'n Proto compiler."
    );

    pyo3::create_exception!(
        peppylib,
        ConversionError,
        pyo3::exceptions::PyValueError,
        "A value does not convert between its canonical JSON form and the \
         wire: the message names the field."
    );

    pyo3::create_exception!(
        peppylib,
        UndeclaredFailureKindError,
        pyo3::exceptions::PyRuntimeError,
        "A provider gave a failure kind that is neither a kind of peppy nor a \
         kind of its contract that the member lists."
    );
}

use exceptions::{ContractError, UndeclaredFailureKindError};

fn codec_err(error: CodecError) -> PyErr {
    exceptions::CodecError::new_err(error.to_string())
}

fn conversion_err(error: ConversionError) -> PyErr {
    exceptions::ConversionError::new_err(error.to_string())
}

fn member_err(error: MemberError) -> PyErr {
    match error {
        MemberError::Undeclared { .. } | MemberError::Target { .. } => {
            PyValueError::new_err(error.to_string())
        }
        MemberError::Codec(error) => codec_err(error),
    }
}

fn consumer_err(error: ConsumerError) -> PyErr {
    match error {
        ConsumerError::Messaging(error) => to_py_err(error),
        ConsumerError::Conversion(error) => conversion_err(error),
        ConsumerError::UndeclaredFailureKind { .. } => {
            UndeclaredFailureKindError::new_err(error.to_string())
        }
    }
}

/// The kind named `kind`: `topic`, `service` or `action`.
fn interface_kind(kind: &str) -> PyResult<InterfaceKind> {
    InterfaceKind::from_str(kind).map_err(|_| {
        PyValueError::new_err(format!(
            "unknown member kind {kind:?}: a member is a `topic`, a `service` or an `action`"
        ))
    })
}

/// The message side named `side`.
fn message_side(side: &str) -> PyResult<MessageSide> {
    MessageSide::parse(side).ok_or_else(|| {
        let names: Vec<&str> = MessageSide::ALL.iter().map(|side| side.as_str()).collect();
        PyValueError::new_err(format!(
            "unknown message side {side:?}: a side is one of {}",
            names.join(", ")
        ))
    })
}

/// A `dict` of canonical JSON values, as JSON.
fn json_of(value: &Bound<'_, PyAny>) -> PyResult<Value> {
    Ok(pythonize::depythonize(value)?)
}

/// A JSON value as the Python value it maps to.
fn py_of<'py>(py: Python<'py>, value: &Value) -> PyResult<Bound<'py, PyAny>> {
    Ok(pythonize::pythonize(py, value)?)
}

/// A pinned contract document, parsed: its description, the descriptions
/// and the message formats of its members with the enums, the marks and
/// the descriptions of their fields, the marks of its members and its
/// failure kinds.
///
/// A member is named by its kind (`"topic"`, `"service"` or `"action"`) and
/// its name, a message side by its name (`"message"` of a topic,
/// `"request"` and `"response"` of a service, `"goal"`, `"goal_response"`,
/// `"feedback"` and `"result"` of an action), and a field by its path in
/// the format: the key of each object it sits in, with the items of an
/// array passed through.
#[pyclass(name = "Contract", frozen)]
pub struct PyContract {
    inner: Arc<PeppyContract>,
}

impl PyContract {
    fn field(&self, kind: &str, name: &str, side: &str, path: &[String]) -> PyResult<&SchemaType> {
        let keys: Vec<&str> = path.iter().map(String::as_str).collect();
        self.inner
            .field(interface_kind(kind)?, name, message_side(side)?, &keys)
            .ok_or_else(|| {
                PyKeyError::new_err(format!(
                    "the {kind} `{name}` has no field `{}` in its {side} message",
                    keys.join(".")
                ))
            })
    }
}

#[pymethods]
impl PyContract {
    /// Parses the text of a contract document. Raises `ContractError` for
    /// a text that is not a contract.
    #[staticmethod]
    fn parse(text: &str) -> PyResult<Self> {
        PeppyContractParser::from_content(text)
            .map(|contract| Self {
                inner: Arc::new(contract),
            })
            .map_err(|error| ContractError::new_err(error.to_string()))
    }

    /// Parses the contract document at `path`.
    #[staticmethod]
    fn from_path(path: PathBuf) -> PyResult<Self> {
        PeppyContractParser::from_path(path)
            .map(|contract| Self {
                inner: Arc::new(contract),
            })
            .map_err(|error| ContractError::new_err(error.to_string()))
    }

    #[getter]
    fn name(&self) -> &str {
        self.inner.manifest.name.as_str()
    }

    #[getter]
    fn tag(&self) -> &str {
        &self.inner.manifest.tag
    }

    #[getter]
    fn labels(&self) -> Vec<String> {
        self.inner.manifest.labels.clone().unwrap_or_default()
    }

    /// What the contract is for: its paragraphs, the first one the summary.
    #[getter]
    fn description(&self) -> Vec<&str> {
        let paragraphs = self.inner.description().paragraphs();
        paragraphs
            .iter()
            .map(|paragraph| paragraph.as_str())
            .collect()
    }

    /// The names of the topics, in the order of the document.
    #[getter]
    fn topics(&self) -> Vec<String> {
        let topics = &self.inner.interfaces.topics;
        topics.iter().map(|topic| topic.name.clone()).collect()
    }

    /// The names of the services, in the order of the document.
    #[getter]
    fn services(&self) -> Vec<String> {
        let services = &self.inner.interfaces.services;
        services
            .iter()
            .map(|service| service.name.clone())
            .collect()
    }

    /// The names of the actions, in the order of the document.
    #[getter]
    fn actions(&self) -> Vec<String> {
        let actions = &self.inner.interfaces.actions;
        actions.iter().map(|action| action.name.clone()).collect()
    }

    /// The format of a message side as data, as the document writes it
    /// (`$type`, `$enum`, `$marks` and the other modifiers included), or
    /// `None` for a side that the member declares no format for.
    fn message_format<'py>(
        &self,
        py: Python<'py>,
        kind: &str,
        name: &str,
        side: &str,
    ) -> PyResult<Option<Bound<'py, PyAny>>> {
        let format = self
            .inner
            .message_format(interface_kind(kind)?, name, message_side(side)?);
        format
            .map(|format| py_of(py, &serde_json::to_value(format).expect("a format is JSON")))
            .transpose()
    }

    /// The closed set of strings of a `string` field (`$enum`), or `None`
    /// for a field with no enum. Raises `KeyError` for a field that the
    /// format does not have.
    fn field_enum(
        &self,
        kind: &str,
        name: &str,
        side: &str,
        path: Vec<String>,
    ) -> PyResult<Option<Vec<String>>> {
        let field = self.field(kind, name, side, &path)?;
        Ok(field.enum_values().map(|values| values.values().to_vec()))
    }

    /// The marks of a field (`$marks`), empty for a field with none. Raises
    /// `KeyError` for a field that the format does not have.
    fn field_marks(
        &self,
        kind: &str,
        name: &str,
        side: &str,
        path: Vec<String>,
    ) -> PyResult<Vec<String>> {
        let field = self.field(kind, name, side, &path)?;
        Ok(field.marks().iter().map(|mark| mark.to_string()).collect())
    }

    /// The marks of a member, empty for a member with none.
    fn member_marks(&self, kind: &str, name: &str) -> PyResult<Vec<String>> {
        let marks = self.inner.marks.of(interface_kind(kind)?, name);
        Ok(marks.iter().map(|mark| mark.to_string()).collect())
    }

    /// The description of a member. Raises `KeyError` for a member that the
    /// contract does not declare.
    fn member_description(&self, kind: &str, name: &str) -> PyResult<&str> {
        self.inner
            .member_description(interface_kind(kind)?, name)
            .map(|description| description.as_str())
            .ok_or_else(|| PyKeyError::new_err(format!("the contract has no {kind} `{name}`")))
    }

    /// The description of a field (`$description`), or `None` for a field
    /// with none. Raises `KeyError` for a field that the format does not
    /// have.
    fn field_description(
        &self,
        kind: &str,
        name: &str,
        side: &str,
        path: Vec<String>,
    ) -> PyResult<Option<&str>> {
        let field = self.field(kind, name, side, &path)?;
        Ok(field.description().map(|description| description.as_str()))
    }

    /// The failure kinds of the contract that a service or an action lists,
    /// each with its meaning. The member can give the kinds of peppy too.
    fn failure_kinds(&self, kind: &str, name: &str) -> PyResult<Vec<(String, String)>> {
        let section = match interface_kind(kind)? {
            InterfaceKind::Service => MemberSection::Service,
            InterfaceKind::Action => MemberSection::Action,
            InterfaceKind::Topic => {
                return Err(PyValueError::new_err(
                    "a topic gives no failure kind: only services and actions do",
                ));
            }
        };
        let kinds = self.inner.failure_kinds.of(section, name);
        Ok(kinds
            .contract_kinds()
            .iter()
            .map(|(kind, meaning)| (kind.to_string(), meaning.to_owned()))
            .collect())
    }

    /// The document as data, as it is written, with no comment.
    fn to_dict<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let document = serde_json::to_value(self.inner.as_ref()).expect("a contract is JSON");
        py_of(py, &document)
    }

    fn __repr__(&self) -> String {
        format!(
            "Contract({:?}, {:?})",
            self.inner.manifest.name.as_str(),
            self.inner.manifest.tag
        )
    }
}

/// The codec of one message format: a `dict` of canonical JSON values to
/// the wire bytes of a generated node, and back.
///
/// The canonical values are the JSON values of the exposure format:
/// `time` is an RFC 3339 UTC string with nanoseconds, `bytes` and arrays of
/// `u8` are base64 strings, `u64` and `i64` are decimal strings, and the
/// other types are the matching JSON values. An optional field is left out
/// of the `dict` when it is absent.
#[pyclass(name = "MessageCodec", frozen)]
pub struct PyMessageCodec {
    inner: MessageCodec,
}

#[pymethods]
impl PyMessageCodec {
    /// Lays out `message_format`, a format as data (such as the one
    /// `Contract.message_format` gives), through the Cap'n Proto compiler.
    /// `schema_name` seeds the file id of the schema; it does not appear in
    /// messages. Raises `ValueError` for data that is not a message format
    /// and `CodecError` for a format that does not lay out.
    #[new]
    fn new(py: Python<'_>, schema_name: &str, message_format: &Bound<'_, PyAny>) -> PyResult<Self> {
        let format: MessageFormat = serde_json::from_value(json_of(message_format)?)
            .map_err(|error| PyValueError::new_err(format!("not a message format: {error}")))?;
        let inner = py
            .detach(|| MessageCodec::new(schema_name, &format))
            .map_err(codec_err)?;
        Ok(Self { inner })
    }

    /// Encodes `value`, a `dict` of canonical values, into a framed
    /// message. Raises `ConversionError` naming the field that does not
    /// convert.
    fn encode<'py>(
        &self,
        py: Python<'py>,
        value: &Bound<'_, PyAny>,
    ) -> PyResult<Bound<'py, PyBytes>> {
        let bytes = self
            .inner
            .encode(&json_of(value)?)
            .map_err(conversion_err)?;
        Ok(PyBytes::new(py, &bytes))
    }

    /// Decodes a framed message into a `dict` of canonical values. Raises
    /// `ConversionError` for bytes that are not a message of the format.
    fn decode<'py>(&self, py: Python<'py>, data: &[u8]) -> PyResult<Bound<'py, PyAny>> {
        let value = self.inner.decode(data).map_err(conversion_err)?;
        py_of(py, &value)
    }
}

/// The identity a client calls as.
fn identity(as_core_node: String, as_instance_id: String) -> ConsumerIdentity {
    ConsumerIdentity {
        core_node: as_core_node,
        instance_id: as_instance_id,
    }
}

/// A client of a service of a contract.
#[pyclass(name = "ServiceClient", frozen)]
pub struct PyServiceClient {
    client: ServiceClient,
    binding: MemberBinding,
}

#[pymethods]
impl PyServiceClient {
    /// The client of the service `name` of `contract`, its codecs laid out
    /// from the formats of the contract. Raises `ValueError` for a service
    /// that the contract does not declare.
    #[new]
    fn new(py: Python<'_>, contract: &PyContract, name: &str) -> PyResult<Self> {
        py.detach(|| {
            let client = ServiceClient::of_contract(&contract.inner, name)?;
            let binding = MemberBinding::of_contract(&contract.inner, name)?;
            Ok(Self { client, binding })
        })
        .map_err(member_err)
    }

    /// Calls the service on `producer`, as the instance `as_instance_id` of
    /// the core node `as_core_node`, and returns the response as a `dict`.
    /// Waits at most `response_timeout_secs` for the response, as long as
    /// it takes with `None`. An error reply raises `FailureError` with its
    /// kind and its message.
    #[pyo3(signature = (messenger, as_core_node, as_instance_id, producer, request, response_timeout_secs=None))]
    #[allow(clippy::too_many_arguments)]
    fn call<'py>(
        &self,
        py: Python<'py>,
        messenger: &PyMessengerHandle,
        as_core_node: String,
        as_instance_id: String,
        producer: PyProducerRef,
        request: &Bound<'_, PyAny>,
        response_timeout_secs: Option<f64>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let deadline =
            optional_duration_from_secs_f64("response_timeout_secs", response_timeout_secs)?;
        let request = json_of(request)?;
        let client = self.client.clone();
        let binding = self.binding.clone();
        let handle = messenger.inner.clone();
        crate::py_future::future_into_py(py, async move {
            let response = client
                .call(
                    &handle,
                    &identity(as_core_node, as_instance_id),
                    &binding,
                    producer.as_inner(),
                    &request,
                    deadline,
                )
                .await
                .map_err(consumer_err)?;
            Ok(JsonValue(response))
        })
    }
}

/// A JSON value that a future gives to Python.
struct JsonValue(Value);

impl<'py> IntoPyObject<'py> for JsonValue {
    type Target = PyAny;
    type Output = Bound<'py, PyAny>;
    type Error = PyErr;

    fn into_pyobject(self, py: Python<'py>) -> PyResult<Self::Output> {
        py_of(py, &self.0)
    }
}

/// A client of an action of a contract.
#[pyclass(name = "ActionClient", frozen)]
pub struct PyActionClient {
    client: ActionClient,
    binding: MemberBinding,
}

#[pymethods]
impl PyActionClient {
    /// The client of the action `name` of `contract`, its codecs laid out
    /// from the formats of the contract. Raises `ValueError` for an action
    /// that the contract does not declare.
    #[new]
    fn new(py: Python<'_>, contract: &PyContract, name: &str) -> PyResult<Self> {
        py.detach(|| {
            let client = ActionClient::of_contract(&contract.inner, name)?;
            let binding = MemberBinding::of_contract(&contract.inner, name)?;
            Ok(Self { client, binding })
        })
        .map_err(member_err)
    }

    /// Whether the action declares a feedback message.
    #[getter]
    fn reports_feedback(&self) -> bool {
        self.client.reports_feedback()
    }

    /// Sends the goal `goal`, a `dict`, to `producer`, as the instance
    /// `as_instance_id` of the core node `as_core_node`, and waits at most
    /// `goal_timeout_secs` for the admission of the producer. The handle
    /// keeps up to `feedback_bound` unread feedback messages (`None` takes
    /// the default bound of peppylib): a caller that reads the feedback and
    /// falls behind by more loses the stream, the handle cancels the goal,
    /// and the next read raises `FeedbackOverflowError`.
    #[pyo3(signature = (messenger, as_core_node, as_instance_id, producer, goal, feedback_bound=None, goal_timeout_secs=2.0))]
    #[allow(clippy::too_many_arguments)]
    fn send_goal<'py>(
        &self,
        py: Python<'py>,
        messenger: &PyMessengerHandle,
        as_core_node: String,
        as_instance_id: String,
        producer: PyProducerRef,
        goal: &Bound<'_, PyAny>,
        feedback_bound: Option<usize>,
        goal_timeout_secs: f64,
    ) -> PyResult<Bound<'py, PyAny>> {
        let deadline = duration_from_secs_f64("goal_timeout_secs", goal_timeout_secs)?;
        let feedback_bound = match feedback_bound {
            None => DEFAULT_FEEDBACK_BOUND,
            Some(bound) => FeedbackBound::new(NonZeroUsize::new(bound).ok_or_else(|| {
                PyValueError::new_err("feedback_bound must be one message or more")
            })?),
        };
        let goal = json_of(goal)?;
        let client = self.client.clone();
        let binding = self.binding.clone();
        let handle = messenger.inner.clone();
        crate::py_future::future_into_py(py, async move {
            let goal_handle = client
                .fire_goal(
                    &handle,
                    &identity(as_core_node, as_instance_id),
                    &binding,
                    producer.as_inner(),
                    &goal,
                    feedback_bound,
                    deadline,
                )
                .await
                .map_err(consumer_err)?;
            Ok(PyGoalHandle::new(goal_handle))
        })
    }
}

/// A goal in flight, sent by `ActionClient.send_goal`.
#[pyclass(name = "GoalHandle", frozen)]
pub struct PyGoalHandle {
    /// The handle, for the feedback, which only one reader takes at a time.
    feedback: Arc<Mutex<GoalHandle>>,
    /// The cancel and the result of the goal, which a caller sends while
    /// another task waits for feedback.
    requests: GoalRequests,
    accepted: bool,
    refusal: Option<(String, String)>,
    goal_id: String,
}

impl PyGoalHandle {
    fn new(handle: GoalHandle) -> Self {
        Self {
            requests: handle.requests(),
            accepted: handle.accepted(),
            refusal: handle
                .refusal()
                .map(|failure| (failure.kind().to_string(), failure.message().to_owned())),
            goal_id: handle.goal_id().to_owned(),
            feedback: Arc::new(Mutex::new(handle)),
        }
    }
}

#[pymethods]
impl PyGoalHandle {
    /// Whether the producer accepted the goal.
    #[getter]
    fn accepted(&self) -> bool {
        self.accepted
    }

    /// The failure kind and the message the producer refused the goal
    /// with, or `None` for an accepted goal.
    #[getter]
    fn refusal(&self) -> Option<(String, String)> {
        self.refusal.clone()
    }

    #[getter]
    fn goal_id(&self) -> &str {
        &self.goal_id
    }

    /// The next feedback message as a `dict`, or `None` once the producer
    /// closed the stream because the goal ended. Feedback of an action with
    /// no feedback format reads as an empty `dict`.
    fn next_feedback<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let handle = Arc::clone(&self.feedback);
        crate::py_future::future_into_py(py, async move {
            let feedback = handle
                .lock()
                .await
                .next_feedback()
                .await
                .map_err(consumer_err)?;
            Ok(feedback.map(JsonValue))
        })
    }

    /// Requests how the goal ended, waiting at most `result_timeout_secs`.
    fn result<'py>(
        &self,
        py: Python<'py>,
        messenger: &PyMessengerHandle,
        result_timeout_secs: f64,
    ) -> PyResult<Bound<'py, PyAny>> {
        let deadline = duration_from_secs_f64("result_timeout_secs", result_timeout_secs)?;
        let requests = self.requests.clone();
        let handle = messenger.inner.clone();
        crate::py_future::future_into_py(py, async move {
            let outcome = requests
                .result(&handle, deadline)
                .await
                .map_err(consumer_err)?;
            Ok(PyGoalOutcome::from(outcome))
        })
    }

    /// Asks the producer to cancel the goal, waiting at most
    /// `cancel_timeout_secs` for its acknowledgement. Returns what the
    /// producer acknowledges: `"signalled"` (the goal runs and received the
    /// cancel signal), `"already_terminal"` (the goal had ended) or
    /// `"unknown"` (the producer knows no such goal).
    fn cancel<'py>(
        &self,
        py: Python<'py>,
        messenger: &PyMessengerHandle,
        cancel_timeout_secs: f64,
    ) -> PyResult<Bound<'py, PyAny>> {
        let deadline = duration_from_secs_f64("cancel_timeout_secs", cancel_timeout_secs)?;
        let requests = self.requests.clone();
        let handle = messenger.inner.clone();
        crate::py_future::future_into_py(py, async move {
            let state = requests
                .cancel(&handle, deadline)
                .await
                .map_err(consumer_err)?;
            Ok(cancel_state_name(state))
        })
    }
}

/// The name of a cancel state, as `GoalHandle.cancel` gives it.
fn cancel_state_name(state: CancelState) -> &'static str {
    match state {
        CancelState::Signalled => "signalled",
        CancelState::AlreadyTerminal => "already_terminal",
        CancelState::Unknown => "unknown",
    }
}

/// How a goal ended.
///
/// `status` is `"completed"`, `"cancelled"`, `"failed"`, `"abandoned"` (the
/// producer dropped the goal) or `"expired"` (the producer no longer keeps
/// its result). `result` is the result of a completed or a cancelled goal as
/// a `dict`, `reason` the reason of a cancel, and `failure_kind` and
/// `failure_message` the failure of a failed goal.
#[pyclass(name = "GoalOutcome", frozen)]
pub struct PyGoalOutcome {
    outcome: GoalOutcome,
}

impl From<GoalOutcome> for PyGoalOutcome {
    fn from(outcome: GoalOutcome) -> Self {
        Self { outcome }
    }
}

#[pymethods]
impl PyGoalOutcome {
    #[getter]
    fn status(&self) -> &'static str {
        match &self.outcome {
            GoalOutcome::Completed(_) => "completed",
            GoalOutcome::Cancelled { .. } => "cancelled",
            GoalOutcome::Failed(_) => "failed",
            GoalOutcome::Abandoned => "abandoned",
            GoalOutcome::Expired => "expired",
        }
    }

    #[getter]
    fn result<'py>(&self, py: Python<'py>) -> PyResult<Option<Bound<'py, PyAny>>> {
        match &self.outcome {
            GoalOutcome::Completed(result) | GoalOutcome::Cancelled { result, .. } => {
                py_of(py, result).map(Some)
            }
            GoalOutcome::Failed(_) | GoalOutcome::Abandoned | GoalOutcome::Expired => Ok(None),
        }
    }

    #[getter]
    fn reason(&self) -> Option<&str> {
        match &self.outcome {
            GoalOutcome::Cancelled { reason, .. } => Some(reason),
            _ => None,
        }
    }

    #[getter]
    fn failure_kind(&self) -> Option<&str> {
        match &self.outcome {
            GoalOutcome::Failed(failure) => Some(failure.kind().as_str()),
            _ => None,
        }
    }

    #[getter]
    fn failure_message(&self) -> Option<&str> {
        match &self.outcome {
            GoalOutcome::Failed(failure) => Some(failure.message()),
            _ => None,
        }
    }

    fn __repr__(&self) -> String {
        format!("GoalOutcome(status={:?})", self.status())
    }
}

/// A subscription to a topic of a contract on one producer.
#[pyclass(name = "TopicConsumer", frozen)]
pub struct PyTopicConsumer {
    inner: Arc<Mutex<TopicConsumer>>,
}

#[pymethods]
impl PyTopicConsumer {
    /// Subscribes, as the instance `as_instance_id` of the core node
    /// `as_core_node`, to the topic `name` of `contract` on `producer`, with
    /// the QoS and the retention that the contract declares. The
    /// subscription keeps the newest unread messages: a reader slower than
    /// the producer loses the oldest unread ones. Raises `ValueError` for a
    /// topic that the contract does not declare.
    #[staticmethod]
    fn subscribe<'py>(
        py: Python<'py>,
        messenger: &PyMessengerHandle,
        as_core_node: String,
        as_instance_id: String,
        contract: &PyContract,
        name: &str,
        producer: PyProducerRef,
    ) -> PyResult<Bound<'py, PyAny>> {
        let (codec, binding) = py
            .detach(|| {
                let codec = contract_topic_codec(&contract.inner, name)?;
                let binding = MemberBinding::of_contract(&contract.inner, name)?;
                Ok::<_, MemberError>((codec, binding))
            })
            .map_err(member_err)?;
        let topic = contract
            .inner
            .topic(name)
            .expect("the codec of the topic was laid out");
        let (qos, retention) = (topic.qos_profile.clone(), topic.retention);
        let handle = messenger.inner.clone();
        crate::py_future::future_into_py(py, async move {
            let consumer = TopicConsumer::of_producer(
                &handle,
                &identity(as_core_node, as_instance_id),
                &binding,
                producer.as_inner(),
                qos,
                retention,
                codec,
            )
            .await
            .map_err(to_py_err)?;
            Ok(PyTopicConsumer {
                inner: Arc::new(Mutex::new(consumer)),
            })
        })
    }

    /// The next message as `(producer, dict)`, or `None` once the
    /// subscription shut down.
    fn next_message<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let inner = Arc::clone(&self.inner);
        crate::py_future::future_into_py(py, async move {
            let mut consumer = inner.lock().await;
            let Some((producer, message)) = consumer.next_message().await else {
                return Ok(None);
            };
            let value = consumer.decode(&message).map_err(conversion_err)?;
            Ok(Some((PyProducerRef::from(producer), JsonValue(value))))
        })
    }
}

/// Names the file that holds the Cap'n Proto compiler of the process: the
/// codec reads it and writes it where it runs it. `peppylib.codec` calls it
/// with the compiler that peppy deploys beside it. The first call wins.
#[pyfunction]
fn carry_compiler_from_file(path: PathBuf) {
    peppy_codec::compiler::carry_from_file(path);
}

/// Register the codec submodule.
pub(crate) fn register(parent_module: &Bound<'_, PyModule>) -> PyResult<()> {
    let py = parent_module.py();
    let codec_module = PyModule::new(py, "codec")?;
    codec_module.add("ContractError", py.get_type::<ContractError>())?;
    codec_module.add("CodecError", py.get_type::<exceptions::CodecError>())?;
    codec_module.add(
        "ConversionError",
        py.get_type::<exceptions::ConversionError>(),
    )?;
    codec_module.add(
        "UndeclaredFailureKindError",
        py.get_type::<UndeclaredFailureKindError>(),
    )?;
    codec_module.add_class::<PyContract>()?;
    codec_module.add_class::<PyMessageCodec>()?;
    codec_module.add_class::<PyServiceClient>()?;
    codec_module.add_class::<PyActionClient>()?;
    codec_module.add_class::<PyGoalHandle>()?;
    codec_module.add_class::<PyGoalOutcome>()?;
    codec_module.add_class::<PyTopicConsumer>()?;
    codec_module.add_function(wrap_pyfunction!(carry_compiler_from_file, &codec_module)?)?;
    parent_module.add_submodule(&codec_module)?;
    Ok(())
}
