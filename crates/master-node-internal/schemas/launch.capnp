@0xa8f5c2b9e4d3f1a9;

# Launch action message structures for master-node services

struct LaunchGoal {
    # JSON5-encoded PeppyLaunch configuration
    peppyLaunchJson5 @0 :Text;
    # Directory to search for node configurations
    nodesDirectory @1 :Text;
    # JSON5-encoded LaunchRuntimeConfig with messaging endpoint
    launchRuntimeConfigJson5 @2 :Text;
}

struct LaunchGoalResponse {
    # Whether the goal was accepted
    accepted @0 :Bool;
    # Path to the log file for this launch action
    logPath @1 :Text;
    # Rejection reason if not accepted (empty if accepted)
    rejectionReason @2 :Text;
}

struct LaunchFeedback {
    # The stream type: "stdout" or "stderr"
    stream @0 :Text;
    # The line of output
    line @1 :Text;
}

struct LaunchResult {
    # Whether the launch was successful
    success @0 :Bool;
    # Path to the log file for this launch action
    logPath @1 :Text;
    # Error message if failed (empty if successful)
    errorMessage @2 :Text;
}
