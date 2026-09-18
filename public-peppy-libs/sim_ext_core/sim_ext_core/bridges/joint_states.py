"""Joint state publisher bridge."""

from __future__ import annotations

import json
import time
from typing import Any

from sim_ext_core.base import BridgePlugin

_QOS = "sensor_data"


class JointStatesBridge(BridgePlugin):
    """Publishes joint positions and velocities from the articulation."""

    def __init__(self, articulation: Any, config: Any, entry: Any) -> None:
        self._articulation = articulation
        self._node_name: str = config.node_name
        self._robot_name: str = entry.robot_name
        self._topic: str = entry.topic
        self._joint_names: list[str] = []
        self._limits_lower: list[float] = []
        self._limits_upper: list[float] = []

    def setup(self) -> bool:
        if not self._articulation.setup():
            return False
        if hasattr(self._articulation, "get_joint_names"):
            self._joint_names = list(self._articulation.get_joint_names())
        else:
            self._joint_names = []
        # Limits are static model data — cache once. Consumers use them to
        # clamp motion targets to the reachable range.
        if hasattr(self._articulation, "get_joint_limits"):
            limits = self._articulation.get_joint_limits()
            if limits is not None:
                self._limits_lower, self._limits_upper = limits
        return True

    def teardown(self) -> None:
        # Forward teardown so any native handles the articulation view holds
        # are released on extension reload.
        self._articulation.teardown()

    def on_step(self, step: int, io: Any) -> None:
        states = self._articulation.get_joint_states()
        if states is None:
            return
        positions, velocities = states
        payload = json.dumps(
            {
                "robot": self._robot_name,
                "step": step,
                "joint_names": self._joint_names,
                "positions": positions,
                "velocities": velocities,
                "limits_lower": self._limits_lower,
                "limits_upper": self._limits_upper,
                "stamp": time.time(),
            }
        ).encode()
        io.emit(self._node_name, self._topic, _QOS, payload)

    @property
    def is_ready(self) -> bool:
        return self._articulation.is_ready
