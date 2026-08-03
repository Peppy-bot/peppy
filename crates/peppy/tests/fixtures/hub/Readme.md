# Multi-daemon e2e repository fixture

The repository the `multi_daemon_e2e` tests resolve every node, contract and
pairing from. `FixtureRepository::create` in
`crates/peppy/tests/multi_daemon_e2e.rs` copies this tree into a temporary
directory, commits it as a git repository, and mounts it read only into every
daemon container, which reaches it as `file:///etc/peppy/fixture-hub`. Each
container's `repositories.json5` names that one repository and nothing else, so a
test run clones nothing over a network.

It is a git repository rather than an `fs` one because a launch may not place a
deployment resolved from an `fs` entry on another core node: the path names a
tree on the coordinator's filesystem. Half of these tests do exactly that
placement.

## What lives here

The identities the tests name, which include every node
`docs/src/content/docs/guides/snippets/launchers/split_compute_manipulation.json5`
deploys. That launcher is the file the `Federation` guide documents and the one
the federated tests drive, so the names, tags and interfaces here are what keep
it runnable unmodified.

| Kind | Identity | Covers |
|------|----------|--------|
| node | `uvc_camera_python_mock:v1` | the container build path (apptainer packs a SIF) |
| node | `uvc_camera_video_reconstruction_python:v1` | a second container node, placed on the peer |
| node | `my_python_robot_arm:v1` | the native build path (`uv` builds a venv) |
| node | `reactive_policy:v1` | the executor role of a pairing, plus two producer links |
| node | `deliberative_planner:v1` | the planner role, and a producer link across machines |
| node | `episode_recorder:v1` | observing one side of a pairing without joining it |
| contract | `rgb_camera:v1` | the camera role two nodes consume by contract |
| pairing | `deliberation:v1` | the bidirectional planner/executor relationship |

Every node is a mock. None of them controls hardware, runs a model, or writes a
dataset; what they do is carry messages over every slot kind the tests assert on
and print a line when they do. The log lines the tests wait for are part of that
contract, so changing one means changing the test that reads it.

## What a test run still fetches

Building a node resolves `pycapnp` and the build backends from PyPI, and the two
container definitions bootstrap their base image from a registry. That image is
pinned by digest for the same reason this fixture exists. Nothing else leaves the
machine.

## Adding an item

Write it under `nodes/`, `contracts/` or `pairings/`, then declare it in
`peppy_repository.json5`. That file is the repository index, and a refresh reads
it rather than walking the tree, so an item missing from it does not exist as far
as any daemon is concerned.

The nodes deliberately ship no `uv.lock`. A lockfile here would pin
`peppylib==<generator crate version>+peppy` and go stale on the next version
bump, and it would have to be regenerated through a peppy build to be written at
all.
