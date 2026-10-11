"""Tests for public peppylib module import paths."""


def test_public_messaging_module_is_importable():
    """`peppylib.messaging` and nested submodules are public import paths."""
    from peppylib.messaging import MessengerHandle, TopicMessenger, ZenohdInstance
    from peppylib.messaging.actions import ActionMessenger
    from peppylib.messaging.services import ServiceMessenger

    from peppylib import messaging

    assert messaging is not None
    assert MessengerHandle is not None
    assert TopicMessenger is not None
    assert ZenohdInstance is not None
    assert ServiceMessenger is not None
    assert ActionMessenger is not None


def test_version_comes_from_the_native_module():
    """`peppylib.__version__` is the version the native extension was built
    with."""
    import peppylib

    assert peppylib.__version__ == peppylib._peppylib.__version__
