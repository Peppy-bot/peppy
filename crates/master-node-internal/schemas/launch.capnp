@0xa8f5c2b9e4d3f1a9;

# Launch message structures for master-node services

struct LaunchRequest {
    # JSON5-encoded PeppyLaunch configuration
    peppyLaunchJson5 @0 :Text;
    # Directory to search for node configurations
    nodesDirectory @1 :Text;
    # JSON5-encoded LaunchRuntimeConfig with messaging endpoint
    launchRuntimeConfigJson5 @2 :Text;
}

struct LaunchResponse {
    # Whether the launch was successful
    success @0 :Bool;
    # Error message if failed (empty if successful)
    errorMessage @1 :Text;
}
