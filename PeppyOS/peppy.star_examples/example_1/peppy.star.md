```python

# Load dependencies
load("./emitting_node_1/peppy.star", "uvc_camera")
load("./service_node_1/peppy.star", "rest_api")

root_node = struct(
    namespace = "/",

    # Node lifecycle settings
    auto_start = True,
    respawn = False,
    respawn_delay = 2.0,  # seconds

    # Communication interfaces
    publishes = [],       # Example: [ {"type": "service", "parameters": []} ]
    subscribe_from = [],  # Example: [ {"node_name": "SomeNode"} ]
    services = [],
    actions = [],

    # Dependencies
    depends_on = ["uvc_camera", "rest_api"],

    # Parameters
    parameters = {},

    # QoS settings
    qos_profile = "default",  # options: "default", "sensor_data", etc.

    # Resource limits
    max_memory_mb = 512,
    cpu_affinity = [],

    # Logging configuration
    log_level = "info",  # options: "debug", "info", "warn", etc.
    log_to_file = False,
    log_file_path = "",

    # Custom initialization
    init_script = "",

    # Monitoring and diagnostics
    enable_diagnostics = True,
)

# Create instance (in Starlark this is just a variable assignment)
node = RootNode
```