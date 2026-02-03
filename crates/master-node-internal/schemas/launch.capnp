@0xa8f5c2b9e4d3f1a9;

# Launch action message structures for master-node services

struct EnvVar {
    key @0 :Text;
    value @1 :Text;
}

struct LaunchGoal {
    # Path to the peppy launch file
    peppyLaunchFilePath @0 :Text;
    # Environment variables to apply when executing add_cmd and start_cmd (e.g. PATH)
    envVars @1 :List(EnvVar);
    # Timeout in seconds for each node add operation
    nodeAddTimeoutSecs @2 :UInt64;
    # Timeout in seconds for each node start operation
    nodeStartTimeoutSecs @3 :UInt64;
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
    startingNode @2;
}

struct LaunchFeedback {
    # The stream type: "stdout" or "stderr"
    stream @0 :Text;
    # The line of output
    line @1 :Text;
    # The step in the launch process this feedback is from
    step @2 :LaunchFeedbackStep;
}

struct LaunchResult {
    # Whether the launch was successful
    success @0 :Bool;
    # Path to the log file for this launch action
    logPath @1 :Text;
    # Error message if failed (empty if successful)
    errorMessage @2 :Text;
}
