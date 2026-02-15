from peppygen import NodeBuilder, NodeRunner
from peppygen.parameters import Parameters


async def setup(params: Parameters, node_runner: NodeRunner):
    pass


def main():
    NodeBuilder().run(setup)


if __name__ == "__main__":
    main()
