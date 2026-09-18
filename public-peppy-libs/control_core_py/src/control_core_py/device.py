"""The hardware device-thread skeleton shared by serial-device nodes: the
thread lifecycle and latest-wins slots only; what a device reads and how it
calibrates belongs to its robot's description.

Serial I/O blocks, so one OS thread owns the bus for the process lifetime:
connect, the tick loop, and the shutdown release all run on it. The asyncio
side exchanges data through latest-wins slots under a lock, and neither side
ever waits on the other.
"""

from __future__ import annotations

import threading
import time
from abc import ABC, abstractmethod

from control_core_py.params import require_positive
from control_core_py.runtime import log

# Bringup budget: connect plus calibration verification plus the first read.
CONNECT_TIMEOUT_S = 15.0
# Shutdown budget for the device thread to finish its tick and disconnect.
JOIN_TIMEOUT_S = 5.0


class LatestSlot:
    """One latest-wins value with its arrival stamp, lock-guarded."""

    def __init__(self) -> None:
        self._lock = threading.Lock()
        self._value = None
        self._stamp = 0.0

    def put(self, value) -> None:
        with self._lock:
            self._value = value
            self._stamp = time.monotonic()

    def fresh(self, timeout_s: float):
        with self._lock:
            if self._value is None or time.monotonic() - self._stamp > timeout_s:
                return None
            return self._value

    def get(self):
        with self._lock:
            return self._value


class LatestValue:
    """One latest-wins value with its arrival stamp and the producer's capture
    stamp, for a holder that only ever lives on one event loop.

    LatestSlot is the cross-thread form, lock-guarded for a device thread
    handing values to an event loop. This one buys nothing from a lock and
    carries two things that form needs no answer for: the stamp a relaying
    node must forward downstream, so a value and the time it was captured
    cannot desync, and a clear() for the consumer that must forget what it
    was told rather than wait for it to age out.
    """

    def __init__(self) -> None:
        self._value = None
        self._stamp = 0.0
        self.wire_timestamp_s: float | None = None

    def set(self, value, wire_timestamp_s: float) -> None:
        """Adopt a value and the time its producer captured it. The capture
        stamp is required, not defaulted: a holder whose stamp can silently
        go missing puts publish time on the wire, which reads as fresh."""
        self._value = value
        self._stamp = time.monotonic()
        self.wire_timestamp_s = wire_timestamp_s

    def fresh(self, timeout_s: float):
        """The value if it arrived within the window, else None."""
        if self._value is None or time.monotonic() - self._stamp > timeout_s:
            return None
        return self._value

    def clear(self) -> None:
        """Forget the value. The next fresh() reads None however recently it
        arrived, for a consumer that must stop acting on what it was told."""
        self._value = None
        self.wire_timestamp_s = None


class DeviceThread(ABC):
    """Owns the hardware thread. Start blocks until bringup succeeds or
    raises, so a node that cannot serve never reports as launched."""

    stop_warning = "device thread did not stop cleanly"

    def __init__(self, hardware, period_s: float, thread_name: str):
        self._hardware = hardware
        # A non-positive or NaN period would turn the tick loop into a busy
        # loop commanding the bus at an unbounded rate.
        self._period_s = require_positive("period_s", period_s)
        self._ready = threading.Event()
        self._stop = threading.Event()
        self._bringup_error: Exception | None = None
        self._thread = threading.Thread(target=self._run, name=thread_name, daemon=True)

    def start(self) -> None:
        self._thread.start()
        if not self._ready.wait(CONNECT_TIMEOUT_S):
            self.stop()
            raise RuntimeError(f"device bringup did not complete within {CONNECT_TIMEOUT_S}s")
        if self._bringup_error is not None:
            raise self._bringup_error

    def stop(self) -> None:
        self._stop.set()
        self._thread.join(timeout=JOIN_TIMEOUT_S)
        if self._thread.is_alive():
            log(self.stop_warning)

    def ready(self) -> bool:
        return self._ready.is_set() and self._bringup_error is None and self._thread.is_alive()

    @abstractmethod
    def _verify_first_read(self) -> None:
        """One full read that raises on failure: a bus that cannot serve it
        must fail the launch, not report ready and stream nothing."""

    @abstractmethod
    def _tick(self) -> None:
        """One period of hardware work; must not raise."""

    def _run(self) -> None:
        try:
            self._hardware.connect()
            self._verify_first_read()
        except Exception as e:
            self._bringup_error = e
            # Connect may have claimed the port or enabled torque before the
            # failure; a failed launch must leave neither behind.
            try:
                self._hardware.disconnect()
            except Exception as release_error:
                log(f"bringup cleanup failed: {release_error!r}")
            self._ready.set()
            return
        self._ready.set()

        deadline = time.monotonic()
        while not self._stop.is_set():
            self._tick()
            deadline += self._period_s
            delay = deadline - time.monotonic()
            if delay > 0.0:
                self._stop.wait(delay)
            else:
                # A slipped tick resyncs instead of bursting.
                deadline = time.monotonic()

        try:
            self._hardware.disconnect()
        except Exception as e:
            log(f"disconnect failed: {e!r}")
