@0xa8f5c2b9e4d3f1aa;

# Node message structures for core-node services

# Feedback stream tag carried by NodeAdd/NodeBuild/NodeRun feedback messages.
enum FeedbackStream {
    stdout @0;
    stderr @1;
    # Out-of-band warning emitted by the daemon itself.
    warning @2;
}

# Stack List service
struct StackListRequest {}

struct StackListResponse {
    # JSON-serialized graph representation (SerializedNodeGraph)
    graphJson @0 :Text;
    # Hostname of the daemon serving this stack
    hostName @1 :Text;
    # Presence identity of the serving daemon: its core-node name and
    # daemon-generation instance id, matching its core-node presence token.
    coreNode @2 :Text;
    instanceId @3 :Text;
    # The launch this daemon's slice belongs to, making the slice
    # self-describing. Carrying it here is what lets a coordinator rediscover
    # its participants by query instead of remembering them, so a coordinator
    # restart is not a catastrophe and `stack reset` works from any machine.
    #
    # A union rather than two optional fields: a launch id with no coordinator
    # names a launch nobody can reset, and a coordinator with no launch id
    # names nothing at all. Neither half is meaningful alone, so the wire does
    # not let them be sent apart and no decoder has to police it.
    launch :union {
        # This stack was not started by a federated launch.
        standalone @4 :Void;
        identity @5 :LaunchIdentity;
    }
    # The reservation currently held over this machine: the launch it guards
    # and the coordinator driving it. Distinct from `launch`, which describes
    # the stack the LAST launch left behind; the reservation describes the
    # launch holding the machine RIGHT NOW. Carrying it here is what makes a
    # held machine visible to `stack list` and discoverable by
    # `stack reset --federated`, including one whose launch died before
    # populating a slice.
    reservation :union {
        # No launch holds this machine.
        none @6 :Void;
        identity @7 :LaunchIdentity;
    }
    # The copies the serving coordinator runs.
    copies @8 :List(CopyInfo);
    # Configured cooperative shutdown grace, used to budget destructive requests.
    shutdownGraceSecs @9 :UInt64;
    # The change of one copy that holds this daemon's stack now: the join of
    # a copy, which `copies` lists only once the join ends, or the removal of
    # a copy. Any other stack change reports none.
    copyChange :union {
        none @10 :Void;
        join @11 :ChangedCopy;
        remove @12 :ChangedCopy;
    }
    # The copies the launch of this stack started, each as the launch started
    # it, in name order. A join or a removal leaves them as they are; a
    # reset, the next launch and the end of the daemon drop them.
    launchCopies @13 :List(CopyInfo);
}

# The copy that a join adds or that a removal removes.
struct ChangedCopy {
    name @0 :Text;
    option @1 :Text;
}

# One copy on the stack: a named instance of an option of an axis that runs
# as copies, the machine of its base, the instances it minted, and how its
# own axes are filled.
struct CopyInfo {
    name @0 :Text;
    # The copy's machine: the machine its name places, which runs its base.
    coreNode @1 :Text;
    instanceIds @2 :List(Text);
    # Each filled own axis of the copy, in the order the copied option
    # declares its axes.
    selections @3 :List(AxisSelection);
    option @4 :Text;
    # The members the copy added to stack instances' set slots, in the order it
    # added them.
    setMembers @5 :List(SetMember);
}

# One filled own axis of a copy: its option, and the machine of the part it
# selects when that part runs on another machine than the copy's.
struct AxisSelection {
    axis @0 :Text;
    option @1 :Text;
    coreNode :union {
        withCopy @2 :Void;
        named @3 :Text;
    }
}

# One member a copy added to a stack instance's set slot.
struct SetMember {
    # The stack instance whose slot the member joined.
    instanceId @0 :Text;
    linkId @1 :Text;
    # The instance that joined the set, `instance` or `instance/link_id`, by the
    # id the stack runs it under.
    target @2 :Text;
}

# Which launch a stack slice belongs to, and who drove it.
struct LaunchIdentity {
    launchId @0 :Text;
    # The coordinator that drove the launch. A participant watches this core
    # node's presence for as long as it holds the slice.
    coordinatorCoreNode @1 :Text;
}

