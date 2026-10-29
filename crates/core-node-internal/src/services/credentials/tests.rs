use super::*;
use auth::test_support::TestMachineCa;
use pmi::{CallReceiver, ServiceQueryKind, SignedCall};

const NAMESPACE: &str = "ws-a";
const DESK: &str = "cn-desk";
const ARM: &str = "cn-arm";
const KEY_EXPR: &str = "cn-desk/cn-desk/*/peppy-cli/service/node/cn-desk/core/*/stack_reset";

/// The keys of the daemon of `core_node`, prepared in `root`, with the
/// certificate of its machine from `ca` when one is given.
fn daemon_keys(
    root: &tempfile::TempDir,
    core_node: &str,
    ca: Option<&TestMachineCa>,
) -> Arc<DaemonKeys> {
    let dirs = PeppyDirs::new(root.path());
    let machine = ca.map(|ca| {
        let files = ca.write_machine(root.path(), core_node);
        MachineCertificate::of_operator(&files.certificate, &files.private_key, &files.trust_anchor)
            .unwrap()
    });
    let keys = Arc::new(DaemonKeys::new(
        dirs,
        NAMESPACE,
        core_node,
        &format!("{core_node}-boot"),
        machine,
    ));
    keys.prepare().unwrap();
    keys
}

fn empty_stack(root: &tempfile::TempDir) -> Arc<NodeStack> {
    let root_config = config::node::NodeConfigParser::from_content(
        r#"{
            peppy_schema: "node/v1",
            manifest: { name: "cn_desk", tag: "v1", description: ["A test node."] },
            interfaces: {},
            execution: { language: "rust", parameters: {}, build_cmd: ["true"], run_cmd: ["true"] },
        }"#,
    )
    .unwrap();
    Arc::new(NodeStack::new(root_config, None, root.path()))
}

/// The check of `keys`, whose clock reads `at`.
fn check_at(keys: &Arc<DaemonKeys>, stack: &Arc<NodeStack>, at: SystemTime) -> DaemonCallCheck {
    DaemonCallCheck::with_clock(Arc::clone(keys), Arc::clone(stack), Arc::new(move || at))
}

/// When the sender signed `proof`.
fn signed_at(proof: &CallProof) -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_millis(proof.timestamp_ms())
}

fn call<'a>(
    core_node: &'a str,
    instance_id: &'a str,
    proof: Option<&'a CallProof>,
) -> ReceivedCall<'a> {
    ReceivedCall {
        caller_core_node: core_node,
        caller_instance_id: instance_id,
        signed: SignedCall {
            namespace: NAMESPACE,
            key_expr: KEY_EXPR,
            kind: ServiceQueryKind::UserRequest,
            payload: b"reset",
        },
        proof,
    }
}

/// The proof of the call of [`call`] by `signer`, for `receiver`.
fn prove(signer: &CallSigner, receiver: CallReceiver) -> CallProof {
    signer.prove(&call("", "", None).signed, receiver)
}

/// The signer of the command line of the daemon of `root`, from its key file.
fn command_line_signer(root: &tempfile::TempDir) -> CallSigner {
    ClientCredential::read(&PeppyDirs::new(root.path()).command_line_key_path())
        .unwrap()
        .into_signer()
}

fn refusal_of(result: Result<VerifiedCaller, Failure>) -> String {
    result
        .expect_err("the call is refused")
        .message()
        .to_owned()
}

#[test]
fn the_command_line_and_the_daemon_of_this_machine_prove_themselves() {
    let root = tempfile::tempdir().unwrap();
    let keys = daemon_keys(&root, DESK, None);
    let stack = empty_stack(&root);

    let proof = prove(&command_line_signer(&root), CallReceiver::ThisMachine);
    let caller = check_at(&keys, &stack, signed_at(&proof))
        .verify(&call(DESK, "peppy-cli", Some(&proof)))
        .unwrap();
    assert_eq!(
        caller,
        VerifiedCaller::new(DESK, "peppy-cli", CallerKind::CommandLine)
    );

    let proof = prove(&keys.daemon_signer(), CallReceiver::ThisMachine);
    let caller = check_at(&keys, &stack, signed_at(&proof))
        .verify(&call(DESK, "cn-desk-boot", Some(&proof)))
        .unwrap();
    assert_eq!(caller.kind(), CallerKind::Daemon);
}

