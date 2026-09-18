"""Transform tree publisher bridge."""

from __future__ import annotations

import json
import time
from typing import Any

from sim_ext_core.base import BridgePlugin

_QOS = "sensor_data"


class TfTreeBridge(BridgePlugin):
    """Publishes the world transform tree."""

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
        frames = self._sensor.get_tf_data()
        if not frames:
            return
        payload = json.dumps(
            {
                "robot": self._robot_name,
                "step": step,
                "frames": [
                    {
                        "name": f["name"],
                        "parent": f["parent"],
                        "position": list(f["position"]),
                        "orientation": list(f["orientation"]),
                    }
                    for f in frames
                ],
                "stamp": time.time(),
            }
        ).encode()
        io.emit(self._node_name, self._topic, _QOS, payload)

    @property
    def is_ready(self) -> bool:
        return self._sensor.is_ready
