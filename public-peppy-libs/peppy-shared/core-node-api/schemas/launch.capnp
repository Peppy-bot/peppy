@0xa8f5c2b9e4d3f1a9;

# Launch action message structures for core-node services

struct EnvVar {
    key @0 :Text;
    value @1 :Text;
}

struct LaunchGoal {
    # Environment variables to apply when executing build_cmd and run_cmd (e.g. PATH)
    envVars @0 :List(EnvVar);
    # Idle timeout in seconds for the node add phase (resets on git/http progress or sub-process output)
    nodeAddIdleTimeoutSecs @1 :UInt64;
    # Idle timeout in seconds for the node run-startup phase (resets on subprocess output until the node signals ready)
    nodeRunIdleTimeoutSecs @2 :UInt64;
    # Absolute max timeout in seconds for the entire launch; 0 = unset, no overall deadline (idle timeouts still apply)
    maxTimeoutSecs @3 :UInt64;
    # Idle timeout in seconds for the node build phase (resets on build_cmd output)
    nodeBuildIdleTimeoutSecs @4 :UInt64;
    # Where the launcher file comes from. `fs` carries an absolute path; `repository` carries
    # a launcher name to look up in `~/.peppy/cache/launchers.json5`.
    launcherOrigin :union {
        fs @5 :Text;
        repository @6 :Text;
    }
    # Identity of this federated launch, minted by the coordinator that
    # receives this goal. Every participant records it alongside its slice,
    # so the global stack is reconstructible by query from any machine and a
    # restarted coordinator finds its own launch again.
    #
    # Never empty. A goal without one comes from a peppy that predates
    # federated launch, and is refused at decode rather than silently running
    # every instance on the coordinator: Cap'n Proto defaults an absent field
    # to the empty string, so nothing but this check makes the break loud.
    launchId @7 :Text;
    # How this launch places the launcher's declared core node links.
    #
    # The INTENT travels, not its expansion. Only the coordinator holds the
    # resolved launcher: a `repository` origin is read from the daemon's own
    # cache, which the caller may never have seen, so the caller cannot know
    # which `core_nodes` the document declares. A caller that expanded
    # `--local` itself could only do so for the origin it happens to hold,
    # and the two origins would drift into meaning different things.
    #
    # `places` is the `--place` wiring, keyed by the placeholder the launcher
    # author declared; empty means no placement flag was given at all, which
    # only a launcher declaring no `core_nodes` can satisfy. `local` is
    # `--local`: collapse EVERY link the document declares onto the
    # coordinator, whatever it turns out to declare.
    #
    # Cap'n Proto defaults an unset union to its first member, so a goal that
    # sets neither decodes as an empty `places` — the same "no placement
    # given" that has always meant.
    placement :union {
        places @8 :List(CoreNodeLink);
        local @9 :Void;
    }
    # The launcher's `--with` selections, verbatim: each entry is `option` or
    # `axis=option`, exactly as the caller typed it.
    #
    # The WORDS travel, not their resolution, for the same reason placement
    # is an intent: only the coordinator holds a repository launcher's
    # document, so only it knows which component axes exist. A caller that
    # resolved selections itself could do so for a filesystem launcher and
    # not for a repository one, and the two origins would drift into meaning
    # different things. Empty means no `--with` was given, which a launcher
    # with no `components` requires and any other launcher fills from its
    # defaults.
    selections @10 :List(Text);
    # Build every node of the launch from its staged sources even when
    # storage already holds an artifact built from byte-identical sources.
    # Forwarded to each node build the launch performs, on the coordinator
    # and on every peer. Absent decodes as false, which reuses such artifacts.
    rebuild @11 :Bool;
}

# One `--place <core-node-link>@<core-node>` wiring: the placeholder the
# launcher author declared on the left, the concrete machine on the right.
struct CoreNodeLink {
    # The placeholder as it appears in the launcher's `core_nodes` list.
    linkId @0 :Text;
    # The core node it is wired to. Never the literal `self`: the CLI
    # resolves `self` to the coordinator's own name before dispatch, so a
    # daemon only ever sees concrete names.
    coreNode @1 :Text;
}

