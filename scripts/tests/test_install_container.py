"""Container build tests that run after install.sh tests.

These tests install peppy into a native-arch Lima VM and exercise the
container node workflow (``peppy node init --container`` + ``peppy node add``).
Only native-architecture VMs are used — no cross-arch QEMU emulation.
"""

from __future__ import annotations

import subprocess
from typing import TYPE_CHECKING

if TYPE_CHECKING:
    from collections.abc import Generator

import pytest

from .lima_helpers import (
    VMConfig,
    diagnostic,
    host_arch,
    install_cmd,
    lima_shell,
    lima_vm_lifecycle,
    setup_lima_guest,
)

LINUX_DISTROS = ["ubuntu"]


# ---------------------------------------------------------------------------
# VM config construction (native arch only)
# ---------------------------------------------------------------------------


def _build_native_vm_configs() -> list[VMConfig]:
    """Build VM configs for the native host architecture only."""
    host = host_arch()
    return [
        VMConfig(os="linux", arch=host, distro=d, instance_prefix="ptc")
        for d in LINUX_DISTROS
    ]


VM_CONFIGS = _build_native_vm_configs()


# ---------------------------------------------------------------------------
# Fixtures
# ---------------------------------------------------------------------------


@pytest.fixture(scope="module", params=VM_CONFIGS, ids=lambda c: c.pytest_id())
def lima_vm(request, _build_release_archives) -> Generator[VMConfig, None, None]:
    """Create (or reuse) an ephemeral Lima VM for container build testing.

    Parameterized across native-arch VM configs only.
    """
    yield from lima_vm_lifecycle(request)


# ---------------------------------------------------------------------------
# Tests
# ---------------------------------------------------------------------------


def test_container_node_build(lima_vm: VMConfig) -> None:
    """Test full container node workflow: init, add, and start."""
    config = lima_vm
    assert config.os == "linux", "container build test only applies to Linux VMs"

    home = setup_lima_guest(
        config, test_name=f"test_container_build_{config.pytest_id()}"
    )
    instance = config.instance_name
    env_preamble = f'export PATH="{home}/bin:$PATH" && export PEPPY_HOME={home}'

    def run_step(
        label: str, script: str, *, timeout: int = 120
    ) -> subprocess.CompletedProcess[str]:
        """Run a single step inside the VM, printing its output."""
        print(f"\n{'=' * 60}")
        print(f"STEP: {label}")
        print(f"{'=' * 60}")
        result = lima_shell(script, instance=instance, timeout=timeout)
        if result.stdout:
            print(result.stdout)
        if result.stderr:
            print(f"[stderr] {result.stderr}")
        print(f"=> exit code: {result.returncode}")
        return result

    # Clean up any leftover state
    run_step(
        "Cleanup",
        f"pkill -f 'peppy service serve' 2>/dev/null; rm -rf {home}; true",
    )

    # Install peppy
    result = run_step(
        "Install peppy",
        install_cmd(config, home, extra_env="PEPPY_FORCE_REINSTALL=1"),
    )
    assert result.returncode == 0, (
        f"install failed on {config.pytest_id()}{diagnostic(result)}"
    )

    # Start daemon in a detached session.  limactl shell (SSH) hangs after
    # bash exits because the daemon inherits SSH channel file descriptors
    # that keep the connection alive.  We fire-and-forget with a short
    # timeout, then verify in a separate SSH session.
    print(f"\n{'=' * 60}")
    print("STEP: Start daemon")
    print(f"{'=' * 60}")
    try:
        lima_shell(
            f"{env_preamble} && setsid peppy service serve < /dev/null > /tmp/peppy-daemon.log 2>&1 &\nsleep 3\necho 'Daemon launched'",
            instance=instance,
            timeout=30,
        )
    except subprocess.TimeoutExpired:
        pass  # Expected: limactl shell hangs after backgrounding the daemon

    # Verify the daemon is running in a fresh SSH session.
    result = run_step(
        "Verify daemon",
        "pgrep -f 'peppy service serve' > /dev/null && echo 'Daemon running'",
    )
    assert result.returncode == 0, f"daemon start failed{diagnostic(result)}"

    # Init container node
    result = run_step(
        "Node init --container",
        f"{env_preamble} && cd /tmp && rm -rf test-node && peppy node init --container test-node",
    )
    assert result.returncode == 0, f"node init failed{diagnostic(result)}"

    # Add the node (triggers Apptainer container build)
    result = run_step(
        "Node add",
        f"{env_preamble} && cd /tmp/test-node && peppy node add .",
        timeout=600,
    )
    assert result.returncode == 0, f"node add failed{diagnostic(result)}"

    # Start the node
    result = run_step(
        "Node start",
        f"{env_preamble} && peppy node start test-node:0.1.0",
        timeout=120,
    )
    assert result.returncode == 0, f"node start failed{diagnostic(result)}"

    # Stop daemon
    run_step(
        "Stop daemon",
        "pkill -f 'peppy service serve' 2>/dev/null; true",
    )
