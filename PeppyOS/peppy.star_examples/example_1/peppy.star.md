```python
# PeppyOS Root Node Configuration
# This is the main configuration file that defines the root node
# and loads child node configurations

# Load child node configurations
load("./emitting_node_1/peppy.star", "uvc_camera")
load("./service_node_1/peppy.star", "rest_api")

def create_root_node():
    """Creates and returns the root node configuration."""
    return struct(
        namespace = "/",

        # Node lifecycle settings
        auto_start = True,
        respawn = False,
        respawn_delay = 2.0,  # seconds

        # Communication interfaces
        publishes = [],  # Topics this node publishes to
        subscribes = [],  # Topics this node subscribes to
        services = [],  # Services this node provides
        actions = [],  # Actions this node provides

        # Dependencies - nodes that must be started before this one
        depends_on = [uvc_camera, rest_api],

        # Runtime parameters
        parameters = struct(
            # Add any runtime parameters here
        ),

        # Quality of Service settings
        qos_profile = "default",  # "default", "sensor_data", "reliable", "best_effort"

        # Resource limits
        resources = struct(
            max_memory_mb = 512,
            cpu_affinity = [],  # CPU cores to pin to (empty = no pinning)
        ),

        # Logging configuration
        logging = struct(
            level = "info",  # "debug", "info", "warn", "error", "fatal"
            to_file = False,
            file_path = "",
        ),

        # Custom initialization script (executed on node startup)
        init_script = "",

        # Monitoring and diagnostics
        diagnostics = struct(
            enabled = True,
            publish_rate_hz = 1.0,
        ),
    )

# Export the root node configuration
root_node = create_root_node()

# Define what this module exports when loaded
exported = struct(
    node = root_node
)
```