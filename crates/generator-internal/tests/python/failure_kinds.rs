//! The failure kinds of generated Python members, without a wire: each
//! member's `FailureKind` holds the kinds of peppy, then the kinds of its
//! contract, and its `Failure` takes no other kind and no message that is
//! not a `str`. A failure of a kind that its member does not declare raises
//! `ValueError` before anything reaches the wire, and a client refuses such
//! a kind from the wire the same way.
//!
//! The servers of the probe read their goals from a stand-in of the
//! `peppylib.ConcurrentAction` engine that records each answer, so the probe
//! sees what would reach the wire: a handler shared between two members, a
//! decision that the server cannot send, a handler that raises, and the
//! failures that a service handler raises or returns.
//!
//! The Rust counterpart is the compile failure of
//! `a_native_provider_cannot_give_an_undeclared_kind`.

use crate::helpers::{
    EXPOSED_ACTION_EXAMPLE, failure_kinds, generate_test_peppygen_lib, run_python_check,
};
use config::consts::NODE_CONFIG_FILE;
use config::node::{
    Cardinality, ConsumedService, MessageFormat, NativeExposedAction, NativeExposedService,
    PeppygenLanguage,
};
use generator::{ContractOrigin, DependencyContext, DeploymentInterface};
use std::fs;
use tempfile::TempDir;

const NODE_CONFIG: &str = r#"{
  peppy_schema: "node/v1",
  manifest: {
    name: "arm_driver",
    tag: "v1",
    depends_on: {
      contracts: [
        { name: "lens_contract", tag: "v1", link_id: "lens" }
      ]
    }
  },
  execution: {
    language: "python",
    build_cmd: ["uv", "sync"],
    run_cmd: ["uv", "run", "arm_driver"]
  }
}"#;

const PYTHON_PROBE: &str = r#"
import asyncio
import importlib
import peppylib

native = importlib.import_module("peppygen.exposed_actions.move_arm")
contract = importlib.import_module("peppygen.exposed_actions.arm_impl.move_arm")
lens = importlib.import_module("peppygen.consumed_services.lens.check_lens")
native_park = importlib.import_module("peppygen.exposed_actions.park_arm")
contract_park = importlib.import_module("peppygen.exposed_actions.arm_impl.park_arm")
home = importlib.import_module("peppygen.exposed_actions.home_arm")
clean = importlib.import_module("peppygen.exposed_services.clean_lens")
focus = importlib.import_module("peppygen.exposed_services.get_focus")


def assert_raises(error_type, call, *texts):
    """`call()` raises `error_type`, whose text holds each of `texts`."""
    try:
        call()
    except error_type as error:
        for text in texts:
            assert text in str(error), (text, str(error))
        return
    raise SystemExit(f"expected a {error_type.__name__} with {texts}")


def assert_failure(call, module, kind, message):
    """`call()` raises the `Failure` of `module`, of `kind` and `message`."""
    try:
        call()
    except peppylib.FailureError as failure:
        assert type(failure) is module.Failure, type(failure)
        assert failure.kind is kind and failure.message == message, failure
        return
    raise SystemExit(f"expected the failure {kind}: {message}")


peppy = ["attempt_failed", "timeout", "precondition_failed", "resource_unavailable", "rejected"]
assert [kind.value for kind in native.FailureKind] == peppy, list(native.FailureKind)
assert [kind.value for kind in contract.FailureKind] == peppy + ["arm_reserved", "unreachable"], (
    list(contract.FailureKind)
)
assert [kind.value for kind in lens.FailureKind] == peppy + ["lens_covered"], list(lens.FailureKind)

# A provider refuses with a kind that its member declares...
decision = contract.GoalDecision.refuse(contract.FailureKind.UNREACHABLE, "no plan")
assert decision.refusal == contract.Failure(contract.FailureKind.UNREACHABLE, "no plan")
assert decision.response is None

# ...and a kind that its member does not declare raises before the wire,
# with the message of the provider in its text.
for kind in ("unreachable", "not_a_kind"):
    assert_raises(
        ValueError,
        lambda: native.GoalDecision.refuse(kind, "no plan"),
        f"{kind!r} is not a failure kind of move_arm (message: 'no plan')",
    )