# Node Add Action (streaming version with feedback)
struct NodeAddGoal {
    # Git commit hash of the node being added
    gitHash @0 :Text;
    # Source of the node (filesystem path, git repository, HTTP URL, a
    # launch-pinned entry, or a `name:tag` the receiving daemon resolves).
    source :union {
        # Filesystem path to the node directory
        fs @1 :Text;
        # Git repository source
        git @2 :NodeAddGitSource;
        # HTTP URL source
        http @3 :Text;
        # JSON5-encoded pin for the root node of a pinned batch: the exact
        # manifest bytes to run, identified by content fingerprint, plus the
        # git location any machine can fetch them from. The daemon
        # materializes exactly these bytes (reusing its own copy on a
        # content match, fetching the pinned commit otherwise) and never
        # resolves the name against its own cache. The pin model lives in
        # peppy; it is opaque text here for the same reason
        # `ParticipantReserveRequest.deploymentPinsJson5` is.
        pinned @8 :Text;
        # Resolve a `name:tag` against the RECEIVING daemon's own repo cache
        # (`~/.peppy/cache/nodes.json5`), then continue exactly as a pinned
        # batch: that daemon resolves the transitive closure once, pins every
        # entry, and adds by pin. The entry arm for `peppy node add
        # <name>:<tag>`: the receiving daemon is the coordinator of its own
        # add.
        resolveRef @10 :NodeAddResolveRefSource;
        # The pins of the `mcp_exposure` documents the built-in MCP server
        # serves, one JSON5 pin per exposure, encoded the way `pinsJson5`
        # carries the contract pins they reference. The daemon materializes
        # exactly these bytes, derives the server's manifest from them, and
        # registers the built-in node ready to start: nothing is fetched for
        # a node, generated or built. Opaque text here for the same reason
        # `pinned` is.
        exposures @12 :List(Text);
    }
    # Optional SHA256 checksum for HTTP sources
    httpSha256 @6 :Text;
    # Environment variables to apply when executing build_cmd (e.g. PATH)
    envVars @4 :List(EnvVar);
    # Timeout in seconds for the add operation (used to report remaining time when busy)
    timeoutSecs @5 :UInt64;
    # When true, cancel any in-progress add action and start a new one
    force @7 :Bool;
    # The federated launch this goal is one step of, when a coordinator
    # dispatched it. Empty for anything a user typed.
    #
    # A daemon reserved by a launch refuses node work that does not name that
    # launch, which is what stops a local `peppy node add` from racing a
    # coordinator that is halfway through replacing this machine's slice.
    launchId @9 :Text;
    # JSON5-encoded pins for the rest of a pinned batch: every transitive
    # node dependency of the `pinned` root, and every contract and pairing
    # document any manifest in the batch names. Populated exactly when the
    # source is `pinned` or `exposures` (for the latter: the contract pins
    # the exposures reference). The daemon refuses a batch whose closure
    # names an entry missing from this list rather than resolving it by name.
    pinsJson5 @11 :List(Text);
}

struct NodeAddResolveRefSource {
    # Node name as it appears in `nodes.json5`
    name @0 :Text;
    # Node tag as it appears in `nodes.json5`
    tag @1 :Text;
}

struct EnvVar {
    key @0 :Text;
    value @1 :Text;
}

struct NodeAddGitSource {
    # URL of the git repository
    repoUrl @0 :Text;
    # Path within the repository to the node
    repoPath @1 :Text;
    # Optional git ref (tag/branch/commit) to checkout before reading repoPath
    repoRef @2 :Text;
}

# The goal response of an accepted node add. The daemon refuses a goal in
# the goal envelope, with a failure kind and a message.
struct NodeAddGoalResponse {
    # Path to the log file of the add, on the daemon's machine.
    logPath @0 :Text;
}

struct NodeAddFeedback {
    stream @0 :FeedbackStream;
    # The line of output
    line @1 :Text;
}

# The result of a completed node add. A failed add gives a failure kind
# and a message, and no result; a cancelled add gives its reason, and no
# result.
struct NodeAddResult {
    # Name of the added node
    nodeName @0 :Text;
    # Tag of the added node
    nodeTag @1 :Text;
}

