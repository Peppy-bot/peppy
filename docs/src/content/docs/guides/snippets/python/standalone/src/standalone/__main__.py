import json

from peppygen import NodeBuilder, NodeRunner, StandaloneConfig
from peppygen.parameters import Parameters


async def setup(params: Parameters, node_runner: NodeRunner):
    print("Inside the setup callback!")


def main():
    # Parameters can also be defined directly in code:
    #
    # from peppygen.parameters import Video, VideoResolution
    #
    # params = Parameters(
    #     device_path="/dev/video0",
    #     video=Video(
    #         frame_rate=30,
    #         resolution=VideoResolution(
    #             width=1920,
    #             height=1080,
    #         ),
    #         encoding="h264",
    #     ),
    # )

    with open("params.json") as f:
        params = json.load(f)

    standalone_config = StandaloneConfig().with_parameters(params)
    NodeBuilder().standalone(standalone_config).run(setup)


if __name__ == "__main__":
    main()
