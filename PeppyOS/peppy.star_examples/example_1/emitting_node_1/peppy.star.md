```python

uvc_camera = struct(
    namespace = "/",

    publishes = [
        struct(
            type = "topic",
            name = "video_feed",
            parameters = {"qos_profile": "default"},
            interface = {
                "header": str,
                "encoding": str,
                "width": uint32,
                "height": uint32,
                "data": uint8,
            },
        ),
    ],

    parameters = {
        "device": "/dev/video5",
        "fps": 30,
        "resolution": "1920x1080",
        "format": "YUYV",
    },
)

    
```