# Node Build Action: drives the build of a previously-added node
struct NodeBuildGoal {
    nodeName @0 :Text;
    nodeTag @1 :Text;
    envVars @2 :List(EnvVar);
    timeoutSecs @3 :UInt64;
    force @4 :Bool;
    # See `NodeAddGoal.launchId`.
    launchId @5 :Text;
    # Build from the staged sources even when storage already holds an
    # artifact built from byte-identical sources. Absent decodes as false,
    # which reuses such an artifact. Unrelated to `force`, which only cancels
    # an in-flight build of the same node.
    rebuild @6 :Bool;
}

# The goal response of an accepted node build. The daemon refuses a goal
# in the goal envelope, with a failure kind and a message.
struct NodeBuildGoalResponse {
    # Path to the log file of the build, on the daemon's machine.
    logPath @0 :Text;
}

struct NodeBuildFeedback {
    stream @0 :FeedbackStream;
    line @1 :Text;
}

# The result of a completed node build. A failed build gives a failure kind
# and a message, and no result; a cancelled build gives its reason, and no
# result.
struct NodeBuildResult {
    # Path to the resulting .sif/archive in storage
    artifactPath @0 :Text;
}

# Node Init service
struct NodeInitRequest {
    # Root directory where the node will be created
    nodeRootDir @0 :Text;
    # Name of the node (used for directory and package name)
    nodeName @1 :Text;
    # Git commit hash of the node being initialized
    gitHash @2 :Text;
    # Toolchain to use for the node ("cargo" or "uv")
    toolchain @3 :Text;
    # Whether to initialize the node with container support
    withContainer @4 :Bool;
    # What the node does: the first paragraph of the description of its
    # manifest. Not blank.
    description @5 :Text;
}

# The response of a node init that succeeded. A failed init is an error
# reply, with a failure kind and a message.
struct NodeInitResponse {
}

# Node Sync service
struct NodeSyncRequest {
    # Root directory of the node/workspace
    nodeRootDir @0 :Text;
    # Git commit hash of the node being synced
    gitHash @1 :Text;
    # When true, dependencies missing from the persistent node stack are
    # looked up in `~/.peppy/cache/nodes.json5` and materialized through
    # the existing FS / git / HTTP repository cache before peppygen
    # generation proceeds. Resolution still prefers the node stack; the
    # repository cache is consulted only as a fallback.
    includeRepositories @2 :Bool;
}

# The response of a node sync that succeeded. A failed sync is an error
# reply, with a failure kind and a message.
struct NodeSyncResponse {
    # `name:tag` of every dependency the daemon resolved through the
    # persistent node stack.
    resolvedFromStack @0 :List(Text);
    # Every dependency the daemon resolved by fetching from the repository
    # cache. Only populated when the request set `includeRepositories`.
    resolvedFromRepositories @1 :List(RepoResolvedEntry);
}

struct RepoResolvedEntry {
    name @0 :Text;
    tag @1 :Text;
    # "fs" | "git"
    sourceKind @2 :Text;
}

