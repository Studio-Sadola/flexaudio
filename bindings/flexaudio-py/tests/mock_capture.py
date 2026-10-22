"""Device-free probe for the test-faults build property.

A wheel built without the test-faults feature rejects ``open("mock")`` as an
unknown source kind before any device is opened. The Windows wheel CI job is
built that way. Tests that need mock capture skip with a reason on every OS.
"""

from __future__ import annotations

import unittest

import flexaudio

SKIP_REASON = (
    "installed build was compiled without the test-faults feature, "
    "so mock capture is unavailable"
)

_available = None


def mock_capture_available():
    """True when this build accepts the mock source."""
    global _available
    if _available is None:
        _available = _probe()
    return _available


def _probe():
    try:
        with flexaudio.open("mock"):
            return True
    except ValueError as error:
        message = str(error)
        if message.startswith("unknown kind:") and "mock" in message:
            return False
        raise


def skip_unless_mock_capture(method):
    """Skip when mock capture is absent."""
    if mock_capture_available():
        return method
    return unittest.skip(SKIP_REASON)(method)
