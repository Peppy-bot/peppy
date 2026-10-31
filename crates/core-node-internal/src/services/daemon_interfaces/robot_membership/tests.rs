mod provider;

use super::*;
use core_node_api::encoding::AxisSelection;

fn scope() -> RobotMembershipScope {
    serde_json::from_value(serde_json::json!({
        "max_copies": 2,
        "options": [
            { "option": "openarm_sim", "model": "openarm_v2", "description": "An OpenArm.",
              "features": [
                { "feature": "ai_brain", "axis": "brain", "option": "ai_brain_vla",
                  "description": "A brain." }
              ] },
            { "option": "so101_sim", "model": "so101", "description": "An SO-101." }
        ]
    }))
    .expect("the scope parses")
}

fn input(name: &str, option: &str, core_node: &str, features: &[(&str, &str)]) -> JoinInput {
    JoinInput {
        name: name.to_owned(),
        option: option.to_owned(),
        core_node: core_node.to_owned(),
        features: features
            .iter()
            .map(|(feature, core_node)| FeatureInput {
                feature: (*feature).to_owned(),
                core_node: (*core_node).to_owned(),
            })
            .collect(),
    }
}

fn copy(name: &str, option: &str) -> CopyInfo {
    CopyInfo {
        name: CopyName::new(name).unwrap(),
        core_node: CoreNodeName::new("cn-one").unwrap(),
        instance_ids: Vec::new(),
        selections: Vec::new(),
        option: option.to_owned(),
        set_members: Vec::new(),
    }
}

#[test]
fn a_join_names_its_machine_as_the_coordinator_or_by_name_and_places_each_feature() {
    let (copy, words) = typed_join(
        &scope(),
        "cn-one",
        &input("bravo", "openarm_sim", "cn-one", &[("ai_brain", "cn-gpu")]),
    )
    .expect("the join is typed");
    assert_eq!(copy.core_node, CoreNodeTarget::Coordinator);
    assert_eq!(copy.name.as_str(), "bravo");
    assert_eq!(words.len(), 1);
    assert_eq!(
        words[0].core_node,
        Some(CoreNodeTarget::Named(CoreNodeName::new("cn-gpu").unwrap()))
    );
    let (copy, words) = typed_join(
        &scope(),
        "cn-one",
        &input("bravo", "openarm_sim", "cn-two", &[("ai_brain", "cn-two")]),
    )
    .unwrap();
    assert_eq!(
        copy.core_node,
        CoreNodeTarget::Named(CoreNodeName::new("cn-two").unwrap())
    );
    assert_eq!(
        words[0].core_node, None,
        "a feature on the copy's machine names none"
    );
}

#[test]
fn a_join_outside_the_scope_is_refused_with_its_reason() {
    for (join, expected) in [
        (
            input("bravo", "openarm", "cn-one", &[]),
            "`openarm` is not an option of this scope; it adds copies of options openarm_sim and \
             so101_sim",
        ),
        (
            input("bravo", "so101_sim", "cn-one", &[("ai_brain", "cn-one")]),
            "`ai_brain` is not a feature of option `so101_sim`, which has no feature",
        ),
        (
            input(
                "bravo",
                "openarm_sim",
                "cn-one",
                &[("ai_brain", "cn-one"), ("ai_brain", "cn-one")],
            ),
            "`features` names feature `ai_brain` twice; name each feature once",
        ),
    ] {
        assert_eq!(typed_join(&scope(), "cn-one", &join).unwrap_err(), expected);
    }
}

#[test]
fn a_join_under_a_name_in_use_or_past_the_limit_is_refused() {
    let copies = [copy("alpha", "openarm_sim"), copy("delta", "so101_sim")];
    assert_eq!(
        refuse_join(&scope(), &copies, &CopyName::new("alpha").unwrap()).unwrap_err(),
        "a copy `alpha` is already on the stack; choose another name"
    );
    assert_eq!(
        refuse_join(&scope(), &copies, &CopyName::new("bravo").unwrap()).unwrap_err(),
        "the stack holds 2 copies of the options openarm_sim and so101_sim (alpha, delta), the \
         most the scope allows; remove one first"
    );
    refuse_join(&scope(), &copies[..1], &CopyName::new("bravo").unwrap())
        .expect("one copy is under the limit");
}