struct NodeRunGoal {
    # JSON5-encoded `NodeInstancePlan`: instance id, arguments, the
    # clock binding, and the resolved slot bindings.
    #
    # Deliberately a PLAN and not an assembled runtime config. A daemon owns
    # the runtime identity of every node it spawns, so the messaging
    # endpoint, `bound_core_node`, and the resolved framework values are
    # supplied by the receiving daemon on every path, this one included. A
    # requester-assembled config would name the requester's own router, which
    # a node on another machine cannot reach.
    #
    # Never empty, and its `instanceId` is never empty: a goal without a plan
    # comes from a peppy that predates federated launch and is refused at
    # decode rather than starting a node under a defaulted identity.
    instancePlanJson5 @0 :Text;
    # Name of the node to run
    nodeName @1 :Text;
    # Tag of the node to run
    tag @2 :Text;
    # Environment variables to apply when executing run_cmd (e.g. PATH)
    envVars @3 :List(EnvVar);
    # Timeout in seconds for the run operation (used to report remaining time when busy)
    timeoutSecs @4 :UInt64;
    # Pairing requests from `--link <link_id>@<peer_instance>[/<peer_link_id>]`
    # or a launch plan: commands to the daemon, not resolved config. The
    # daemon validates and reserves each pair BEFORE spawning and delivers it
    # live after the instance commits to Running. One entry per pair, so a
    # multi-cardinality slot contributes several entries sharing one linkId,
    # in the order the plan lists them.
    requestedPairs @5 :List(PairRequest);
    # Pairing slots deliberately left unpaired via `--vacant-link
    # <link_id>=<why>` / the launcher's `links: { <link_id>: { vacant:
    # "<why>" } }`, each carrying the reason the deployment wrote down. Legal
    # only on a slot the manifest declares `cardinality: "zero_or_one"`; the
    # daemon re-checks that against its own copy of the manifest. Together with
    # requestedPairs and coveredPairs these must cover every pairing slot of
    # the manifest or the daemon rejects the run.
    vacantPairs @6 :List(VacantPair);
    # Pairing slots of this instance that a LATER-starting instance of the
    # same `stack launch` will claim through its own requestedPairs entry;
    # each entry names that future peer. A launch-mechanism marker, not user
    # intent: the slot boots unpaired and needs no action, unlike a
    # vacantPairs entry which records a deliberate opt-out. Never set by
    # the CLI. One entry per pair, like requestedPairs.
    coveredPairs @7 :List(PairRequest);
    # Observer requests from `--link <observer_link>@<source_instance>[/<source_link>]`
    # or a launch plan: the observer slots of this instance and the source each
    # taps. Like requestedPairs these are commands to the daemon, not resolved
    # config: the daemon registers each observation BEFORE the instance commits
    # to Running so it delivers the source pin the moment both are up (and
    # again whenever the source restarts).
    plannedObservations @8 :List(ObservationRequest);
    # SHA256 of the node manifest the planner validated this instance
    # against, which is the manifest the coordinator resolved for the whole
    # launch. The spawning daemon compares it against the entity now in its
    # stack and refuses if the hashes differ, so an entity replaced between
    # the launch's add phase and this dispatch fails loudly here instead of
    # starting a node the plan was never checked against. Empty only on the
    # in-process launch path, where planner and spawner are the same daemon
    # reading the same entity under the same lock.
    manifestSha256 @9 :Text;
    # See `NodeAddGoal.launchId`.
    launchId @10 :Text;
    # Core nodes that must hear about this instance's lifecycle but that the
    # spawning daemon cannot work out for itself.
    #
    # Concretely: the daemons whose OBSERVERS tap this instance. An observer
    # claims no slot and holds no peer, and the source is deliberately unaware
    # of it, so the source's daemon has no record it could consult. Pairing is
    # different: a pair names both endpoints and their core nodes, so the
    # daemon derives those recipients from its own registry.
    #
    # Only the planner sees the whole graph, so only the planner can fill this
    # in. Empty for a single-machine launch and for anything a user typed.
    lifecycleWatchers @11 :List(Text);
    # The restart policy the launcher or the flag wrote for this instance.
    restart @12 :RestartRequest;
}

# One instance, addressed the way the whole mesh addresses instances: by the
# `(coreNode, instanceId)` pair. An `instanceId` alone is unique only within one
# stack, so every cross-machine reference carries both halves and no daemon ever
# infers "this must be local" from an absent field, including when the target
# IS local, where this holds the receiving daemon's own name.
#
# Decodes to `config::runtime::ProducerRef`, the same type the validator stamps
# into slot bindings, so a message and the plan it acts on speak one vocabulary.
struct InstanceAddress {
    coreNode @0 :Text;
    instanceId @1 :Text;
}

struct VacantPair {
    # The starting node's own pairing-slot link_id.
    linkId @0 :Text;
    # Why the deployment leaves this slot unpaired, in its own words. Always
    # non-empty: a vacancy without a reason is rejected where it is written,
    # so the operator reading a running stack sees the same sentence the
    # launch file carries.
    reason @1 :Text;
}

