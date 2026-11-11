"""Datetime parsing preserves values without Terrio changing warning filters, on pandas 2/3."""

import functools
import sys
import warnings

import pytest

pd = pytest.importorskip("pandas")

import terrio.pyogrio as tpg  # noqa: E402


def record_filter_changes(calls, frame, event, _argument):
    """Profile warning-filter calls made directly by Terrio, excluding pandas' own calls.

    Args:
        calls: Collected changes.
        frame: Profiled Python frame.
        event: Profiling event.
        _argument: Unused profiling payload.
    """
    if event != "call" or frame.f_globals.get("__name__") != "warnings":
        return
    name = frame.f_code.co_name
    if name not in {"__enter__", "filterwarnings", "simplefilter", "resetwarnings"}:
        return
    caller = frame.f_back.f_globals.get("__name__", "") if frame.f_back is not None else ""
    if caller.startswith("terrio."):
        calls.append((caller, name))


CASES = [
    (
        ["2024-01-01T12:00:00", "2024-01-02T12:00:00"],
        ["2024-01-01T12:00:00", "2024-01-02T12:00:00"],
        ["2024-01-01T12:00:00", "2024-01-02T12:00:00"],
        False,
    ),
    (
        ["2024-01-01T12:00:00+01:00", "2024-01-02T12:00:00+0100"],
        ["2024-01-01T12:00:00+01:00", "2024-01-02T12:00:00+01:00"],
        ["2024-01-01T12:00:00+01:00", "2024-01-02T12:00:00+01:00"],
        False,
    ),
    (
        ["2024-01-01T12:00:00Z", "2024-01-02T12:00:00+00:00"],
        ["2024-01-01T12:00:00+00:00", "2024-01-02T12:00:00+00:00"],
        ["2024-01-01T12:00:00+00:00", "2024-01-02T12:00:00+00:00"],
        False,
    ),
    (
        ["2024-01-01T12:00:00+01:00", "2024-01-02T12:00:00+02:00"],
        ["2024-01-01T12:00:00+01:00", "2024-01-02T12:00:00+02:00"],
        ["2024-01-01T11:00:00+00:00", "2024-01-02T10:00:00+00:00"],
        True,
    ),
    (
        ["2024-01-01T12:00:00", "2024-01-02T12:00:00+01:00"],
        ["2024-01-01T12:00:00", "2024-01-02T12:00:00+01:00"],
        ["2024-01-01T12:00:00+00:00", "2024-01-02T11:00:00+00:00"],
        True,
    ),
    (
        ["2024-01-01T12:00:00Z", None, "2024-01-02T12:00:00+01:00"],
        ["2024-01-01T12:00:00+00:00", None, "2024-01-02T12:00:00+01:00"],
        ["2024-01-01T12:00:00+00:00", None, "2024-01-02T11:00:00+00:00"],
        True,
    ),
]


@pytest.mark.parametrize("values,preserved,utc,mixed", CASES)
@pytest.mark.parametrize("as_utc", [False, True])
def test_datetime_values_without_terrio_filter_changes(values, preserved, utc, mixed, as_utc):
    """Uniform, mixed, naive and missing dates retain the requested conversion semantics.

    Args:
        values: Input strings and missing values.
        preserved: Expected values with their original offsets.
        utc: Expected values when mixed offsets are converted to UTC.
        mixed: Whether inputs mix distinct offsets or naive and aware values.
        as_utc: Whether mixed values should become UTC datetimes.
    """
    calls = []
    previous = sys.getprofile()
    sys.setprofile(functools.partial(record_filter_changes, calls))
    try:
        result = tpg._parse_datetime(pd.Series(values, dtype="object"), False, as_utc)
    finally:
        sys.setprofile(previous)
    assert calls == []
    assert [None if pd.isna(value) else value.isoformat() for value in result] == (
        utc if as_utc else preserved
    )
    assert result.dtype.kind == ("O" if mixed and not as_utc else "M")


