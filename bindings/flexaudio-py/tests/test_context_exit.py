"""Cleanup preserves body exceptions and their existing context chains."""
import flexaudio
import pytest


class RejectNotesError(Exception):
    def add_note(self, note):
        raise RuntimeError("note rejected")

    def __setattr__(self, name, value):
        raise RuntimeError("assignment rejected")


@pytest.mark.parametrize("fixture, failure", [
    ("_stream_stop_failure", "device lost"),
    ("_watcher_join_failure", "join"),
])
@pytest.mark.parametrize("reject_notes", [False, True])
@pytest.mark.parametrize("existing_context", [False, True])
def test_context_exit_preserves_body_and_existing_context(
        fixture, failure, reject_notes, existing_context):
    if not hasattr(flexaudio, fixture):
        pytest.skip("requires test-faults wheel with the requested fixture")
    manager = getattr(flexaudio, fixture)()
    body = RejectNotesError("body failure") if reject_notes else ValueError("body failure")
    previous = LookupError("previous failure") if existing_context else None
    BaseException.__setattr__(body, "__context__", previous)
    with pytest.raises(type(body)) as caught:
        with manager:
            raise body
    assert caught.value is body
    assert str(body) == "body failure"
    if not reject_notes and hasattr(body, "__notes__"):
        assert body.__context__ is previous
        assert any(failure in note for note in body.__notes__)
    else:
        if previous is not None:
            assert body.__context__ is previous
            cleanup = previous.__context__
        else:
            cleanup = body.__context__
        assert isinstance(cleanup, RuntimeError)
        assert cleanup.audio_error.kind == (
            "deviceLost" if fixture == "_stream_stop_failure" else "backend")
        assert failure in str(cleanup)