struct StackJoinGoal {
    # The copy's name: the prefix of every instance id it mints, and the
    # placement link the whole copy is wired by.
    name @0 :Text;
    # The option of a `zero_or_more` axis the copy runs.
    option @1 :Text;
    # `--with` words over the copied option's own axes, verbatim.
    selections @2 :List(Text);
    # Argument overrides for the copy's instances, each `(instanceId,
    # argument)` pair named at most once.
    arguments @3 :List(ArgumentOverride);
    # Where the whole copy runs; absent decodes to the coordinator.
    placement :union {
        coordinator @4 :Void;
        coreNode @5 :Text;
    }
    # The same budgets a LaunchGoal carries.
    envVars @6 :List(EnvVar);
    nodeAddIdleTimeoutSecs @7 :UInt64;
    nodeBuildIdleTimeoutSecs @8 :UInt64;
    nodeRunIdleTimeoutSecs @9 :UInt64;
    maxTimeoutSecs @10 :UInt64;
}

# One argument of one of the copy's instances, given a new value.
struct ArgumentOverride {
    instanceId @0 :Text;
    argument @1 :Text;
    # The value as JSON5 text, holding only finite numbers.
    value @2 :Text;
}

# Stop and remove the instances of one copy, by the name `stack join` gave it.
struct StackRemoveGoal {
    name @0 :Text;
}

struct LaunchGoalResponse {
    # Whether the goal was accepted
    accepted @0 :Bool;
    # Path to the log file for this launch action
    logPath @1 :Text;
    # Rejection reason if not accepted (empty if accepted)
    rejectionReason @2 :Text;
}

enum LaunchFeedbackStep {
    launcherStep @0;
    addingNode @1;
    runningNode @2;
    buildingNode @3;
}

struct LaunchFeedback {
    # The stream type: "stdout" or "stderr"
    stream @0 :Text;
    # The line of output
    line @1 :Text;
    # The step in the launch process this feedback is from
    step @2 :LaunchFeedbackStep;
}

struct NodeAddLog {
    # Node label in "name:tag" format
    nodeLabel @0 :Text;
    # Path to the node add log file
    logPath @1 :Text;
    # Whether the add operation failed
    failed @2 :Bool;
    # Core node whose filesystem holds the log file
    coreNode @3 :Text;
}

struct NodeRunLog {
    # Instance ID
    instanceId @0 :Text;
    # Node label in "name:tag" format
    nodeLabel @1 :Text;
    # Path to the node run log file
    logPath @2 :Text;
    # Whether the run operation failed
    failed @3 :Bool;
    # Core node whose filesystem holds the log file
    coreNode @4 :Text;
}

struct NodeBuildLog {
    # Node label in "name:tag" format
    nodeLabel @0 :Text;
    # Path to the node build log file
    logPath @1 :Text;
    # Whether the build operation failed
    failed @2 :Bool;
    # Core node whose filesystem holds the log file
    coreNode @3 :Text;
}

# What an operator does with an endpoint's URL: `page` is opened in a
# browser, `mcp` is attached to by an MCP client. Every schema in this
# directory is self-contained, so this mirrors `node.capnp`'s enum.
enum EndpointKind {
    page @0;
    mcp @1;
}

# One endpoint an instance serves: its declared label and kind, and the URLs
# the daemon hosting the instance expanded the bound socket into.
struct LaunchEndpoint {
    label @0 :Text;
    kind @1 :EndpointKind;
    urls @2 :List(Text);
}

# The endpoints of one started instance. The URLs were expanded by the daemon
# that hosts the instance against its own interfaces, so a coordinator only
# forwards them with that daemon's core node name.
struct InstanceEndpoints {
    # Instance ID
    instanceId @0 :Text;
    # Node label in "name:tag" format
    nodeLabel @1 :Text;
    # Core node hosting the instance
    coreNode @2 :Text;
    endpoints @3 :List(LaunchEndpoint);
}

struct LaunchResult {
    # Whether the launch was successful
    success @0 :Bool;
    # Path to the log file for this launch action
    logPath @1 :Text;
    # Error message if failed (empty if successful)
    errorMessage @2 :Text;
    # Per-node add log entries
    nodeAddLogs @3 :List(NodeAddLog);
    # Per-node run log entries
    nodeRunLogs @4 :List(NodeRunLog);
    # Per-node build log entries
    nodeBuildLogs @5 :List(NodeBuildLog);
    # The endpoints of every started instance that serves one, in start order.
    instanceEndpoints @6 :List(InstanceEndpoints);
}
