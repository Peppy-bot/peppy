//! Everything composition can refuse: bad `--with` words, copies that
//! select what their option does not declare, unreadable or unsafe fragment
//! paths, options that do not provide what their axis promises, adjustments
//! that fight, and joins that would change what already runs.
//!
//! The selection refusals are launch refusals (the words and the copies are
//! caller input); the rest surface wherever the launcher or its fragments
//! are read, which for `repo index --check` is authoring time and for a
//! launch is before anything is pinned or started.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum CompositionError {
    #[error(
        "`--with` was given but this launcher declares no `components`; it is a flat stack with \
         nothing to select"
    )]
    WithOnFlatLauncher,

    #[error("`--with {word}` names no axis or option in reach of this launch.{menu}")]
    UnknownSelection { word: String, menu: String },

    #[error(
        "`--with {word}` matches an option of more than one axis ({axes}); say which you mean \
         with the `axis=option` form"
    )]
    AmbiguousSelection { word: String, axes: String },

    #[error(
        "axis `{axis}` is selected twice with different options (`{first}`, `{second}`); an axis \
         takes one option, never two"
    )]
    ConflictingSelection {
        axis: String,
        first: String,
        second: String,
    },

    #[error(
        "axis `{axis}`{origin} has cardinality `one` and nothing selects it; deploy one of its \
         options in `deployments` as `{{ {axis}: \"<option>\" }}`, or pass `--with <option>`. \
         Its options: {options}"
    )]
    UnresolvedAxis {
        axis: String,
        /// Empty for a launcher axis, ` of <option>` for a fragment's own.
        origin: String,
        options: String,
    },

    #[error(
        "axis `{axis}` of `{option}` has cardinality `one` and copy `{copy}` selects nothing \
         for it; add `with: {{ {axis}: \"<option>\" }}` to the copy, or `--with <option>` on \
         `peppy stack join`. Its options: {options}"
    )]
    UnresolvedCopyAxis {
        copy: String,
        option: String,
        axis: String,
        options: String,
    },

    #[error(
        "`--with {word}` selects axis `{axis}`, which runs as named copies; list them under \
         `deployments` as `{{ {axis}: \"{option}\", instances: [{{ instance_id: \"NAME\" }}] }}`, \
         or add one to the running stack with `peppy stack join {option} -i NAME`"
    )]
    RepeatableAxisAtLaunch {
        word: String,
        axis: String,
        option: String,
    },

    #[error(
        "`--with {word}` selects axis `{axis}` of the copies of `{parent}`; select it per copy \
         with `with: {{ {axis}: \"{option}\" }}` under `deployments`, or on `peppy stack join \
         {parent} -i NAME --with {option}`"
    )]
    CopyAxisAtLaunch {
        word: String,
        axis: String,
        parent: String,
        option: String,
    },

    #[error(
        "axis `{axis}` is declared by both {first} and {second}, which run together; give one \
         of them another name"
    )]
    AxisInReachTwice {
        axis: String,
        first: String,
        second: String,
    },

    #[error(
        "copy `{copy}` of `{option}` selects axis `{axis}`, which `{option}` does not declare. \
         Its axes: {axes}"
    )]
    CopySelectsUnknownAxis {
        copy: String,
        option: String,
        axis: String,
        axes: String,
    },

    #[error(
        "copy `{copy}` selects `{selection}`, which that axis of `{option}` does not declare. \
         Its options: {options}"
    )]
    CopySelectsUnknownOption {
        copy: String,
        option: String,
        /// `axis: option`, as the copy wrote it.
        selection: String,
        options: String,
    },

    #[error(
        "this launcher declares no `zero_or_more` axis, so nothing can be added to its stack \
         with `stack join`"
    )]
    NoRepeatableAxis,

    #[error(
        "`{option}` is not an option of a `zero_or_more` axis of this launcher; the copies it \
         can add:{menu}"
    )]
    JoinUnknownOption { option: String, menu: String },

    #[error(
        "copy `{copy}` of `{option}` starts no node; an option that runs as copies deploys at \
         least one"
    )]
    CopyStartsNothing { copy: String, option: String },

    #[error(
        "copy `{copy}` cannot run under that name: {reason}. A copy's name is its placement \
         link, a core node name"
    )]
    CopyNameNotPlaceable { copy: String, reason: String },

    #[error(
        "`{target}.{argument}` is overridden twice; pass one --set-arguments value per argument"
    )]
    DuplicateArgumentOverride { target: String, argument: String },

    #[error("`{target}` is not an instance of copy `{copy}`; override one of {available}")]
    ArgumentTargetAbsent {
        copy: String,
        target: String,
        available: String,
    },

    #[error(
        "joining `{name}` would change `{instance}`, which already runs: {changes}. Select \
         options compatible with the running stack, or reset and launch the complete \
         configuration"
    )]
    JoinChangesExisting {
        name: String,
        instance: String,
        changes: String,
    },

    #[error(
        "copies `{first}` and `{second}` write `{target}` differently ({difference}). Copies \
         that share a stack instance must agree on it"
    )]
    CopiesConflict {
        first: String,
        second: String,
        /// `instance.arguments.key` or `instance.links.slot`.
        target: String,
        /// `alpha writes "v1", bravo writes "v2"`.
        difference: String,
    },

    #[error(
        "copy `{copy}` cannot be removed while the stack links to it ({links}); remove the copy \
         holding the link first, or relaunch without `{copy}`"
    )]
    CopyStillLinked { copy: String, links: String },

    #[error(
        "copy `{copy}` is of `{option}` on axis `{axis}`, which this launcher does not run as \
         copies; remove it through the launcher that started it"
    )]
    CopyOfAnotherLauncher {
        copy: String,
        axis: String,
        option: String,
    },

    #[error(
        "constraint in {origin} names axis `{axis}`, which runs as copies; write it in the \
         launcher's `constraints`, or in a fragment of a `{axis}` option"
    )]
    ConstraintOnCopyAxis { origin: String, axis: String },

    #[error(
        "the launcher's `constraints` entry {position} names copy axes {axes}; a copy fills one \
         axis, so a constraint names one copy axis"
    )]
    ConstraintSpansCopyAxes { position: usize, axes: String },

    #[error(
        "{origin} declares `core_nodes`, which a copy of `{axis}` cannot use; a copy is placed \
         by its name with --place NAME@CORE_NODE"
    )]
    CopyFragmentCoreNodes { origin: String, axis: String },

    #[error(
        "`--with {word}` names no option of axis `{axis}`, which runs as copies; add one with \
         `peppy stack join OPTION -i NAME`, choosing from:{menu}"
    )]
    RepeatableAxisUnknownOption {
        word: String,
        axis: String,
        menu: String,
    },

    #[error("`--with {word}` names no option of the axes `{option}` declares.{menu}")]
    UnknownCopySelection {
        word: String,
        option: String,
        menu: String,
    },

    #[error("`--with {word}` names copy `{copy}`, which the file does not deploy; {copies}")]
    ScopedSelectionUnknownCopy {
        word: String,
        copy: String,
        /// The file's copies and where a copy comes from.
        copies: String,
    },

    #[error(
        "`--with {word}` names no copy before the dot; a launch word is `option`, \
         `axis=option`, `NAME.option` or `NAME.axis=option`"
    )]
    ScopedWordNamesNoCopy { word: String },
    #[error(
        "`{word}` names copy `{copy}` and nothing after the dot; a launch word selects a file \
         copy's own axis as `NAME.axis=option` or `NAME.option`"
    )]
    ScopedWordNamesNoOption { word: String, copy: String },

    #[error(
        "{origin} is guarded on axis `{axis}`, which runs as copies; a copy's adjustments are \
         guarded on the launcher's other axes and the copy's own"
    )]
    CopyAdjustmentOnCopyAxis { origin: String, axis: String },

    #[error("{origin}: {detail}")]
    CopyAdjustmentGuard { origin: String, detail: String },

    #[error(
        "{origin} targets `{target}`, which is not an instance of the copy ({available}); a \
         write to a stack instance belongs in the option's fragment `adjustments`"
    )]
    CopyAdjustmentTarget {
        origin: String,
        target: String,
        available: String,
    },

    #[error(
        "copy `{copy}` mints `{id}`, an instance id the stack already runs; choose another name"
    )]
    PrefixedIdCollision { copy: String, id: String },

    #[error(
        "copy `{copy}` prefixes the link targets of `{instance}` onto one another: {reason}; \
         choose another name"
    )]
    PrefixedLinksCollide {
        copy: String,
        instance: String,
        reason: String,
    },

    #[error(
        "`{name}` is already a core node link, declared by the launcher or by a copy on the \
         stack; choose another name"
    )]
    NameIsCoreNodeLink { name: String },

    #[error(
        "instance `{instance}` of copy `{copy}` declares `core_node: {core_node}`; a copy is \
         placed as a whole with `--place {copy}@CORE_NODE`, so drop the instance's `core_node`"
    )]
    CopyInstancePlaced {
        copy: String,
        instance: String,
        core_node: String,
    },

    #[error(
        "the fragments of `{option}` define `{id}`, which the stack also defines; a copy's \
         instances are minted under its name, so give one of them another id"
    )]
    CopyReusesStackId { option: String, id: String },

    #[error(
        "{condition} requires {alternatives}, which this selection ({selection}) does not \
         satisfy. {reason}"
    )]
    ConstraintUnsatisfied {
        /// `selecting axis=option ...`, or the document, for an
        /// unconditional constraint: the offending choice leads the message.
        condition: String,
        /// The rendered `requires` alternatives.
        alternatives: String,
        /// The full resolved selection: the axes the operator did not name
        /// are usually the fix.
        selection: String,
        /// The author's `reason`, verbatim.
        reason: String,
    },

    #[error("{condition} forbids {matched}, which this selection ({selection}) has. {reason}")]
    ConstraintForbidden {
        condition: String,
        matched: String,
        selection: String,
        reason: String,
    },

    #[error(
        "fragment path `{path}` in {origin} is not usable: {reason}. A fragment path is \
         relative to the declaring document's directory and must stay inside its repository \
         root (or that directory for a standalone launcher); use a fragment inside that \
         repository"
    )]
    FragmentPath {
        path: String,
        origin: String,
        reason: String,
    },

    #[error("the launcher's directory cannot be used: {reason}")]
    LauncherDirectory { reason: String },

    #[error("fragment `{path}` referenced by {origin} cannot be read: {detail}")]
    FragmentUnreadable {
        path: String,
        origin: String,
        detail: String,
    },

    #[error("fragment `{path}` referenced by {origin} does not parse: {detail}")]
    FragmentInvalid {
        path: String,
        origin: String,
        detail: String,
    },

    #[error("fragment `{path}` of {origin}: {detail}")]
    FragmentReferencesUnknownAxis {
        path: String,
        origin: String,
        detail: String,
    },

    #[error(
        "fragment `{path}` referenced by {origin} declares `components`; a fragment two levels \
         below the launcher declares none"
    )]
    NestedComponents { path: String, origin: String },

    #[error(
        "option `{option}` declares axis `{axis}` in both {first} and {second}; an option's \
         fragments declare each axis once"
    )]
    AxisDeclaredTwice {
        option: String,
        axis: String,
        first: String,
        second: String,
    },

    #[error(
        "option `{option}` deploys axis `{axis}` in both {first} and {second}; an option's \
         fragments deploy each axis once"
    )]
    AxisDeployedTwice {
        option: String,
        axis: String,
        first: String,
        second: String,
    },

    #[error(
        "option `{option}` of axis `{axis}` does not define `{id}`, which the axis's `provides` \
         promises; every option defines the axis's interface ids"
    )]
    ProvidesUnmet {
        axis: String,
        option: String,
        id: String,
    },

    #[error(
        "adjustment on `{target}` in {origin} names a target no option of this launcher defines \
         anywhere; that is a dead reference or a typo"
    )]
    TargetDefinedNowhere { target: String, origin: String },

    #[error(
        "adjustment on `{target}` in {origin} is guarded on axis `{axis}`, which runs as \
         copies; a stack fragment's guard names the stack's axes, and a copy's own \
         adjustments belong in its fragment"
    )]
    GuardOnCopyAxis {
        target: String,
        origin: String,
        axis: String,
    },

    #[error(
        "instance `{id}` is defined by both {first} and {second}; each instance id belongs to \
         one origin"
    )]
    DuplicateInstanceId {
        id: String,
        first: String,
        second: String,
    },

    #[error(
        "two fragments adjust the same thing: {first} and {second} both write \
         `{target}.{field}`. Fragments refuse to fight over one value; the base specializes \
         fragments"
    )]
    AdjustmentsConflict {
        target: String,
        field: String,
        first: String,
        second: String,
    },

    #[error(
        "adjustment in {origin} cannot append to slot `{slot}` on `{target}`: the slot holds \
         {holds}. Appending is for array bindings; replacing is `set_links`'s job"
    )]
    AddLinksOnNonArray {
        origin: String,
        target: String,
        slot: String,
        holds: &'static str,
    },

    #[error(
        "adjustment in {origin} adds `{added}` to slot `{slot}` on `{target}`, which already \
         binds it: a slot's bound set lists each producer once"
    )]
    AddLinksDuplicateTarget {
        origin: String,
        target: String,
        slot: String,
        added: String,
    },

    #[error("the flattened launcher does not validate: {0}")]
    FlatValidation(#[from] crate::error::Error),
}
