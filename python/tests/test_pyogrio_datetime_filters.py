"""Datetime parsing preserves values without Terrio changing warning filters, on pandas 2/3."""

import datetime
import functools
import sys
import warnings
from types import SimpleNamespace

import pytest

pd = pytest.importorskip("pandas")

import terrio.pyogrio as tpg  # noqa: E402


def test_uniform_offsets_do_not_require_stdlib_preparse(monkeypatch):
    """Uniform standard ISO timestamps pass straight to pandas.

    Args:
        monkeypatch: Scoped stdlib parser replacement.
    """
    calls = []

    def observed(value):
        """Record offset inspection calls.

        Args:
            value: ISO timestamp.

        Returns:
            datetime: Parsed timestamp.
        """
        calls.append(value)
        return datetime.datetime.fromisoformat(value)

    monkeypatch.setattr(
        tpg, "_dt", SimpleNamespace(datetime=SimpleNamespace(fromisoformat=observed))
    )
    values = pd.Series(["2024-01-01T12:00:00+01:00", "2024-01-02T12:00:00+0100"])
    result = tpg._parse_datetime(values, False, False)
    assert len(result) == 2
    assert calls == []


@pytest.mark.parametrize("fraction", ["5", "12345", "1234567", "123456789"])
def test_fractional_seconds_are_normalized_for_older_stdlib(monkeypatch, fraction):
    """Python datetime output does not require singleton pandas calls for nanoseconds.

    Args:
        monkeypatch: Scoped external parser replacement.
        fraction: Fractional seconds with a precision an older parser may reject.
    """

    def reject_pandas(*args, **kwargs):
        """Fail if the stdlib output path falls back to pandas.

        Args:
            *args: Ignored parser arguments.
            **kwargs: Ignored parser options.

        Raises:
            AssertionError: Always.
        """
        raise AssertionError("unexpected singleton pandas conversion")

    def limited(value):
        """Simulate a stdlib accepting at most six fractional digits.

        Args:
            value: ISO timestamp.

        Returns:
            datetime: Parsed timestamp.

        Raises:
            ValueError: More than six fractional digits remain.
        """
        digits = value.split(".")[1].split("+")[0]
        if len(digits) not in {3, 6}:
            raise ValueError("unsupported precision")
        return datetime.datetime.fromisoformat(value)

    monkeypatch.setattr(
        tpg, "_dt", SimpleNamespace(datetime=SimpleNamespace(fromisoformat=limited))
    )
    monkeypatch.setattr(pd, "to_datetime", reject_pandas)
    with warnings.catch_warnings(record=True) as caught:
        warnings.simplefilter("always")
        result = tpg._parse_iso_datetime(
            pd, f"2024-01-01T12:00:00.{fraction}+01:00", warn_precision_loss=True
        )
    assert result.microsecond == int(fraction[:6].ljust(6, "0"))
    assert bool(caught) == (len(fraction) > 6)
    if caught:
        assert caught[0].filename == __file__


def test_precision_warning_from_pandas_fallback_points_at_caller(monkeypatch):
    """Older stdlib parsers still report nanosecond loss at the public caller.

    Args:
        monkeypatch: Scoped replacement of Terrio's stdlib parser alias.
    """

    def reject(value):
        """Simulate a stdlib parser that rejects this ISO representation.

        Args:
            value: The ISO string being parsed.

        Raises:
            ValueError: Always, to exercise the real pandas fallback.
        """
        raise ValueError(f"unsupported ISO representation: {value}")

    monkeypatch.setattr(tpg, "_dt", SimpleNamespace(datetime=SimpleNamespace(fromisoformat=reject)))
    with warnings.catch_warnings(record=True) as caught:
        warnings.simplefilter("always")
        value = tpg._parse_iso_datetime(
            pd, "2024-01-01T12:00:00.123456789+01:00", warn_precision_loss=True
        )
    assert value.microsecond == 123456
    assert len(caught) == 1
    assert "Discarding nonzero nanoseconds" in str(caught[0].message)
    assert caught[0].filename == __file__


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
@pytest.mark.parametrize("invalid", [False, True])
def test_distinct_reduced_dates_use_batch_fallback(monkeypatch, as_utc, invalid):
    """Many distinct reduced dates use bounded pandas calls in both offset modes.

    Args:
        monkeypatch: Scoped external parser replacement.
        as_utc: Whether to convert mixed offsets to UTC.
        invalid: Whether one late reduced value is invalid.
    """
    reduced = pd.date_range("2000-01-01", periods=1200, freq="MS").strftime("%Y-%m").tolist()
    values = pd.Series(["2024-01-01T12:00:00+01:00", "2024-01-02T12:00:00+02:00", *reduced])
    if invalid:
        values.loc[len(values)] = "2024-99"
    original = pd.to_datetime
    calls = []

    def counted(*args, **kwargs):
        """Record pandas conversion batches.

        Args:
            *args: Positional conversion arguments.
            **kwargs: Keyword conversion arguments.

        Returns:
            object: The original pandas conversion result.
        """
        calls.append(1)
        return original(*args, **kwargs)

    monkeypatch.setattr(pd, "to_datetime", counted)
    if invalid:
        with pytest.warns(UserWarning, match="Error parsing datetimes"):
            result = tpg._parse_datetime(values, False, as_utc)
        pd.testing.assert_series_equal(result, values)
    else:
        result = tpg._parse_datetime(values, False, as_utc)
        assert result.iloc[2].year == 2000
        assert result.iloc[-1].year == 2099
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
    if precision:
        assert precision[0].filename == __file__
    assert not [warning for warning in caught if issubclass(warning.category, FutureWarning)]
    assert result.iloc[0].microsecond == (123000 if as_utc else 123456)