#[test]
fn the_key_of_the_command_line_stays_across_boots_and_only_its_user_reads_it() {
    let root = tempfile::tempdir().unwrap();
    daemon_keys(&root, DESK, None);
    let first = command_line_signer(&root).public_key();
    daemon_keys(&root, DESK, None);
    assert_eq!(command_line_signer(&root).public_key(), first);

    use std::os::unix::fs::PermissionsExt as _;
    let dirs = PeppyDirs::new(root.path());
    let mode =
        |path: &std::path::Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(&dirs.credentials_dir()), 0o700);
    assert_eq!(mode(&dirs.instance_keys_dir()), 0o700);
    assert_eq!(mode(&dirs.command_line_key_path()), 0o600);
}

#[test]
fn a_call_with_no_signature_or_under_another_id_is_refused() {
    let root = tempfile::tempdir().unwrap();
    let keys = daemon_keys(&root, DESK, None);
    let stack = empty_stack(&root);
    let check = check_at(&keys, &stack, SystemTime::now());

    let refused = refusal_of(check.verify(&call(DESK, "peppy-cli", None)));
    assert!(refused.contains("carries no signature"), "{refused}");
    assert!(refused.contains("build it again"), "{refused}");

    let stranger = ClientCredential::new(ClientKey::generate(), None).into_signer();
    let proof = prove(&stranger, CallReceiver::ThisMachine);
    let check = check_at(&keys, &stack, signed_at(&proof));
    let refused = refusal_of(check.verify(&call(DESK, "peppy-cli", Some(&proof))));
    assert!(refused.contains("does not verify"), "{refused}");
    let refused = refusal_of(check.verify(&call(DESK, "arm_inst", Some(&proof))));
    assert!(refused.contains("has no key on this daemon"), "{refused}");
}

#[test]
fn an_instance_whose_process_does_not_run_has_no_key() {
    let root = tempfile::tempdir().unwrap();
    let keys = daemon_keys(&root, DESK, None);
    let stack = empty_stack(&root);
    let issued = keys.issue_instance_key("arm_inst").unwrap();
    let signer = ClientCredential::read(&issued.path).unwrap().into_signer();

    let proof = prove(&signer, CallReceiver::ThisMachine);
    let refused = refusal_of(check_at(&keys, &stack, signed_at(&proof)).verify(&call(
        DESK,
        "arm_inst",
        Some(&proof),
    )));
    assert!(refused.contains("has no key on this daemon"), "{refused}");
}

#[test]
fn each_start_of_an_instance_writes_a_key_file_of_its_own() {
    let root = tempfile::tempdir().unwrap();
    let keys = daemon_keys(&root, DESK, None);
    let first = keys.issue_instance_key("arm_inst").unwrap();
    let second = keys.issue_instance_key("arm_inst").unwrap();
    assert_ne!(first.path, second.path);
    assert_ne!(
        ClientCredential::read(&first.path)
            .unwrap()
            .key()
            .public_key(),
        ClientCredential::read(&second.path)
            .unwrap()
            .key()
            .public_key()
    );

    daemon_keys(&root, DESK, None);
    assert!(
        !first.path.exists() && !second.path.exists(),
        "a boot removes the key files of the instances of an earlier boot"
    );
}

#[test]
fn a_timestamp_outside_the_window_and_a_replayed_call_are_refused() {
    let root = tempfile::tempdir().unwrap();
    let keys = daemon_keys(&root, DESK, None);
    let stack = empty_stack(&root);
    let proof = prove(&command_line_signer(&root), CallReceiver::ThisMachine);

    let skew = TIMESTAMP_WINDOW + Duration::from_millis(1);
    let late = check_at(&keys, &stack, signed_at(&proof) + skew);
    let refused = refusal_of(late.verify(&call(DESK, "peppy-cli", Some(&proof))));
    assert!(refused.contains("outside the window of 30 s"), "{refused}");
    let early = check_at(&keys, &stack, signed_at(&proof) - skew);
    assert!(
        early
            .verify(&call(DESK, "peppy-cli", Some(&proof)))
            .is_err()
    );

    let check = check_at(&keys, &stack, signed_at(&proof) + TIMESTAMP_WINDOW);
    check
        .verify(&call(DESK, "peppy-cli", Some(&proof)))
        .expect("at the edge of the window");
    let refused = refusal_of(check.verify(&call(DESK, "peppy-cli", Some(&proof))));
    assert!(refused.contains("a replayed call"), "{refused}");
}

#[test]
fn the_nonces_of_the_calls_that_left_the_window_are_dropped() {
    let mut window = NonceWindow::default();
    let key = ClientKey::generate().public_key();
    let window_ms = TIMESTAMP_WINDOW.as_millis() as u64;
    assert!(window.take(key, [1; NONCE_LEN], 1_000, 1_000, window_ms));
    assert!(!window.take(key, [1; NONCE_LEN], 1_000, 2_000, window_ms));
    assert!(window.take(key, [2; NONCE_LEN], 40_000, 40_000, window_ms));
    assert_eq!(window.taken.len(), 1, "the first call left the window");
}

