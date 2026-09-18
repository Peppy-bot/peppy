"""Odometry publisher bridge."""

from __future__ import annotations

import json
import time
from typing import Any

from sim_ext_core.base import BridgePlugin

_QOS = "sensor_data"


class OdometryBridge(BridgePlugin):
    """Publishes base pose and twist."""

    def __init__(self, sensor: Any, config: Any, entry: Any) -> None:
        self._sensor = sensor
        self._node_name: str = config.node_name
        self._robot_name: str = entry.robot_name
        self._topic: str = entry.topic

    def setup(self) -> bool:
        return self._sensor.setup()

    def teardown(self) -> None:
        self._sensor.teardown()

    def on_step(self, step: int, io: Any) -> None:
        data = self._sensor.get_odometry_data()
        if data is None:
            return
        payload = json.dumps(
            {
                "robot": self._robot_name,
                "step": step,
                "position": data["position"],
                "orientation": data["orientation"],
                "linear_velocity": data["linear_velocity"],
                "angular_velocity": data["angular_velocity"],
                "stamp": time.time(),
            }
        ).encode()
        io.emit(self._node_name, self._topic, _QOS, payload)

    @property
    def is_ready(self) -> bool:
        return self._sensor.is_ready
