from peppygen import NodeBuilder, NodeRunner, StandaloneConfig
from peppygen.parameters import (
    Device,
    Parameters,
    Video,
    VideoResolution,
)


async def setup(params: Parameters, node_runner: NodeRunner):
    print("Inside the setup callback!")


def main():
    # Those arguments could eventually be parsed with argparse
    fake_params = Parameters(
        device=Device(
            physical="/dev/video0",
            sim="virtual_camera",
            priority="high",
        ),
        video=Video(
            frame_rate=30,
            resolution=VideoResolution(
                width=1920,
                height=1080,
            ),
            encoding="h264",
        ),
    )

    standalone_config = StandaloneConfig().with_parameters(fake_params)
    NodeBuilder().standalone(standalone_config).run(setup)


if __name__ == "__main__":
    main()
