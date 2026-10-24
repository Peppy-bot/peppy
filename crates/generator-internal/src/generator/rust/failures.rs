//! The failure kinds of a member in generated Rust: the `FailureKind` enum
//! and the `Failure` struct that each action and service module declares.
//!
//! A member gives a kind from a closed set: the kinds of peppy and the kinds
//! of its contract that it lists. The enum holds that set, so generated
//! provider code can give no other kind, and generated client code parses
//! each failure from the wire into it.

use crate::generator::naming::to_camel_case;
use config::failure_kind::MemberFailureKinds;
use proc_macro2::{Ident, Literal, Span, TokenStream};
use quote::quote;

/// Whether the member is an action or a service. A service module also
/// reads its failure from the error of a call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum FailingMember {
    Action,
    Service,
}

/// The `FailureKind` variant of the kind `name`: its name in camel case.
/// `Self` is the one keyword a kind name gives in camel case, so the kind
/// `self` gives `Self_`.
fn failure_kind_variant(name: &str) -> String {
    let variant = to_camel_case(name);
    if variant == "Self" {
        return "Self_".to_string();
    }
    variant
}

/// Builds the `FailureKind` enum and the `Failure` struct of a member that
/// gives `kinds`. `member_name` names the constant that holds the name of
/// the member in the module, for the error of a failure kind the member does
/// not declare.
pub(super) fn build_failure_items(
    kinds: &MemberFailureKinds,
    member_name: &TokenStream,
    member: FailingMember,
) -> TokenStream {
    let mut variants = Vec::new();
    let mut variant_docs = Vec::new();
    let mut names = Vec::new();
    for (name, meaning) in kinds.iter() {
        let variant = failure_kind_variant(name);
        variants.push(Ident::new(&variant, Span::call_site()));
        variant_docs.push(Literal::string(&format!(" {meaning}")));
        names.push(Literal::string(name));
    }
    let kind_count = Literal::usize_unsuffixed(variants.len());

    let service_methods = match member {
        FailingMember::Action => TokenStream::new(),
        FailingMember::Service => quote! {
            /// The failure of the error reply that `error` holds: `None`
            /// when `error` is not an error reply of a provider, or when
            /// its kind is not a kind of this member.
            pub fn of(error: &crate::Error) -> Option<Self> {
                let crate::Error::ServiceFailed { failure, .. } = error else {
                    return None;
                };
                Self::from_wire(failure).ok()
            }

            /// `error` itself, unless it is an error reply with a kind this
            /// member does not declare: such a reply breaks the contract of
            /// the member, and gives a deserialization error instead.
            pub(crate) fn reject_undeclared_kind(error: crate::Error) -> crate::Error {
                let crate::Error::ServiceFailed { failure, .. } = &error else {
                    return error;
                };
                match Self::from_wire(failure) {
                    Ok(_) => error,
                    Err(undeclared) => undeclared,
                }
            }
        },
    };

    quote! {
        /// A failure kind of this member: a kind of peppy, or a kind of its
        /// contract that it lists.
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        #[allow(dead_code)]
        pub enum FailureKind {
            #(
                #[doc = #variant_docs]
                #variants,
            )*
        }

        #[allow(dead_code)]
        impl FailureKind {
            /// Every kind of this member: the kinds of peppy, then the
            /// kinds of its contract that it lists.
            pub const ALL: [Self; #kind_count] = [#(Self::#variants),*];

            /// The name of the kind on the wire.
            pub const fn as_str(self) -> &'static str {
                match self {
                    #(Self::#variants => #names,)*
                }
            }

            /// The kind named `name`, or `None` when this member does not
            /// declare it.
            pub fn parse(name: &str) -> Option<Self> {
                Self::ALL.into_iter().find(|kind| kind.as_str() == name)
            }
        }

        impl std::fmt::Display for FailureKind {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(self.as_str())
            }
        }

        /// Why this member refused or failed a goal, or gave an error
        /// reply: a kind that it declares, and a message for a person.
        #[derive(Debug, Clone, PartialEq, Eq)]
        #[allow(dead_code)]
        pub struct Failure {
            pub kind: FailureKind,
            pub message: String,
        }

        #[allow(dead_code)]
        impl Failure {
            pub fn new(kind: FailureKind, message: impl Into<String>) -> Self {
                Self {
                    kind,
                    message: message.into(),
                }
            }

            /// The wire form of the failure.
            pub(crate) fn to_wire(&self) -> peppylib::messaging::Failure {
                let kind = peppylib::messaging::FailureKindName::parse(self.kind.as_str())
                    .expect("each kind of a member is a valid failure kind name");
                peppylib::messaging::Failure::new(kind, self.message.clone())
            }

            /// Parses a failure from the wire. A kind that this member does
            /// not declare is a deserialization error, whose text keeps the
            /// message of the provider.
            pub(crate) fn from_wire(failure: &peppylib::messaging::Failure) -> crate::Result<Self> {
                let kind = FailureKind::parse(failure.kind().as_str()).ok_or_else(|| {
                    crate::Error::Deserialization(format!(
                        "the failure kind `{}` is not a kind of `{}` (message: {:?})",
                        failure.kind(),
                        #member_name,
                        failure.message()
                    ))
                })?;
                Ok(Self::new(kind, failure.message()))
            }

            #service_methods
        }

        impl std::fmt::Display for Failure {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "{}: {}", self.kind, self.message)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::generator::rust::type_mapping::render_tokens;
    use config::failure_kind::{ContractFailureKinds, FailureKindName};

    fn contract_kinds(kinds: &[(&str, &str)]) -> MemberFailureKinds {
        MemberFailureKinds::of_member(
            ContractFailureKinds::new(kinds.iter().map(|(name, meaning)| {
                (FailureKindName::parse(name).unwrap(), meaning.to_string())
            }))
            .unwrap(),
        )
    }

    fn render(kinds: &MemberFailureKinds, member: FailingMember) -> String {
        render_tokens(build_failure_items(kinds, &quote!(SERVICE_NAME), member))
    }

    #[test]
    fn a_native_member_declares_the_kinds_of_peppy_only() {
        let rendered = render(&MemberFailureKinds::peppy_only(), FailingMember::Action);
        assert!(
            rendered.contains(
                "pub const ALL: [Self; 5] = [\n        Self::AttemptFailed,\n        Self::Timeout,\n        Self::PreconditionFailed,\n        Self::ResourceUnavailable,\n        Self::Rejected,\n    ];"
            ),
            "{rendered}"
        );
        assert!(rendered.contains("/// The attempt gave no result\n    AttemptFailed,"));
        assert!(rendered.contains("Self::Rejected => \"rejected\","));
    }

    #[test]
    fn a_contract_member_declares_its_kinds_after_the_kinds_of_peppy() {
        let rendered = render(
            &contract_kinds(&[("unreachable", "No plan to the target")]),
            FailingMember::Action,
        );
        assert!(
            rendered.contains("Self::Rejected,\n        Self::Unreachable,\n    ];"),
            "{rendered}"
        );
        assert!(rendered.contains("/// No plan to the target\n    Unreachable,"));
        assert!(rendered.contains("Self::Unreachable => \"unreachable\","));
    }

    #[test]
    fn only_a_service_reads_its_failure_from_an_error() {
        let action = render(&MemberFailureKinds::peppy_only(), FailingMember::Action);
        assert!(!action.contains("pub fn of("));
        let service = render(&MemberFailureKinds::peppy_only(), FailingMember::Service);
        assert!(service.contains("pub fn of(error: &crate::Error) -> Option<Self>"));
        assert!(service.contains("pub(crate) fn reject_undeclared_kind("));
    }

    #[test]
    fn an_undeclared_kind_keeps_the_message_of_the_provider() {
        let rendered = render(&MemberFailureKinds::peppy_only(), FailingMember::Action);
        assert!(
            rendered.contains("\"the failure kind `{}` is not a kind of `{}` (message: {:?})\""),
            "{rendered}"
        );
        // Once in the error of an undeclared kind, once in the failure.
        assert_eq!(
            rendered.matches("failure.message()").count(),
            2,
            "the error of an undeclared kind gives the message of the provider: {rendered}"
        );
    }

    #[test]
    fn the_kind_self_gives_a_variant_that_is_not_a_keyword() {
        let rendered = render(
            &contract_kinds(&[("self", "The robot hit itself")]),
            FailingMember::Action,
        );
        assert!(rendered.contains("Self::Self_ => \"self\","), "{rendered}");
    }
}
