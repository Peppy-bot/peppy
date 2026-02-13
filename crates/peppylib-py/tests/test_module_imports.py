"""Tests for public peppylib module import paths."""


def test_public_messaging_module_is_importable():
    """`peppylib.messaging` and nested submodules are public import paths."""
    from peppylib import messaging
    from peppylib.messaging import MessengerHandle, TopicMessenger, ZenohdInstance
    from peppylib.messaging.actions import ActionMessenger
    from peppylib.messaging.services import ServiceMessenger

    assert messaging is not None
    assert MessengerHandle is not None
    assert TopicMessenger is not None
    assert ZenohdInstance is not None
    assert ServiceMessenger is not None
    assert ActionMessenger is not None