# A failure is a peppylib.FailureError whose args are its kind and message.
failure = lens.Failure(lens.FailureKind.LENS_COVERED, "tape on the lens")
assert isinstance(failure, peppylib.FailureError)
assert failure.args == ("lens_covered", "tape on the lens"), failure.args
assert str(failure) == "lens_covered: tape on the lens", str(failure)

# A client parses the wire form of a failure, and refuses a kind that its
# member does not declare, with the message of the provider in its text.
assert lens.Failure("lens_covered", "tape on the lens") == failure
assert_raises(
    ValueError,
    lambda: lens.Failure("unreachable", "no plan"),
    "'unreachable' is not a failure kind of check_lens (message: 'no plan')",
)

# The message of a failure is a str: any other value raises TypeError where
# the failure is built.
not_a_str = "the message of a failure of move_arm is a str, not TimeoutError"
assert_raises(TypeError, lambda: native.Failure(native.FailureKind.TIMEOUT, TimeoutError("slow")), not_a_str)
assert_raises(TypeError, lambda: native.GoalDecision.refuse(native.FailureKind.TIMEOUT, TimeoutError("slow")), not_a_str)

# A goal decision is an accept with the declared response, or a refusal with
# no response. Any other pair raises where it is built.
both_or_neither = "an accept with the declared GoalResponse, or a refusal with no response"
assert_raises(ValueError, lambda: native_park.GoalDecision(None, None), both_or_neither)
assert_raises(ValueError, lambda: native_park.GoalDecision.accept(None), both_or_neither)
assert_raises(
    ValueError,
    lambda: native_park.GoalDecision(
        native_park.GoalResponse(parked=True),
        native_park.Failure(native_park.FailureKind.REJECTED, "busy"),
    ),
    both_or_neither,
)
assert_raises(ValueError, lambda: home.GoalDecision(object(), None), "this action declares no GoalResponse")
assert home.GoalDecision.accept().refusal is None

# The refusal of a decision is a failure of its member: a failure of another
# member or of peppylib is parsed again.
assert_raises(
    ValueError,
    lambda: native_park.GoalDecision(
        None, contract_park.Failure(contract_park.FailureKind.UNREACHABLE, "no plan")
    ),
    "is not a failure kind of park_arm (message: 'no plan')",
)
decision = native_park.GoalDecision(None, peppylib.FailureError("timeout", "slow"))
assert type(decision.refusal) is native_park.Failure, type(decision.refusal)
assert decision.refusal == native_park.Failure(native_park.FailureKind.TIMEOUT, "slow")


class Context:
    """The context of an accepted goal: it records how the goal ends."""

    cancel_cause = "sender_gone"
    cancel_reason = "its sender is gone"

    def __init__(self):
        self.ends = []

    async def fail(self, kind, message):
        self.ends.append(("failed", kind, message))


class Goal:
    """A goal that waits for its answer: it records the answer."""

    instance_id = "client_instance"
    core_node = "client_core"
    request_bytes = b""

    def __init__(self):
        self.answers = []

    async def refuse(self, kind, message):
        self.answers.append(("refused", kind, message))

    async def accept(self, response):
        self.answers.append(("accepted", response))
        return Context()


class Engine:
    """Gives each goal once, then the end of the goal stream."""

    def __init__(self, goals):
        self._goals = list(goals)

    async def recv_next_goal(self):
        return self._goals.pop(0) if self._goals else None


def serve(module, handler, goals):
    """The answer of the action of `module` to `goals`, with `handler`."""
    action = module.ActionHandle()
    action._inner = Engine(goals)
    return asyncio.run(action.handle_goal_next_request(handler))


def in_turn(*handlers):
    """A handler that hands each goal to the next of `handlers`."""
    remaining = list(handlers)
    return lambda request: remaining.pop(0)(request)


def refuse_unreachable(request):
    """A handler shared between the two park_arm members, which builds the
    decisions of the member of the contract."""
    return contract_park.GoalDecision.refuse(contract_park.FailureKind.UNREACHABLE, "no plan")


def accept_parked(request):
    return native_park.GoalDecision.accept(native_park.GoalResponse(parked=True))


# The member of the contract declares the kind: the goal is refused with it.
goal = Goal()
assert serve(contract_park, refuse_unreachable, [goal]) is None
assert goal.answers == [("refused", "unreachable", "no plan")], goal.answers


