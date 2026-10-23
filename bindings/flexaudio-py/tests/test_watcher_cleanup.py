"""Device-free watcher teardown failures use the real Linux owner join."""
import gc
import sys

import flexaudio
import pytest


@pytest.mark.skipif(not hasattr(flexaudio, "_watcher_join_failure"),
                    reason="requires Linux test-faults wheel")
@pytest.mark.parametrize("mode", ["context", "drop"])
def test_watcher_teardown_delivers_join_failure(mode):
    if mode == "context":
        with pytest.raises(flexaudio.BackendError, match="join"):
            with flexaudio._watcher_join_failure():
                pass
    else:
        observed = []
        previous = sys.unraisablehook
        sys.unraisablehook = observed.append
        try:
            watcher = flexaudio._watcher_join_failure()
            del watcher
            gc.collect()
        finally:
            sys.unraisablehook = previous
        assert len(observed) == 1
        assert isinstance(observed[0].exc_value, flexaudio.BackendError)
        assert "join" in str(observed[0].exc_value)
