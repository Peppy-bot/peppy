```python
# PeppyOS UVC Camera Node Configuration
# This node handles UVC camera input and publishes video feed

def create_uvc_camera_node():
    """Creates and returns the UVC camera node configuration."""
    return struct(
        namespace = "/",
        tags = ["uvc", "camera", "usb"], # Tags used to look the node up on the public repo

		# Allows pulling the node from a central repo (https://repo.peppy.bot by default)
		#pull_from = "uvc_camera:0.1"

        # Published topics
        publishes = [
            struct(
                topic_type = "sensor_msgs/Image",
                topic_name = "/camera/video_feed",
                message_format = struct(
                    header = struct(
                        stamp = "time",
                    ),
                    encoding = str,  # "rgb8", "bgr8", "yuyv", "mjpeg"
                    width = uint32,
                    height = uint32,
                    image = [uint8, uint8, uint8],
                ),
            ),
        ],

        # Subscribed topics
        subscribes_to = [],

        # Services provided by this node
        services = [
	        # TODO Fix
            struct(
                service_type = "std_srvs/SetBool",
                service_name = "/camera/enable",
                callback = "handle_camera_enable",
            ),
        ],

        # Node parameters
        parameters = struct(
            device_path = "/dev/video5",
            frame_rate = 30,
            resolution = struct(
                width = 1920,
                height = 1080,
            ),
            pixel_format = "YUYV",  # "YUYV", "MJPEG", "H264"
        ),

        # Quality of Service settings
        qos_profile = "sensor_data",

        # Resource limits
        resources = struct(
            max_memory_mb = 256,
            cpu_affinity = [],
        ),

        # Logging configuration
        logging = struct(
            level = "info",
            to_file = False,
            file_path = "",
        ),

        # Node lifecycle
        auto_start = True,
        respawn = True,
        respawn_delay = 5.0,

        # Diagnostics
        diagnostics = struct(
            enabled = True,
            publish_rate_hz = 1.0,
            health_checks = [
                "camera_connected",
                "frame_rate_nominal",
                "buffer_underrun",
            ],
        ),
    )

# Create the node instance
uvc_camera = create_uvc_camera_node()

# Export for use by other modules
exported = struct(
    node = uvc_camera
)
    
```