async def refuse_timeout(request):
    """A shared asynchronous handler that refuses with a kind of peppy."""
    return contract_park.GoalDecision.refuse(contract_park.FailureKind.TIMEOUT, "slow")


goal = Goal()
assert serve(native_park, refuse_timeout, [goal]) is None
assert goal.answers == [("refused", "timeout", "slow")], goal.answers


def refuse_with_an_exception(request):
    return native_park.GoalDecision.refuse(native_park.FailureKind.TIMEOUT, TimeoutError("slow"))


def give_nothing(request):
    return None


# Each goal gets an answer, and the server keeps serving: a decision that the
# serving member cannot send, and a handler that raises, refuse the goal with
# attempt_failed and the error, and the next goal is accepted. Nothing of the
# decision of the handler reaches the wire.
for handler, error in (
    (refuse_unreachable, "ValueError: 'unreachable' is not a failure kind of park_arm (message: 'no plan')"),
    (lambda request: home.GoalDecision.accept(), f"ValueError: a goal decision is {both_or_neither}"),
    (lambda request: native_park.GoalDecision(None, None), f"ValueError: a goal decision is {both_or_neither}"),
    (refuse_with_an_exception, "TypeError: the message of a failure of park_arm is a str, not TimeoutError"),
    (give_nothing, "AttributeError: 'NoneType' object has no attribute 'response'"),
):
    refused, accepted = Goal(), Goal()
    context = serve(native_park, in_turn(handler, accept_parked), [refused, accepted])
    assert refused.answers == [("refused", "attempt_failed", f"the goal handler raised {error}")], refused.answers
    assert [answer[0] for answer in accepted.answers] == ["accepted"], accepted.answers
    assert isinstance(context, native_park.GoalContext), type(context)

# A response that does not encode refuses the goal the same way.
refused = Goal()
assert serve(
    native_park,
    lambda request: native_park.GoalDecision.accept(native_park.GoalResponse(parked="yes")),
    [refused],
) is None
[(answer, kind, message)] = refused.answers
assert (answer, kind) == ("refused", "attempt_failed"), refused.answers
assert message.startswith("the goal handler raised KjException: "), message

# An accept sends the encoded response, and gives the context of the goal.
goal = Goal()
context = serve(native_park, accept_parked, [goal])
assert isinstance(context, native_park.GoalContext), type(context)
assert [answer[0] for answer in goal.answers] == ["accepted"], goal.answers
assert goal.answers[0][1], "the accept carries the encoded response"

goal = Goal()
serve(home, lambda request: home.GoalDecision.accept(), [goal])
assert goal.answers == [("accepted", b"")], goal.answers

# The context gives the reason of the cancel cause, and fails the goal with
# a kind that its member declares and a message that is a str.
assert context.cancel_cause() == "sender_gone"
assert context.cancel_reason() == "its sender is gone"
asyncio.run(context.fail(native_park.FailureKind.TIMEOUT, "the arm is slow"))
assert context._inner.ends == [("failed", "timeout", "the arm is slow")], context._inner.ends
assert_raises(
    ValueError,
    lambda: asyncio.run(context.fail(contract_park.FailureKind.UNREACHABLE, "no plan")),
    "is not a failure kind of park_arm (message: 'no plan')",
)
assert_raises(
    TypeError,
    lambda: asyncio.run(context.fail(native_park.FailureKind.TIMEOUT, TimeoutError("slow"))),
    "the message of a failure of park_arm is a str, not TimeoutError",
)
assert context._inner.ends == [("failed", "timeout", "the arm is slow")], context._inner.ends


def handle(module, handler):
    """The reply of the service of `module` to one request, with `handler`."""
    return asyncio.run(module._handle_request_payload(handler, "client_core", "client_instance"))


def raise_failure(failure):
    def handler(request):
        raise failure

    return handler


