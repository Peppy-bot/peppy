"""
Tests for peppylib.config module.
"""


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
