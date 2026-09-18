"""Contact force publisher bridge."""

from __future__ import annotations

import json
import time
from typing import Any

from sim_ext_core.base import BridgePlugin

_QOS = "sensor_data"


class ContactForcesBridge(BridgePlugin):
    """Publishes active contacts read from a contact sensor."""

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
        contacts = self._sensor.get_contact_data()
        if not contacts:
            return
        payload = json.dumps(
            {
                "robot": self._robot_name,
                "step": step,
                "contacts": [
                    {
                        "body1": c["body1"],
                        "body2": c["body2"],
                        "position": list(c["position"]),
                        "force": list(c["force"]),
                    }
                    for c in contacts
                ],
                "stamp": time.time(),
            }
        ).encode()
        io.emit(self._node_name, self._topic, _QOS, payload)

    @property
    def is_ready(self) -> bool:
        return self._sensor.is_ready
