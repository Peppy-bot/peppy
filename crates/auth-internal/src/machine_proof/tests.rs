use super::*;
use crate::test_support::{EXPIRES_AT, ISSUED_AT, TestMachineCa};
use std::time::Duration;

const NAMESPACE: &str = "ws-a";
const MACHINE: &str = "cn-arm";
const BOOT: &str = "calm-heron-5678";

/// A moment in the validity of the test certificates.
fn during_validity() -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_secs(((ISSUED_AT + EXPIRES_AT) / 2) as u64)
}

fn sender<'a>(instance_id: &'a str) -> ClaimedSender<'a> {
    ClaimedSender {
        namespace: NAMESPACE,
        core_node: MACHINE,
        instance_id,
    }
}

/// The certificate of the machine `common_name` from the operator CA `ca`,
/// written to `directory`.
fn operator_machine(
    ca: &TestMachineCa,
    directory: &tempfile::TempDir,
    common_name: &str,
) -> MachineCertificate {
    let files = ca.write_machine(directory.path(), common_name);
    MachineCertificate::of_operator(&files.certificate, &files.private_key, &files.trust_anchor)
        .unwrap()
}

/// A boot of the machine: its key and its credential.
fn boot(machine: &MachineCertificate) -> (ClientKey, BootCredential) {
    let key = ClientKey::generate();
    let credential = machine.boot_credential(NAMESPACE, MACHINE, BOOT, &key.public_key());
    (key, credential)
}

#[test]
fn the_proof_of_a_daemon_gives_the_key_of_its_boot() {
    let ca = TestMachineCa::root("operator CA");
    let directory = tempfile::tempdir().unwrap();
    let machine = operator_machine(&ca, &directory, MACHINE);
    let (boot_key, credential) = boot(&machine);

    let proven = machine
        .trust()
        .verify(&credential.daemon_proof(), sender(BOOT), during_validity())
        .unwrap();
    assert_eq!(
        proven,
        ProvenKey {
            key: boot_key.public_key(),
            is_daemon: true
        }
    );
}

#[test]
fn the_proof_of_a_client_gives_the_key_that_the_boot_endorses_for_it_alone() {
    let ca = TestMachineCa::root("operator CA");
    let directory = tempfile::tempdir().unwrap();
    let machine = operator_machine(&ca, &directory, MACHINE);
    let (boot_key, credential) = boot(&machine);
    let client_key = ClientKey::generate().public_key();
    let proof = credential.client_proof("peppy-cli", &client_key, &boot_key);

    let proven = machine
        .trust()
        .verify(&proof, sender("peppy-cli"), during_validity())
        .unwrap();
    assert_eq!(
        proven,
        ProvenKey {
            key: client_key,
            is_daemon: false
        }
    );
    assert!(matches!(
        machine
            .trust()
            .verify(&proof, sender("arm_inst"), during_validity()),
        Err(MachineProofError::NoEndorsement(client)) if client == "arm_inst"
    ));
}

#[test]
fn a_proof_that_names_another_namespace_or_machine_is_refused() {
    let ca = TestMachineCa::root("operator CA");
    let directory = tempfile::tempdir().unwrap();
    let machine = operator_machine(&ca, &directory, MACHINE);
    let (_, credential) = boot(&machine);
    let proof = credential.daemon_proof();
    let trust = machine.trust();

    let elsewhere = ClaimedSender {
        namespace: "ws-b",
        ..sender(BOOT)
    };
    assert!(matches!(
        trust.verify(&proof, elsewhere, during_validity()),
        Err(MachineProofError::NamesAnother {
            field: "the namespace",
            ..
        })
    ));
    let other_machine = ClaimedSender {
        core_node: "cn-desk",
        ..sender(BOOT)
    };
    assert!(matches!(
        trust.verify(&proof, other_machine, during_validity()),
        Err(MachineProofError::NamesAnother {
            field: "the core node",
            ..
        })
    ));
}

