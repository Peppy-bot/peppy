"""IMU publisher bridge."""

from __future__ import annotations

import json
import time
from typing import Any

from sim_ext_core.base import BridgePlugin

_QOS = "sensor_data"


class ImuBridge(BridgePlugin):
    """Publishes IMU orientation, angular velocity, and linear acceleration."""

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
        data = self._sensor.get_imu_data()
        if data is None:
            return
        payload = json.dumps(
            {
                "robot": self._robot_name,
                "step": step,
                "orientation": data["orientation"],
                "angular_velocity": data["angular_velocity"],
                "linear_acceleration": data["linear_acceleration"],
                "stamp": time.time(),
            }
        ).encode()
        io.emit(self._node_name, self._topic, _QOS, payload)

    @property
    def is_ready(self) -> bool:
        return self._sensor.is_ready
