//! The launcher's adjustments as one list, each with the option it sits
//! under, and which units an entry reaches: the stack, or the copies of
//! one axis.

use super::super::composition::{Adjustment, ComponentAxis, OriginatedAdjustment, option_origin};
use super::super::types::PeppyLauncher;
use super::constraints::names_axis;
use super::load::LoadedComposition;
use super::select::UnitSelection;
use std::collections::{HashMap, HashSet};

/// One of the launcher's adjustments as it runs, with the origin the
/// report names it by and, for an option's adjustment, the option it sits
/// under: selecting that option is what brings the entry into a unit.
pub(super) struct LauncherAdjustment<'a> {
    pub(super) adjustment: &'a Adjustment,
    pub(super) origin: String,
    /// The axis and option this adjustment sits under; `None` for the
    /// launcher's top-level list.
    pub(super) under: Option<(&'a str, &'a str)>,
}

impl<'a> LauncherAdjustment<'a> {
    /// Whether this entry is part of a unit resolved to `selection`: the
    /// launcher's own entries always are, an option's when the unit
    /// selected that option on its axis.
    pub(super) fn selected_in(&self, selection: &UnitSelection) -> bool {
        self.under
            .is_none_or(|(axis, option)| selection.option_of(axis) == Some(option))
    }

    /// Whether this entry speaks about `axis`: the option it sits under
    /// fills it, or its guard names it.
    pub(super) fn names_axis(&self, axis: &str) -> bool {
        self.under.is_some_and(|(under, _)| under == axis)
            || names_axis(self.adjustment.when.as_ref(), axis)
    }

    /// The options of `axis` this entry runs under: the option it sits
    /// under when that option fills `axis`, otherwise the ones its guard
    /// names.
    pub(super) fn options_named_on(&self, axis: &str) -> Vec<&'a str> {
        match self.under {
            Some((under, option)) if under == axis => vec![option],
            _ => self
                .adjustment
                .when
                .as_ref()
                .and_then(|when| when.get(axis))
                .map(|options| options.iter().collect())
                .unwrap_or_default(),
        }
    }

    /// Whether this entry is part of a copy of `axis` that defines
    /// `defines`, `stack_ids` being what can run in the stack beside the
    /// entry: the option it sits under fills the axis or its guard names
    /// it, or, when it names no copy axis at all, it writes an instance the
    /// copy defines and the stack cannot.
    pub(super) fn reaches_copy(
        &self,
        axis: &str,
        defines: &HashSet<&str>,
        launcher: &PeppyLauncher,
        stack_ids: &HashSet<&str>,
    ) -> bool {
        if launcher
            .repeatable_axes()
            .any(|copy_axis| self.names_axis(&copy_axis.name))
        {
            return self.names_axis(axis);
        }
        let target = self.adjustment.target.as_str();
        defines.contains(target) && !stack_ids.contains(target)
    }

    /// The entry as a unit plans it: the adjustment and its origin.
    pub(super) fn originated(&self) -> OriginatedAdjustment<'a> {
        OriginatedAdjustment {
            adjustment: self.adjustment,
            origin: self.origin.clone(),
        }
    }
}

/// One launcher adjustment with what deciding its units needs: the ids that
/// can run in the stack beside it, and the axes whose copies run it. The id
/// sets within it are built once per option.
pub(super) struct Routed<'a> {
    pub(super) entry: LauncherAdjustment<'a>,
    pub(super) stack_ids: HashSet<&'a str>,
    /// Empty when the stack runs the entry itself.
    pub(super) copy_axes: Vec<&'a str>,
}

