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
    # author declared or by the name of a copy the file lists, never by a
    # `--join` copy, which takes its machine from its own reference; empty
    # means no placement flag was given at all. `local` is `--local`:
    # collapse EVERY link the document declares onto the coordinator,
    # whatever it turns out to declare, and no `--join` and no word names a
    # machine.
    #
    # Cap'n Proto defaults an unset union to its first member, so a goal that
    # sets neither decodes as an empty `places`, which means no placement
    # was given.
    placement :union {
        places @8 :List(CoreNodeLink);
        local @9 :Void;
    }
    # The launcher's `--with` words: each selects an option of a stack axis,
    # or of an own axis of one copy of the launch, the part that it selects
    # running on the machine the word names or with the copy's base.
    #
    # The WORDS travel, not their resolution, for the same reason placement
    # is an intent: only the coordinator holds a repository launcher's
    # document, so only it knows which component axes exist. A caller that
    # resolved selections itself could do so for a filesystem launcher and
    # not for a repository one, and the two origins would drift into meaning
    # different things. Empty means no `--with` was given, which a launcher
    # with no `components` requires and any other launcher fills from its
    # defaults.
    selections @10 :List(LaunchWord);
    # Build every node of the launch from its staged sources even when
    # storage already holds an artifact built from byte-identical sources.
    # Forwarded to each node build the launch performs, on the coordinator
    # and on every peer. Absent decodes as false, which reuses such artifacts.
    rebuild @11 :Bool;
    # The copies `--join OPTION:NAME@CORE_NODE` starts with the launch, in
    # the order they were given: each a copy of the launcher's entry for the
    # option under that name, its base on the copy's machine, composed,
    # validated and started with the launch.
    joins @12 :List(CopyReference);
    # Resolve, add and build on the coordinator every joinable node: each
    # node that a join of an option of a `stack_copies` scope of the launch
    # can bring, so that such a join on the coordinator builds nothing.
    # `--no-prebuild` sets it to false: the launch then resolves and builds
    # only the nodes it runs. Absent decodes as true.
    prebuild @13 :Bool = true;
}

# The machine a copy, a part of a copy or a core node link runs on. `self`
# travels as `coordinator`, the daemon the launch or the join is sent to,
# and is resolved there.
struct CoreNodeTarget {
  union {
    coordinator @0 :Void;
    named @1 :Text;
  }
}

# `option` or `axis=option`: one option of one component axis. With no
# axis, the coordinator finds the axis that holds the option.
struct OptionChoice {
  axis :union {
    unnamed @0 :Void;
    named @1 :Text;
  }
  option @2 :Text;
}

# `CHOICE[@CORE_NODE]`: one option of an own axis of a copy, and where the
# part it selects runs. `withCopy` runs the part on the copy's machine,
# with its base.
struct SelectionWord {
  choice @0 :OptionChoice;
  coreNode :union {
    withCopy @1 :Void;
    target @2 :CoreNodeTarget;
  }
}

# `NAME.CHOICE[@CORE_NODE]`: a selection word of the copy NAME of a launch.
struct CopyWord {
  copy @0 :Text;
  word @1 :SelectionWord;
}

# One `--with` word of a launch: an option of a stack axis, which runs
# where its `core_node` link says, or a word of one copy.
struct LaunchWord {
  union {
    stack @0 :OptionChoice;
    copy @1 :CopyWord;
  }
}

# `OPTION:NAME[@CORE_NODE]`: the copy of `option` that runs under `name`,
# its base on the copy's machine.
struct CopyReference {
  option @0 :Text;
  name @1 :Text;
  coreNode @2 :CoreNodeTarget;
}

# One `--place <core-node-link>@<core-node>` wiring: the placeholder the
# launcher author declared, or the name of a copy the file lists, on the
# left, the machine on the right.
struct CoreNodeLink {
    # The placeholder as it appears in the launcher's `core_nodes` list, or
    # the name of a copy the launcher file lists.
    linkId @0 :Text;
    # The machine it is wired to; `self` travels as `coordinator`.
    coreNode @1 :CoreNodeTarget;
}

struct StackJoinGoal {
    # The copy: its option, of an axis that runs as copies; its name, the
    # prefix of every instance id it mints and the placement link of the
    # copy's machine; and the copy's machine, which runs its base.
    copy @0 :CopyReference;
    # `--with` words over the copied option's own axes, each with the
    # machine of the part it selects.
    selections @1 :List(SelectionWord);
    # Argument overrides for the copy's instances, each `(instanceId,
    # argument)` pair named at most once.
    arguments @2 :List(ArgumentOverride);
    # The same budgets a LaunchGoal carries.
    envVars @3 :List(EnvVar);
    nodeAddIdleTimeoutSecs @4 :UInt64;
    nodeBuildIdleTimeoutSecs @5 :UInt64;
    nodeRunIdleTimeoutSecs @6 :UInt64;
    maxTimeoutSecs @7 :UInt64;
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
