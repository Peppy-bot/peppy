@0xa8f5c2b9e4d3f1a9;

# Launcher message structures for master-node services

struct LauncherRequest {
    # JSON5-encoded PeppyLauncher configuration
    peppyLauncherJson5 @0 :Text;
    # Directory to search for node configurations
    nodesDirectory @1 :Text;
}

struct LauncherResponse {
    # Whether the launcher was successful
    success @0 :Bool;
    # Error message if failed (empty if successful)
    errorMessage @1 :Text;
}
