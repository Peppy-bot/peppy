//! The failure kinds of a member in generated Python: the `FailureKind` enum
//! and the `Failure` exception that each action and service module declares.
//!
//! A member gives a kind from a closed set: the kinds of peppy and the kinds
//! that its contract declares. `FailureKind` holds that set, and `Failure`
//! takes no other kind: its constructor raises `ValueError` for any other,
//! before anything reaches the wire, and the generated clients parse each
//! failure from the wire through it.
//!
//! `Failure` is a `peppylib.FailureError`, so a service handler raises it to
//! give an error reply of its kind, and a caller catches it from `poll`.

use super::code_builder::{PythonCodeBuilder, python_string_literal};
use config::failure_kind::MemberFailureKinds;

/// Emits `FailureKind` and `Failure` for a member that gives `kinds`.
/// `member_name` names the module constant that holds the name of the
/// member, for the error of a kind the member does not declare.
pub(super) fn emit_failure_items(
    builder: &mut PythonCodeBuilder,
    kinds: &MemberFailureKinds,
    member_name: &str,
) {
    builder.add_import("from enum import Enum");
    builder.add_import("import peppylib");
    builder.block("class FailureKind(str, Enum):", |builder| {
        builder.docstring(
            "A failure kind of this member: a kind of peppy, or a kind that its contract declares.",
        );
        builder.blank_line();
        for (name, meaning) in kinds.iter() {
            // A kind name is lowercase snake case, so its upper case is a
            // distinct name for each kind, and never a keyword.
            builder.line(&format!(
                "{} = {}",
                name.to_uppercase(),
                python_string_literal(name)
            ));
            builder.line(&python_string_literal(meaning));
        }
    });
    builder.blank_line();
    builder.blank_line();
    builder.block("class Failure(peppylib.FailureError):", |builder| {
        builder.multiline_docstring(
            "Why this member refused or failed a goal, or gave an error reply.",
            [
                "A kind that this member declares, and a message for a person. The",
                "constructor raises ValueError for any other kind. A service handler",
                "raises or returns it to give an error reply of its kind.",
            ],
        );
        builder.blank_line();
        builder.lines(["kind: FailureKind", "message: str"]);
        builder.blank_line();
        builder.block(
            "def __init__(self, kind: FailureKind | str, message: str) -> None:",
            |builder| {
                builder.block("try:", |builder| builder.line("parsed = FailureKind(kind)"));
                builder.block("except ValueError:", |builder| {
                    builder.line(&format!(
                        "raise ValueError(f\"{{kind!r}} is not a failure kind of {{{member_name}}}\") from None"
                    ));
                });
                builder.py(r#"
super().__init__(parsed.value, message)
self.kind = parsed
self.message = message
"#);
            },
        );
        builder.blank_line();
        builder.py(r#"
def __eq__(self, other: object) -> bool:
    if not isinstance(other, Failure):
        return NotImplemented
    return self.kind is other.kind and self.message == other.message

def __hash__(self) -> int:
    return hash((self.kind, self.message))

def __repr__(self) -> str:
    return f"Failure({self.kind!r}, {self.message!r})"

def __str__(self) -> str:
    return f"{self.kind.value}: {self.message}"
"#);
    });
    builder.blank_line();
}

#[cfg(test)]
mod tests {
    use super::*;
    use config::failure_kind::{ContractFailureKinds, FailureKindName};

    fn render(kinds: &MemberFailureKinds) -> String {
        let mut builder = PythonCodeBuilder::new();
        emit_failure_items(&mut builder, kinds, "SERVICE_NAME");
        builder.build()
    }

    #[test]
    fn a_native_member_declares_the_kinds_of_peppy_only() {
        let rendered = render(&MemberFailureKinds::peppy_only());
        assert!(
            rendered.contains(
                "    ATTEMPT_FAILED = \"attempt_failed\"\n    \"The attempt gave no result\"\n"
            ),
            "{rendered}"
        );
        assert!(rendered.contains("    REJECTED = \"rejected\"\n"));
        assert!(!rendered.contains("UNREACHABLE"));
    }

    #[test]
    fn a_contract_member_declares_its_kinds_after_the_kinds_of_peppy() {
        let contract = ContractFailureKinds::new([(
            FailureKindName::parse("unreachable").unwrap(),
            "No plan to the \"target\"".to_string(),
        )])
        .unwrap();
        let rendered = render(&MemberFailureKinds::of_contract(contract));
        let rejected = rendered.find("REJECTED =").unwrap();
        let unreachable = rendered.find("UNREACHABLE =").unwrap();
        assert!(rejected < unreachable, "{rendered}");
        assert!(
            rendered.contains("    \"No plan to the \\\"target\\\"\"\n"),
            "a meaning is a valid Python string literal: {rendered}"
        );
    }

    #[test]
    fn a_failure_names_its_member_when_it_refuses_a_kind() {
        let rendered = render(&MemberFailureKinds::peppy_only());
        assert!(
            rendered.contains(
                "raise ValueError(f\"{kind!r} is not a failure kind of {SERVICE_NAME}\") from None"
            ),
            "{rendered}"
        );
    }
}
