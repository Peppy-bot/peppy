"""
Tests for peppylib MessengerHandle.
"""

import inspect

import pytest


def test_messenger_handle_import():
    """MessengerHandle can be imported from peppylib."""
    from peppylib import MessengerHandle

    assert MessengerHandle is not None


def test_messenger_handle_has_from_host_port():
    """MessengerHandle has from_host_port static method."""
    from peppylib import MessengerHandle

    assert hasattr(MessengerHandle, "from_host_port")
    assert callable(MessengerHandle.from_host_port)


async def test_messenger_handle_from_host_port_returns_awaitable():
    """MessengerHandle.from_host_port returns an awaitable."""
    from peppylib import MessengerHandle

    result = MessengerHandle.from_host_port("127.0.0.1", 7447)
    assert inspect.isawaitable(result)
    # Cancel the future to clean up
    result.cancel()