/// Every adjustment the launcher writes, routed: which copies run it, and
/// the ids in reach where it sits, with the copy axes the routing read.
pub(super) fn route<'a>(
    launcher: &'a PeppyLauncher,
    loaded: &'a LoadedComposition,
    launcher_label: &str,
) -> (Vec<CopyAxis<'a>>, Vec<Routed<'a>>) {
    let copy_axes = copy_axes(launcher, loaded);
    let mut beside: HashMap<Option<(&str, &str)>, HashSet<&str>> = HashMap::new();
    let routed = launcher_adjustments(launcher, launcher_label)
        .into_iter()
        .map(|entry| {
            let stack_ids = beside
                .entry(entry.under)
                .or_insert_with(|| ids_beside(launcher, loaded, entry.under, launcher.stack_axes()))
                .clone();
            let reached = copy_axes_of(&entry, launcher, &stack_ids, &copy_axes)
                .into_iter()
                .map(|axis| axis.name)
                .collect();
            Routed {
                entry,
                stack_ids,
                copy_axes: reached,
            }
        })
        .collect();
    (copy_axes, routed)
}

/// One copy axis of a launcher, with every instance id its options can
/// define.
pub(super) struct CopyAxis<'a> {
    pub(super) name: &'a str,
    pub(super) defines: HashSet<&'a str>,
}

/// The launcher's copy axes with the ids their options define.
fn copy_axes<'a>(launcher: &'a PeppyLauncher, loaded: &'a LoadedComposition) -> Vec<CopyAxis<'a>> {
    launcher
        .repeatable_axes()
        .map(|axis| CopyAxis {
            name: axis.name.as_str(),
            defines: loaded
                .options_of(&axis.name)
                .flat_map(|(_, option)| option.definable_ids())
                .collect(),
        })
        .collect()
}

/// The ids that can run beside an entry: the launcher's own `deployments`
/// and every option of `axes`, less the siblings of the option the entry
/// sits under, which never run beside it.
pub(super) fn ids_beside<'a>(
    launcher: &'a PeppyLauncher,
    loaded: &'a LoadedComposition,
    under: Option<(&str, &str)>,
    axes: impl Iterator<Item = &'a ComponentAxis>,
) -> HashSet<&'a str> {
    launcher
        .deployments
        .iter()
        .flat_map(|deployment| &deployment.instances)
        .map(|instance| instance.instance_id.as_str())
        .chain(
            axes.flat_map(|axis| loaded.options_of(&axis.name).map(move |o| (axis, o)))
                .filter(|(axis, (option, _))| {
                    under.is_none_or(|(under_axis, under_option)| {
                        axis.name != under_axis || option.as_str() == under_option
                    })
                })
                .flat_map(|(_, (_, option))| option.definable_ids()),
        )
        .collect()
}

/// The copy axes that reach one launcher adjustment, `stack_ids` being
/// what can run in the stack beside it. Each copy of those axes runs the
/// adjustment, so the stack does not; empty when the stack runs it.
fn copy_axes_of<'a, 'c>(
    entry: &LauncherAdjustment<'_>,
    launcher: &PeppyLauncher,
    stack_ids: &HashSet<&str>,
    copy_axes: &'c [CopyAxis<'a>],
) -> Vec<&'c CopyAxis<'a>> {
    copy_axes
        .iter()
        .filter(|axis| entry.reaches_copy(axis.name, &axis.defines, launcher, stack_ids))
        .collect()
}

/// The launcher's adjustments in the order they apply: each option's under
/// its axis, axes in declaration order, then the launcher's own list. A
/// unit selects one option per axis, so options of one axis never
/// interleave.
fn launcher_adjustments<'a>(
    launcher: &'a PeppyLauncher,
    launcher_label: &str,
) -> Vec<LauncherAdjustment<'a>> {
    let under_options = launcher.components.iter().flat_map(|axis| {
        axis.options.iter().flat_map(move |(option, spec)| {
            spec.adjustments
                .iter()
                .map(move |adjustment| LauncherAdjustment {
                    adjustment,
                    origin: format!("{launcher_label}, {}", option_origin(&axis.name, option)),
                    under: Some((axis.name.as_str(), option.as_str())),
                })
        })
    });
    let own = launcher
        .adjustments
        .iter()
        .map(|adjustment| LauncherAdjustment {
            adjustment,
            origin: format!("{launcher_label}, top-level `adjustments`"),
            under: None,
        });
    under_options.chain(own).collect()
}
