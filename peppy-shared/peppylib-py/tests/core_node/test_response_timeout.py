"""Every `core_node` helper reads its optional `response_timeout_secs` the same
way: a negative number raises `ValueError` naming the argument, at call time,
before any request goes out."""

import pytest
from peppylib import clock, datastore, info, stack

from .common import start_router_and_runner

# Each helper called with only its required arguments plus the timeout.
_HELPERS_WITH_TIMEOUT = {
    "info": lambda runner, timeout: info(runner, timeout),
    "stack.list": lambda runner, timeout: stack.list(runner, timeout),
    "clock.synchronize": lambda runner, timeout: clock.synchronize(runner, timeout),
    "datastore.store": lambda runner, timeout: datastore.store(
        runner, "key", b"value", datastore.Encoding.TEXT_PLAIN, timeout
    ),
    "datastore.get": lambda runner, timeout: datastore.get(runner, "key", timeout),
    "datastore.list": lambda runner, timeout: datastore.list(runner, timeout),
    "datastore.remove": lambda runner, timeout: datastore.remove(
        runner, "key", timeout
    ),
}


@pytest.mark.asyncio
@pytest.mark.parametrize(
    "helper", _HELPERS_WITH_TIMEOUT.values(), ids=_HELPERS_WITH_TIMEOUT
)
async def test_core_node_helpers_reject_a_negative_response_timeout(tmp_path, helper):
    router, node_runner, _server_handle = await start_router_and_runner(tmp_path)
    try:
        with pytest.raises(
            ValueError,
            match=r"^response_timeout_secs must be a finite, non-negative number of "
            r"seconds, got -1\.5$",
        ):
            helper(node_runner, -1.5)
    finally:
        await router.stop()
