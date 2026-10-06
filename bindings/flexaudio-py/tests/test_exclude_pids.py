"""Boundary tests: every call fails validation before audio hardware is accessed."""

import inspect
import re
from collections.abc import Sequence

import pytest

import flexaudio


class IndexOnly:
    def __index__(self):
        return 1


class PidSequence(Sequence):
    def __init__(self, values):
        self.values = values

    def __len__(self):
        return len(self.values)

    def __getitem__(self, index):
        return self.values[index]


@pytest.mark.parametrize("kind", ["mic", "system", "process", "mix"])
@pytest.mark.parametrize("pid", [True, False, 1.5, "1", None, IndexOnly()])
@pytest.mark.parametrize("container", [list, tuple, PidSequence])
def test_non_integer_entries_are_type_errors(kind, pid, container):
    with pytest.raises(TypeError, match=r"exclude_pids\[3\] must be a positive integer"):
        flexaudio.open(kind, exclude_pids=container([1, 2, 3, pid]))


@pytest.mark.parametrize("kind", ["mic", "system", "process", "mix"])
@pytest.mark.parametrize("pid", [0, -1, 4294967296, 2 ** 256, -(2 ** 256), 10 ** 5000], ids=["zero", "negative", "overflow", "huge", "huge-negative", "decimal-limit"])
@pytest.mark.parametrize("container", [list, tuple])
def test_out_of_range_entries_are_value_errors(kind, pid, container):
    with pytest.raises(ValueError, match=r"exclude_pids\[3\] must be a positive integer in 1\.\.=4294967295"):
        flexaudio.open(kind, exclude_pids=container([1, 2, 3, pid]))


@pytest.mark.parametrize("value", ["123", b"123", {1: 2}, {1}, frozenset([1]), iter([1]), (pid for pid in [1]), 1, True])
def test_non_sequences_are_type_errors(value):
    with pytest.raises(TypeError, match="exclude_pids must be a sequence"):
        flexaudio.open("mic", exclude_pids=value)


@pytest.mark.parametrize("container", [list, tuple])
def test_length_guard_precedes_element_validation(container):
    with pytest.raises(ValueError, match=re.escape("exclude_pids: too many entries (max 4096)")):
        flexaudio.open("mic", exclude_pids=container([False] * 4097))


@pytest.mark.parametrize("value", [None, [], (), [1, 4294967295, 1], (1, 4294967295), range(1, 4), PidSequence([1, 2]), [1] * 4096])
def test_valid_exclusions_reach_source_validation_without_device_access(value):
    with pytest.raises(ValueError, match="unknown kind"):
        flexaudio.open("validation-only", exclude_pids=value)


def test_zero_error_includes_index_and_value():
    with pytest.raises(ValueError) as error:
        flexaudio.open("mic", exclude_pids=[1, 2, 3, 0])
    assert str(error.value) == "exclude_pids[3] must be a positive integer in 1..=4294967295, got 0"


def test_exclusions_are_validated_before_addons():
    with pytest.raises(ValueError, match=r"exclude_pids\[0\]"):
        flexaudio.open("mic", exclude_pids=[0], denoise=True, output_rate=16000)


@pytest.mark.parametrize("entry_point", [flexaudio.open, flexaudio.Stream.switch_source])
def test_exclude_pids_is_keyword_only(entry_point):
    parameter = inspect.signature(entry_point).parameters["exclude_pids"]
    assert parameter.kind is inspect.Parameter.KEYWORD_ONLY
    assert parameter.default is None