# The launcher's or the flag's say on an instance's restart policy, which the
# daemon resolves against the node's own manifest.
struct RestartRequest {
    # "never", "on_failure" or "always"; empty when nothing was written.
    policy @0 :Text;
    # Where the policy was written: "instance", "flag" or "launcher_root".
    origin @1 :Text;
    # Attempts in a row before the instance stays failed; 0 for no cap.
    maxRestarts @2 :UInt32;
}

struct PairRequest {
    # The starting node's own pairing-slot link_id. Repeated across entries
    # when that slot holds several pairs.
    linkId @0 :Text;
    # The peer instance this slot pairs with.
    peer @1 :InstanceAddress;
    # The complementary slot on the peer, when the request pins one. Empty
    # means unpinned: exactly one available complementary slot must exist on
    # the peer and the daemon resolves it.
    peerLinkId @2 :Text;
    # The planner's verdict about a peer the receiving daemon cannot inspect.
    #
    # Set only when `peer.coreNode` is a DIFFERENT machine from the one being
    # asked to start. A daemon validates a pair against the two manifests it
    # holds; for a peer on another machine it holds neither, so re-deriving the
    # rules is not an option and guessing is worse. A federated launch's
    # coordinator does hold every participant's manifests and checks the whole
    # plan (same pairing, complementary roles, matching sha pins) before
    # anything starts; this carries that verdict to the daemon that commits
    # the local half.
    #
    # Absent for a same-daemon peer, where the local manifests are the
    # authority and a coordinator's opinion would be a second one.
    remotePeer @3 :RemotePeerPairing;
}

# What a daemon needs about a pair endpoint it cannot read a manifest for.
# Mirrors the fields the pair registry records, so a cross-machine pair is
# stored exactly like a local one once committed.
struct RemotePeerPairing {
    # Empty `pairingName` means "not set": a same-daemon peer carries no
    # remote verdict, and Cap'n Proto defaults an absent struct's text to "".
    pairingName @0 :Text;
    pairingTag @1 :Text;
    # The role the peer's manifest declares for its side of the pair.
    peerRole @2 :Text;
    # The cardinality the peer's manifest declares for its slot, one of the
    # four spellings a manifest writes.
    peerCardinality @3 :Text;
    # The copy the peer's instance belongs to; empty outside a copy.
    peerCopy @4 :Text;
}

struct ObservationRequest {
    # The starting node's own observer-slot link_id.
    observerLinkId @0 :Text;
    # The pairings this slot observes, in the order the plan lists them
    # (launcher array order, or `--link` occurrence order). Sized against the
    # slot's declared cardinality by the planner. Never empty: a
    # `zero_or_more` slot that observes nothing carries no ObservationRequest
    # at all.
    targets @1 :List(ObservationMember);
}

# One pairing an observer slot taps.
struct ObservationMember {
    # The source instance whose role topics this member observes. A remote
    # source subscribes identically to a local one; what differs is that its
    # lifecycle transitions reach the observing daemon as notifications from
    # the source's own daemon rather than from local lifecycle events.
    source @0 :InstanceAddress;
    # The source-side participant slot link_id: the segment the source
    # publishes its observed role topics under, and the third element of the
    # observer's fully-pinned subscription. Always resolved by the planner (the
    # CLI preflight or the launcher), never empty.
    sourceLinkId @1 :Text;
    # The pair's other end when the plan named the pair by it: the peer the
    # source publishes to and the link_id of the peer's slot. Absent, the
    # member observes every pair of the source's slot.
    peer @2 :ObservedPeer;
}

struct ObservedPeer {
    instance @0 :InstanceAddress;
    linkId @1 :Text;
}

# The goal response of an accepted node run. The daemon refuses a goal in
# the goal envelope, with a failure kind and a message.
struct NodeRunGoalResponse {
    # Path to the log file of the run, on the daemon's machine.
    logPath @0 :Text;
}

struct NodeRunFeedback {
    stream @0 :FeedbackStream;
    # The line of output
    line @1 :Text;
}

# What an operator does with an endpoint's URL: `page` is opened in a
# browser, `mcp` is attached to by an MCP client. Mirrors the manifest's
# `execution.endpoints.<label>.kind`.
enum EndpointKind {
    page @0;
    mcp @1;
}