# A service handler gives an error reply of its member with any
# peppylib.FailureError that it raises or returns: the Failure of another
# member, or one of peppylib.
for service in (clean, focus):
    for failure in (lens.Failure(lens.FailureKind.TIMEOUT, "slow"), peppylib.FailureError("timeout", "slow")):
        for handler in (lambda request: failure, raise_failure(failure)):
            assert_failure(lambda: handle(service, handler), service, service.FailureKind.TIMEOUT, "slow")
    assert_failure(
        lambda: handle(service, lambda request: service.Failure(service.FailureKind.REJECTED, "no")),
        service,
        service.FailureKind.REJECTED,
        "no",
    )

    # A kind that the member does not declare, and a message that is not a
    # str, raise before the wire, so the runtime gives attempt_failed with
    # the text of the error.
    undeclared = lens.Failure(lens.FailureKind.LENS_COVERED, "tape on the lens")
    for handler in (lambda request: undeclared, raise_failure(undeclared)):
        assert_raises(
            ValueError,
            lambda: handle(service, handler),
            f"'lens_covered' is not a failure kind of {service.SERVICE_NAME} (message: 'tape on the lens')",
        )
    not_a_str = peppylib.FailureError("timeout", TimeoutError("slow"))
    for handler in (lambda request: not_a_str, raise_failure(not_a_str)):
        assert_raises(
            TypeError,
            lambda: handle(service, handler),
            f"the message of a failure of {service.SERVICE_NAME} is a str, not TimeoutError",
        )

assert handle(clean, lambda request: None) == b""
assert handle(focus, lambda request: focus.Response(focused=True)), "the response is encoded"

print("failure kinds hold")
"#;

/// The member `arm_impl` of the contract `arm_actions`, which declares the
/// kinds `arm_reserved` and `unreachable`.
fn arm_impl_origin() -> ContractOrigin {
    ContractOrigin {
        link_id: "arm_impl".to_string(),
        contract_name: "arm_actions".to_string(),
        contract_tag: "v1".to_string(),
        failure_kinds: failure_kinds(&[
            ("arm_reserved", "The arm of the goal is reserved"),
            ("unreachable", "No plan to the target"),
        ]),
    }
}

#[test]
fn python_members_give_and_take_their_declared_kinds_only() {
    let temp_dir = TempDir::new_in(config_test_support::test_tmp_root())
        .expect("failed to create temp directory");
    let user_node_dir = temp_dir.path().join("user_node");
    fs::create_dir_all(&user_node_dir).expect("failed to create user_node directory");
    fs::write(user_node_dir.join(NODE_CONFIG_FILE), NODE_CONFIG)
        .expect("failed to write peppy.json5");

    let action: NativeExposedAction =
        serde_json5::from_str(EXPOSED_ACTION_EXAMPLE).expect("the action parses");
    let check_lens: ConsumedService =
        serde_json5::from_str(r#"{ link_id: "lens", name: "check_lens" }"#)
            .expect("the consumed service parses");
    // An action with a goal response and no request, and one with neither.
    let park_arm: NativeExposedAction = serde_json5::from_str(
        r#"{ name: "park_arm", goal_service: { response_message_format: { parked: "bool" } } }"#,
    )
    .expect("the action parses");
    let home_arm: NativeExposedAction =
        serde_json5::from_str(r#"{ name: "home_arm" }"#).expect("the action parses");
    // A service with no response, and one with a response.
    let clean_lens: NativeExposedService =
        serde_json5::from_str(r#"{ name: "clean_lens" }"#).expect("the service parses");
    let get_focus: NativeExposedService = serde_json5::from_str(
        r#"{ name: "get_focus", response_message_format: { focused: "bool" } }"#,
    )
    .expect("the service parses");
    let interfaces = vec![
        DeploymentInterface::exposed_action(action.clone(), None),
        DeploymentInterface::exposed_action(action, Some(arm_impl_origin())),
        DeploymentInterface::exposed_action(park_arm.clone(), None),
        DeploymentInterface::exposed_action(park_arm, Some(arm_impl_origin())),
        DeploymentInterface::exposed_action(home_arm, None),
        DeploymentInterface::exposed_service(clean_lens, None),
        DeploymentInterface::exposed_service(get_focus, None),
        DeploymentInterface::consumed_service(
            check_lens,
            MessageFormat::default(),
            serde_json5::from_str(r#"{ clear: "bool" }"#).expect("the response format parses"),
            DependencyContext::contract(
                "lens_contract",
                "v1",
                "lens",
                Cardinality::One,
                failure_kinds(&[("lens_covered", "Something covers the lens")]),
            ),
        ),
    ];

    generate_test_peppygen_lib(PeppygenLanguage::Python, &user_node_dir, interfaces)
        .expect("failed to generate peppygen lib");

    run_python_check(
        &user_node_dir,
        PYTHON_PROBE,
        "Python probe of the failure kinds of generated members",
    );
}