#[test]
fn a_call_of_another_machine_proves_its_key_with_the_certificate_of_its_machine() {
    let ca = TestMachineCa::root("operator CA");
    let desk_root = tempfile::tempdir().unwrap();
    let arm_root = tempfile::tempdir().unwrap();
    let desk = daemon_keys(&desk_root, DESK, Some(&ca));
    let arm = daemon_keys(&arm_root, ARM, Some(&ca));
    let stack = empty_stack(&desk_root);

    let proof = prove(&command_line_signer(&arm_root), CallReceiver::OtherMachine);
    let caller = check_at(&desk, &stack, signed_at(&proof))
        .verify(&call(ARM, "peppy-cli", Some(&proof)))
        .unwrap();
    assert_eq!(
        caller,
        VerifiedCaller::new(ARM, "peppy-cli", CallerKind::CommandLine)
    );

    let proof = prove(&arm.daemon_signer(), CallReceiver::OtherMachine);
    let caller = check_at(&desk, &stack, signed_at(&proof))
        .verify(&call(ARM, "cn-arm-boot", Some(&proof)))
        .unwrap();
    assert_eq!(caller.kind(), CallerKind::Daemon);

    let issued = arm.issue_instance_key("arm_inst").unwrap();
    let signer = ClientCredential::read(&issued.path).unwrap().into_signer();
    let proof = prove(&signer, CallReceiver::OtherMachine);
    let caller = check_at(&desk, &stack, signed_at(&proof))
        .verify(&call(ARM, "arm_inst", Some(&proof)))
        .unwrap();
    assert_eq!(caller.kind(), CallerKind::Instance);

    let refused = refusal_of(check_at(&desk, &stack, signed_at(&proof)).verify(&call(
        ARM,
        "planner_inst",
        Some(&proof),
    )));
    assert!(
        refused.contains("endorses no key for `planner_inst`"),
        "{refused}"
    );
}

#[test]
fn a_call_of_another_machine_without_a_proof_or_a_ca_or_from_another_ca_is_refused() {
    let ours = TestMachineCa::root("our CA");
    let theirs = TestMachineCa::root("their CA");
    let desk_root = tempfile::tempdir().unwrap();
    let arm_root = tempfile::tempdir().unwrap();
    let stranger_root = tempfile::tempdir().unwrap();
    let plain_root = tempfile::tempdir().unwrap();
    let desk = daemon_keys(&desk_root, DESK, Some(&ours));
    daemon_keys(&arm_root, ARM, Some(&theirs));
    daemon_keys(&stranger_root, ARM, None);
    let plain = daemon_keys(&plain_root, DESK, None);
    let stack = empty_stack(&desk_root);

    let proof = prove(&command_line_signer(&arm_root), CallReceiver::OtherMachine);
    let refused = refusal_of(check_at(&desk, &stack, signed_at(&proof)).verify(&call(
        ARM,
        "peppy-cli",
        Some(&proof),
    )));
    assert!(
        refused.contains("does not chain to the CA of this daemon"),
        "{refused}"
    );
    let refused = refusal_of(check_at(&plain, &stack, signed_at(&proof)).verify(&call(
        ARM,
        "peppy-cli",
        Some(&proof),
    )));
    assert!(refused.contains("has no machine certificate"), "{refused}");

    let proof = prove(
        &command_line_signer(&stranger_root),
        CallReceiver::OtherMachine,
    );
    let refused = refusal_of(check_at(&desk, &stack, signed_at(&proof)).verify(&call(
        ARM,
        "peppy-cli",
        Some(&proof),
    )));
    assert!(refused.contains("carries no machine proof"), "{refused}");
}

#[test]
fn a_machine_cannot_give_the_core_node_name_of_another_machine() {
    let ca = TestMachineCa::root("operator CA");
    let desk_root = tempfile::tempdir().unwrap();
    let arm_root = tempfile::tempdir().unwrap();
    let desk = daemon_keys(&desk_root, DESK, Some(&ca));
    daemon_keys(&arm_root, ARM, Some(&ca));
    let stack = empty_stack(&desk_root);

    let proof = prove(&command_line_signer(&arm_root), CallReceiver::OtherMachine);
    let refused = refusal_of(check_at(&desk, &stack, signed_at(&proof)).verify(&call(
        "cn-gripper",
        "peppy-cli",
        Some(&proof),
    )));
    assert!(
        refused.contains("names the core node `cn-arm`"),
        "{refused}"
    );
}