#[test]
fn a_removal_names_a_copy_of_a_scoped_option() {
    let copies = [copy("alpha", "openarm_sim"), copy("tool", "camera")];
    refuse_removal(&scope(), &copies, &CopyName::new("alpha").unwrap()).unwrap();
    assert_eq!(
        refuse_removal(&scope(), &copies, &CopyName::new("tool").unwrap()).unwrap_err(),
        "no copy `tool` of the options openarm_sim and so101_sim on the stack"
    );
}

#[test]
fn a_listed_copy_gives_its_features_on_their_machines() {
    let mut joined = copy("bravo", "openarm_sim");
    joined.selections = vec![AxisSelection {
        axis: Name::new("brain").unwrap(),
        option: Name::new("ai_brain_vla").unwrap(),
        core_node: Some(CoreNodeName::new("cn-gpu").unwrap()),
    }];
    let listed = listed_copy(&scope(), &joined, true);
    assert_eq!(listed.copy.features.len(), 1);
    assert_eq!(listed.copy.features[0].feature, "ai_brain");
    assert_eq!(listed.copy.features[0].core_node, "cn-gpu");
    assert!(listed.running);
}

#[test]
fn the_messages_of_a_change_name_it() {
    let addition = Change::Addition {
        name: CopyName::new("bravo").unwrap(),
        option: Name::new("so101_sim").unwrap(),
    };
    assert_eq!(
        addition.accepted(),
        "the daemon accepted the addition of bravo (so101_sim)"
    );
    assert_eq!(
        addition.failed("it exited during setup.", &[Path::new("/logs/join.log")]),
        "the addition of bravo failed: it exited during setup. Log files: /logs/join.log"
    );
    let refused = addition.refused(&DaemonFailure::new(
        DaemonFailureKind::Leased,
        "robot `bravo` is leased by `run` on `cn-one` under the name `tidy`",
    ));
    assert_eq!(refused.kind().as_str(), "leased");
    assert_eq!(
        refused.message(),
        "the daemon refused the addition of bravo: robot `bravo` is leased by `run` on `cn-one` \
         under the name `tidy`"
    );
}

/// The sheet of an option as `list` answers it: each part that the sheet
/// gives, and an empty value for each part that it does not give.
#[test]
fn the_listed_product_gives_each_part_of_the_sheet_and_empty_values_for_the_others() {
    let scope: RobotMembershipScope = serde_json::from_value(serde_json::json!({
        "max_copies": 3,
        "options": [
            { "option": "openarm_sim", "model": "openarm_v2", "description": "An OpenArm.",
              "product": {
                "vendor": "Enactic",
                "purchase_url": "https://openarm.dev/buy",
                "price": { "amount": 6500, "currency": "USD", "checked_on": "2026-10-31" },
                "availability": { "state": "preorder", "checked_on": "2026-10-30" },
                "mass_kg": 5.5,
                "height_m": 1.2,
              } },
            { "option": "so101_sim", "model": "so101", "description": "An SO-101.",
              "product": { "vendor": "A seller", "mass_kg": 1.4 } },
            { "option": "lekiwi_sim", "model": "lekiwi", "description": "A LeKiwi." }
        ]
    }))
    .expect("the scope parses");
    let listed: Vec<serde_json::Value> = scope
        .options()
        .iter()
        .map(|entry| serde_json::to_value(ListedProduct::from(entry.product())).unwrap())
        .collect();
    assert_eq!(
        listed,
        [
            serde_json::json!({
                "vendor": "Enactic",
                "purchase_url": "https://openarm.dev/buy",
                "price": { "amount": 6500.0, "currency": "USD", "checked_on": "2026-10-31" },
                "availability": { "state": "preorder", "checked_on": "2026-10-30" },
                "mass_kg": 5.5,
                "height_m": 1.2,
            }),
            serde_json::json!({
                "vendor": "A seller",
                "purchase_url": "",
                "price": { "amount": 0.0, "currency": "", "checked_on": "" },
                "availability": { "state": "", "checked_on": "" },
                "mass_kg": 1.4,
                "height_m": 0.0,
            }),
            serde_json::json!({
                "vendor": "",
                "purchase_url": "",
                "price": { "amount": 0.0, "currency": "", "checked_on": "" },
                "availability": { "state": "", "checked_on": "" },
                "mass_kg": 0.0,
                "height_m": 0.0,
            }),
        ]
    );
}