# One endpoint an instance serves: the label its manifest declares, the kind
# the declaration gives it, and the URLs the daemon hosting it expanded the
# bound socket into, one per host interface address.
struct InstanceEndpoint {
    label @0 :Text;
    kind @1 :EndpointKind;
    urls @2 :List(Text);
}

# The result of a completed node run. A failed run gives a failure kind
# and a message, and no result; a cancelled run gives its reason, and no
# result.
struct NodeRunResult {
    # Process ID of the running node (0 if not available)
    pid @0 :UInt32;
    # The endpoints the started instance serves, in label order; empty for a
    # node that declares none.
    endpoints @1 :List(InstanceEndpoint);
}

# Node Stop service
struct NodeStopRequest {
    # Instance ID of the node to stop
    instanceId @0 :Text;
}

# The response of a node stop that succeeded. A failed stop is an error
# reply, with a failure kind and a message.
struct NodeStopResponse {
    # Whether the node had to be force-killed (it did not exit within the
    # cooperative shutdown grace period). False when it exited gracefully.
    forceKilled @0 :Bool;
}

# Node Remove service
struct NodeRemoveRequest {
    # Name of the node to remove
    nodeName @0 :Text;
    # If set, will attempt to stop all the instances associated with this node first before removing the node
    stopInstances @1 :Bool;
    # Tag of the node to remove
    tag @2 :Text;
    # The launch removing a node it added, admitted on a daemon reserved
    # for that launch; empty for a request a user typed
    launchId @3 :Text;
}

# The response of a node removal that succeeded. A failed removal is an
# error reply, with a failure kind and a message.
struct NodeRemoveResponse {
    # The instances the removal stopped that did not exit within the
    # cooperative shutdown window and were force-killed; empty when each
    # exited gracefully.
    forceKilled @0 :List(Text);
}

# Stack Reset service
struct StackResetRequest {
}

# The response of a stack reset that succeeded. A failed reset is an error
# reply, with a failure kind and a message.
struct StackResetResponse {
}

# Node Info service
struct NodeInfoRequest {
    # Name of the node to look up in the stack
    nodeName @0 :Text;
    # Tag of the node to look up in the stack
    nodeTag @1 :Text;
}

struct NodeInstanceInfo {
    # Instance identifier
    instanceId @0 :Text;
    # Per-instance state: "starting", "running", "restarting", "finished" or
    # "failed".
    state @1 :Text;
    # JSON-encoded `BTreeMap<String, Vec<ProducerRef>>` (link_id → bound
    # producer list) mirroring
    # `RuntimeConfig.node_instance.slot_bindings`. Empty string when the
    # node has no `depends_on` slots. Surfacing this lets the launcher /
    # CLI cross-check newly-staged binding plans against what running
    # consumers have already claimed.
    slotBindingsJson @2 :Text;
    # Liveness from the daemon's most recent `node_health` probe for this
    # instance: true when it last answered within the probe timeout, false
    # otherwise. Defaults to `true` so a message from a producer that predates
    # this field is not read as spuriously unhealthy on version skew.
    healthy @3 :Bool = true;
    # JSON-encoded `BTreeMap<String, SerializedPairingSlot>` mirroring
    # `SerializedInstance.pairing_slots` (the live pairing-slot state per
    # `depends_on.pairings` entry). Empty string when the node declares no
    # pairings.
    pairingSlotsJson @4 :Text;
    # JSON-encoded `config::runtime::ClockBinding`: the one clock this instance
    # reads, and its role in that clock's domain. Empty string for wall time,
    # which is also what an absent field reads as.
    clockJson @5 :Text;
    # The endpoints the instance serves, in label order; empty for a node
    # that declares none.
    endpoints @6 :List(InstanceEndpoint);
    # The resolved restart policy: "never", "on_failure" or "always".
    restart @7 :Text;
    # Restart attempts in a row the policy allows before the next exit or
    # failed start leaves the instance failed; 0 for no cap.
    maxRestarts @8 :UInt32;
    # Where the policy was written: "instance", "flag", "node",
    # "launcher_root" or "default". Empty, like `restart`, from a daemon
    # older than the policy.
    restartOrigin @9 :Text;
    # Times a restarted process of this instance reached running.
    restartCount @10 :UInt32;
    # Restart attempts in a row, which `maxRestarts` caps, as
    # `RestartStatus.attempt` in core-node-api documents them.
    attempt @11 :UInt32;
    # Why the last start ended: its process's exit, or a start failure.
    lastExit @12 :ExitCause;
    # When the instance last reached running, in milliseconds since the Unix
    # epoch; 0 when it never did.
    runningSinceMs @13 :UInt64;
    # When the next start is due, in milliseconds since the Unix epoch; 0 when
    # none is scheduled.
    nextStartMs @14 :UInt64;
}

