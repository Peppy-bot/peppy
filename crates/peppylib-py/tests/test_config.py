"""
Tests for peppylib.config module.
"""

import pytest


# QoSProfile tests

def test_qos_profile_import_from_config_module():
    """QoSProfile can be imported from peppylib.config."""
    from peppylib.config import QoSProfile

    assert QoSProfile is not None


def test_qos_profile_import_from_top_level():
    """QoSProfile can be imported from peppylib top-level."""
    from peppylib import QoSProfile

    assert QoSProfile is not None


def test_qos_profile_all_variants_exist():
    """All QoSProfile variants are accessible."""
    from peppylib.config import QoSProfile

    assert hasattr(QoSProfile, "SensorData")
    assert hasattr(QoSProfile, "Standard")
    assert hasattr(QoSProfile, "Reliable")
    assert hasattr(QoSProfile, "Critical")


def test_qos_profile_variants_are_distinct():
    """Each QoSProfile variant is distinct."""
    from peppylib.config import QoSProfile

    variants = [
        QoSProfile.SensorData,
        QoSProfile.Standard,
        QoSProfile.Reliable,
        QoSProfile.Critical,
    ]
    # Check all pairs are not equal
    for i, v1 in enumerate(variants):
        for v2 in variants[i + 1 :]:
            assert v1 != v2


def test_qos_profile_equality_same_variant():
    """Same QoSProfile variants are equal."""
    from peppylib.config import QoSProfile

    assert QoSProfile.Standard == QoSProfile.Standard
    assert QoSProfile.Critical == QoSProfile.Critical


def test_qos_profile_equality_different_variants():
    """Different QoSProfile variants are not equal."""
    from peppylib.config import QoSProfile

    assert QoSProfile.SensorData != QoSProfile.Standard
    assert QoSProfile.Reliable != QoSProfile.Critical


# DEFAULT_MESSAGING_PORT tests

def test_default_messaging_port_import():
    """DEFAULT_MESSAGING_PORT can be imported from peppylib.config."""
    from peppylib.config import DEFAULT_MESSAGING_PORT

    assert DEFAULT_MESSAGING_PORT is not None


def test_default_messaging_port_is_integer():
    """DEFAULT_MESSAGING_PORT is an integer."""
    from peppylib.config import DEFAULT_MESSAGING_PORT

    assert isinstance(DEFAULT_MESSAGING_PORT, int)


def test_default_messaging_port_is_valid():
    """DEFAULT_MESSAGING_PORT is in valid port range."""
    from peppylib.config import DEFAULT_MESSAGING_PORT

    assert 1 <= DEFAULT_MESSAGING_PORT <= 65535
