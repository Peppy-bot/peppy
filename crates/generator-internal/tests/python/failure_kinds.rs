//! The failure kinds of generated Python members, without a wire: each
//! member's `FailureKind` holds the kinds of peppy, then the kinds of its
//! contract, and its `Failure` takes no other kind. A provider that refuses
//! a goal with a kind its member does not declare raises `ValueError`
//! before anything reaches the wire, and a client refuses such a kind from
//! the wire the same way.
//!
//! The Rust counterpart is the compile failure of
//! `a_native_provider_cannot_give_an_undeclared_kind`.

use crate::helpers::{
    EXPOSED_ACTION_EXAMPLE, failure_kinds, generate_test_peppygen_lib, run_python_check,
};
use config::consts::NODE_CONFIG_FILE;
use config::node::{
    Cardinality, ConsumedService, MessageFormat, NativeExposedAction, PeppygenLanguage,
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
import importlib
import peppylib

native = importlib.import_module("peppygen.exposed_actions.move_arm")
contract = importlib.import_module("peppygen.exposed_actions.arm_impl.move_arm")
lens = importlib.import_module("peppygen.consumed_services.lens.check_lens")

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

# ...and a kind that its member does not declare raises before the wire.
for kind in ("unreachable", "not_a_kind"):
    try:
        native.GoalDecision.refuse(kind, "no plan")
    except ValueError as error:
        assert "is not a failure kind of move_arm" in str(error), error
    else:
        raise SystemExit(f"a native refusal with {kind!r} must raise")

# A failure is a peppylib.FailureError whose args are its kind and message.
failure = lens.Failure(lens.FailureKind.LENS_COVERED, "tape on the lens")
assert isinstance(failure, peppylib.FailureError)
assert failure.args == ("lens_covered", "tape on the lens"), failure.args
assert str(failure) == "lens_covered: tape on the lens", str(failure)

# A client parses the wire form of a failure, and refuses a kind that its
# member does not declare.
assert lens.Failure("lens_covered", "tape on the lens") == failure
try:
    lens.Failure("unreachable", "no plan")
except ValueError:
    pass
else:
    raise SystemExit("a client must refuse a kind that its member does not declare")

print("failure kinds hold")
"#;

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
    let interfaces = vec![
        DeploymentInterface::exposed_action(action.clone(), None),
        DeploymentInterface::exposed_action(
            action,
            Some(ContractOrigin {
                link_id: "arm_impl".to_string(),
                contract_name: "arm_actions".to_string(),
                contract_tag: "v1".to_string(),
                failure_kinds: failure_kinds(&[
                    ("arm_reserved", "The arm of the goal is reserved"),
                    ("unreachable", "No plan to the target"),
                ]),
            }),
        ),
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
