```python
# PeppyOS REST API Service Node Configuration
# This node provides a REST API interface for system interaction

def create_video_stream_api():
    """Turns the received frames into a video feed"""
    return struct(
        namespace = "/",
        version = "0.1.0",

        # Subscribed topics
        subscribes_to = [
            struct(
                topic_type = "sensor_msgs/Image",
                topic_name = "/camera/video_feed",
                callback = "handle_video_feed",
            ),
        ],

        # Services provided by this node
        services = [
           # TODO fix
            struct(
                service_type = "custom_srvs/GetSystemInfo",
                service_name = "/api/get_system_info",
                callback = "handle_get_system_info",
            ),
        ],

        # Actions provided by this node
        actions = [
            struct(
                action_type = "custom_actions/ProcessVideo",
                action_name = "/api/process_video",
                callback = "handle_process_video_action",
            ),
        ],

        # Node parameters
        parameters = struct(
            # HTTP Server configuration
            http = struct(
                host = "0.0.0.0",
                port = 8080,
                cors_enabled = True,
                cors_origins = ["*"],
                max_connections = 100,
                request_timeout_ms = 30000,
            ),

            # WebSocket configuration
            websocket = struct(
                enabled = True,
                port = 8081,
                heartbeat_interval_ms = 5000,
                max_message_size_bytes = 1048576,  # 1MB
            ),

            # Authentication
            auth = struct(
                enabled = False,
                type = "jwt",  # "jwt", "api_key", "oauth2"
                secret_key = "",  # Should be loaded from environment
            ),

            # API endpoints configuration
            endpoints = struct(
                prefix = "/api/v1",
                health_check = "/health",
                metrics = "/metrics",
                documentation = "/docs",
            ),

            # Rate limiting
            rate_limit = struct(
                enabled = True,
                requests_per_minute = 60,
                burst_size = 10,
            ),

            # Video streaming settings
            video_stream = struct(
                enabled = True,
                format = "mjpeg",  # "mjpeg", "h264", "webrtc"
                quality = 80,  # 0-100 for JPEG quality
                max_fps = 30,
            ),
        ),

        # Quality of Service settings
        qos_profile = "reliable",

        # Resource limits
        resources = struct(
            max_memory_mb = 512,
            cpu_affinity = [],
            max_threads = 10,
        ),

        # Logging configuration
        logging = struct(
            level = "info",
            to_file = True,
            file_path = "/var/log/peppy/rest_api.log",
            max_file_size_mb = 100,
            max_files = 5,
            format = "json",  # "json", "text"
        ),

        # Node lifecycle
        auto_start = True,
        respawn = True,
        respawn_delay = 10.0,
        shutdown_timeout_s = 30.0,

        # Diagnostics
        diagnostics = struct(
            enabled = True,
            publish_rate_hz = 0.5,
            health_checks = [
                "http_server_running",
                "websocket_connections",
                "request_latency",
                "error_rate",
            ],
            metrics = struct(
                prometheus_enabled = True,
                prometheus_port = 9090,
            ),
        ),

        # Dependencies
        depends_on = [],  # Will be populated by parent if needed
    )

# Define custom helper functions for the API node
def create_response(status_code, data, headers = None):
    """Helper function to create HTTP responses."""
    return struct(
        status = status_code,
        body = data,
        headers = headers or {},
    )

def validate_request(request, required_fields):
    """Helper function to validate incoming requests."""
    for field in required_fields:
        if field not in request:
            return False, "Missing required field: " + field
    return True, ""

# Export for use by other modules
exported = struct(
    node = create_video_stream_api(),
    create_response = create_response,
    validate_request = validate_request,
)
```