def test_invalid_mixed_datetime_retains_its_diagnostic_without_filter_changes():
    """Invalid input retains its strings and diagnostic rather than a synthetic timezone error."""
    values = pd.Series(
        ["2024-01-01T12:00:00+01:00", "2024-01-02T12:00:00+02:00", "invalid"], dtype="object"
    )
    calls = []
    previous = sys.getprofile()
    with pytest.warns(UserWarning, match="Error parsing datetimes") as caught:
        sys.setprofile(functools.partial(record_filter_changes, calls))
        try:
            actual = tpg._parse_datetime(values, False, False)
        finally:
            sys.setprofile(previous)
    assert actual.equals(values)
    assert calls == []
    assert not [warning for warning in caught if issubclass(warning.category, FutureWarning)]


@pytest.mark.parametrize("extra", ["2024", "2024-01", "NaT", ""])
@pytest.mark.parametrize("as_utc", [False, True])
def test_pandas_iso_dates_do_not_bypass_mixed_offset_detection(extra, as_utc):
    """Pandas-only ISO forms and missing strings cannot expose its mixed-offset warning.

    Args:
        extra: A value pandas accepts that datetime.fromisoformat cannot parse.
        as_utc: Whether mixed offsets should be converted to UTC.
    """
    values = pd.Series(["2024-01-01T12:00:00+01:00", "2024-01-02T12:00:00+02:00", extra])
    calls = []
    previous = sys.getprofile()
    with warnings.catch_warnings(record=True) as caught:
        warnings.simplefilter("always", FutureWarning)
        sys.setprofile(functools.partial(record_filter_changes, calls))
        try:
            result = tpg._parse_datetime(values, False, as_utc)
        finally:
            sys.setprofile(previous)
    assert calls == []
    assert not caught
    if as_utc:
        expected = pd.to_datetime(values, format="ISO8601", utc=True).astype("datetime64[ms, UTC]")
    else:
        expected = pd.Series(
            [pd.to_datetime([value], format="ISO8601")[0].to_pydatetime() for value in values],
            dtype="object",
        )
    pd.testing.assert_series_equal(result, expected)


@pytest.mark.parametrize(
    "extra,expected",
    [
        ("2024", "2024-01-01T00:00:00"),
        ("2024-01-01T12:00:00.123456789+03:00", "2024-01-01T12:00:00.123456+03:00"),
    ],
)
def test_repeated_iso_forms_do_not_parse_each_row_with_pandas(monkeypatch, extra, expected):
    """Preserving mixed offsets parses repeated ISO forms a bounded number of times.

    Args:
        monkeypatch: Scoped replacement of the external pandas parser.
        extra: Repeated reduced date or nanosecond timestamp.
        expected: Expected Python datetime representation.
    """
    original = pd.to_datetime
    calls = []

    def counted(*args, **kwargs):
        """Record external parser calls without changing the parsing result.

        Args:
            *args: Positional pandas parser arguments.
            **kwargs: Keyword pandas parser arguments.

        Returns:
            object: The original pandas parser result.
        """
        calls.append(1)
        return original(*args, **kwargs)

    monkeypatch.setattr(pd, "to_datetime", counted)
    values = pd.Series(["2024-01-01T12:00:00+01:00", "2024-01-02T12:00:00+02:00", extra] * 1000)
    with warnings.catch_warnings(record=True):
        warnings.simplefilter("always")
        result = tpg._parse_datetime(values, False, False)
    assert len(result) == 3000
    assert result.iloc[2].isoformat() == expected
    assert len(calls) < 10


@pytest.mark.parametrize("as_utc", [False, True])
def test_submicrosecond_precision_keeps_the_conversion_diagnostic(as_utc):
    """Preserving offsets retains pandas' precision warning; UTC output uses milliseconds.

    Args:
        as_utc: Whether to convert mixed offsets to the documented millisecond UTC dtype.
    """
    values = pd.Series(
        ["2024-01-01T12:00:00.123456789+01:00", "2024-01-02T12:00:00.123456789+02:00"]
    )
    with warnings.catch_warnings(record=True) as caught:
        warnings.simplefilter("always")
        result = tpg._parse_datetime(values, False, as_utc)
    precision = [
        warning for warning in caught if "Discarding nonzero nanoseconds" in str(warning.message)
    ]
    assert bool(precision) == (not as_utc)
    assert not [warning for warning in caught if issubclass(warning.category, FutureWarning)]
    assert result.iloc[0].microsecond == (123000 if as_utc else 123456)