# Why the last start of an instance ended: its process's exit, or a start
# failure.
struct ExitCause {
    union {
        # No process of the instance has exited.
        none @0 :Void;
        # The process exited with this code.
        code @1 :Int32;
        # The process was ended by a signal.
        signal :group {
            # Its number.
            number @2 :Int32;
            # Its name on the machine the process ran on; empty when that
            # machine has none for the number.
            name @3 :Text;
            # Whether the process dumped core.
            coreDumped @4 :Bool;
        }
        # The daemon's own account: a start failure's message, or an exit
        # status it could not read.
        message @5 :Text;
        # The daemon killed the process, which answered no health check for
        # this many seconds.
        healthKillSecs @6 :UInt64;
    }
}

# Node info lookup result.
#
# The response is a union so that "no such node in the stack" is a
# first-class successful outcome rather than a protocol-level error. This
# lets the `peppy node add` preflight check, `peppy node info`, and any
# other caller disambiguate "not found" from a real fault without having
# to sniff the error-string payload of an `InvalidServiceRequest`.
struct NodeInfoResponse {
    union {
        # The node is not in the stack. Carries no payload: the caller
        # already knows which `(name, tag)` it asked about.
        notInStack @0 :Void;
        # The node is in the stack. All of the node metadata is grouped
        # under this arm; callers must match on the union before reading.
        found :group {
            # JSON5-serialized NodeConfig as stored in the node stack
            configJson5 @1 :Text;
            # SHA256 of the entire NodeConfig file
            configSha256 @2 :Text;
            # Lifecycle stage of the in-stack entity ("Added"/"Building"/"Ready"/"Root").
            stage @3 :Text;
            # All tracked instances of this entity, including in-flight `Starting` ones.
            instances @4 :List(NodeInstanceInfo);
            # Path to the most-recent add/build log file for this entity.
            # Empty string when no add log has been produced yet.
            addLogPath @5 :Text;
            # Per-instance run log paths, aligned with `instances` (same order).
            runLogPaths @6 :List(Text);
            # The daemon's clock when it answered, in milliseconds since the
            # Unix epoch: what each instance's `nextStartMs` counts down against.
            reportedAtMs @7 :UInt64;
        }
    }
}

# Instance Documents service
struct InstanceDocumentsRequest {
    # Id of an instance of this daemon's stack
    instanceId @0 :Text;
}

# One document the daemon holds, by the identity it is pinned under.
struct PinnedDocument {
    name @0 :Text;
    tag @1 :Text;
    # SHA-256 of `content`, in lowercase hex.
    sha256 @2 :Text;
    content @3 :Text;
}

# The pinned documents of one instance: what its build was generated from.
# An instance that is not in the stack, or a document the daemon can no
# longer read, is an error reply with a failure kind.
struct InstanceDocumentsResponse {
    # The daemon that runs the instance.
    coreNode @0 :Text;
    instanceId @1 :Text;
    # The node manifest as the stack holds it, serialized as JSON5, with the
    # node's name and tag.
    manifest @2 :PinnedDocument;
    # Each contract document the manifest names, in `manifest.implements`
    # order, then `depends_on.contracts` order, then the document of each
    # daemon interface in `depends_on.daemon` order.
    contracts @3 :List(PinnedDocument);
    # Each pairing document the manifest names, in `depends_on.pairings`
    # order, then `depends_on.pairing_observers` order.
    pairings @4 :List(PinnedDocument);
}