#[test]
fn a_certificate_of_another_ca_or_out_of_its_validity_is_refused() {
    let ours = TestMachineCa::root("our CA");
    let theirs = TestMachineCa::root("their CA");
    let our_directory = tempfile::tempdir().unwrap();
    let their_directory = tempfile::tempdir().unwrap();
    let our_machine = operator_machine(&ours, &our_directory, MACHINE);
    let their_machine = operator_machine(&theirs, &their_directory, MACHINE);
    let (_, their_credential) = boot(&their_machine);

    assert!(matches!(
        our_machine.trust().verify(
            &their_credential.daemon_proof(),
            sender(BOOT),
            during_validity()
        ),
        Err(MachineProofError::Chain(_))
    ));

    let (_, our_credential) = boot(&our_machine);
    let after = SystemTime::UNIX_EPOCH + Duration::from_secs(EXPIRES_AT as u64 + 1);
    assert!(matches!(
        our_machine
            .trust()
            .verify(&our_credential.daemon_proof(), sender(BOOT), after),
        Err(MachineProofError::Chain(_))
    ));
}

#[test]
fn an_operator_certificate_names_the_core_node_of_its_machine() {
    let ca = TestMachineCa::root("operator CA");
    let directory = tempfile::tempdir().unwrap();
    let impostor = operator_machine(&ca, &directory, "cn-desk");
    let (_, credential) = boot(&impostor);

    assert!(matches!(
        impostor
            .trust()
            .verify(&credential.daemon_proof(), sender(BOOT), during_validity()),
        Err(MachineProofError::CommonName { named, .. }) if named == "cn-desk"
    ));
}

#[test]
fn a_platform_certificate_chains_to_the_project_ca_through_the_issuer_of_its_peer() {
    let project = TestMachineCa::root("project CA");
    let peer_issuer = project.issuer("issuer of the peer");
    let directory = tempfile::tempdir().unwrap();
    let (leaf, key) = peer_issuer.leaf_pem("my robot", ISSUED_AT, EXPIRES_AT);
    let write = |name: &str, content: &str| {
        let path = directory.path().join(name);
        std::fs::write(&path, content).unwrap();
        path
    };
    let enrollment = Enrollment {
        peer_certificate: write("peer.crt", &leaf),
        peer_key: write("peer.key", &key),
        issuer_chain: write("chain.crt", &peer_issuer.chain_pem()),
        trust_anchor: write("ca.crt", &project.trust_anchor_pem()),
        ..crate::test_support::enrollment()
    };
    let machine = MachineCertificate::of_enrollment(&enrollment).unwrap();
    let (boot_key, credential) = boot(&machine);

    let proven = machine
        .trust()
        .verify(&credential.daemon_proof(), sender(BOOT), during_validity())
        .unwrap();
    assert_eq!(
        proven.key,
        boot_key.public_key(),
        "the label of the leaf names no core node"
    );
}

#[test]
fn a_tampered_statement_or_endorsement_is_refused() {
    let ca = TestMachineCa::root("operator CA");
    let directory = tempfile::tempdir().unwrap();
    let machine = operator_machine(&ca, &directory, MACHINE);
    let (boot_key, credential) = boot(&machine);
    let trust = machine.trust();

    let mut statement = MachineProof::decode(&credential.daemon_proof()).unwrap();
    statement.boot.public_key = ClientKey::generate().public_key();
    assert!(matches!(
        trust.verify(&statement.encode(), sender(BOOT), during_validity()),
        Err(MachineProofError::BootStatement)
    ));

    let client_key = ClientKey::generate().public_key();
    let mut endorsed =
        MachineProof::decode(&credential.client_proof("arm_inst", &client_key, &boot_key)).unwrap();
    endorsed
        .endorsement
        .as_mut()
        .expect("a client proof holds an endorsement")
        .public_key = ClientKey::generate().public_key();
    assert!(matches!(
        trust.verify(&endorsed.encode(), sender("arm_inst"), during_validity()),
        Err(MachineProofError::Endorsement)
    ));

    assert!(matches!(
        trust.verify(b"not json", sender(BOOT), during_validity()),
        Err(MachineProofError::Decode(_))
    ));
}

#[test]
fn a_key_that_is_not_a_supported_pkcs8_key_is_refused() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("machine.key");
    std::fs::write(
        &path,
        "-----BEGIN PRIVATE KEY-----\nAAAA\n-----END PRIVATE KEY-----\n",
    )
    .unwrap();
    assert!(matches!(
        MachineKey::read(&path),
        Err(MachineProofError::UnsupportedKey)
    ));
}
