"""HEC time base helpers.

Times from the native layer are whole seconds since 1899-12-31 00:00 (HEC julian day 0),
naive, never shifted by a time zone (SPEC §7). HEC's 24:00 stamp is the same instant as
the next day's 00:00.
"""

from __future__ import annotations

import datetime as _dt
import re
from typing import Union

import numpy as np
import pandas as pd

__all__ = ["HEC_EPOCH", "TimeLike", "to_hec_seconds", "hec_seconds_to_datetime64"]

#: HEC epoch (julian day 0) as a seconds-resolution numpy datetime.
HEC_EPOCH = np.datetime64("1899-12-31T00:00:00", "s")

TimeLike = Union[str, _dt.datetime, _dt.date, pd.Timestamp, np.datetime64]

# 01JAN2020, 01JAN2020 0000, 01JAN2020 2400, 01Jan2020 24:00, 1jan2020 12:30:15
_HEC_DATE = re.compile(
    r"^\s*(\d{1,2})([A-Za-z]{3})(\d{4})"
    r"(?:[\s,]+(\d{2}):?(\d{2})(?::?(\d{2}))?)?\s*$"
)
_MONTHS = {m: i for i, m in enumerate(
    ["JAN", "FEB", "MAR", "APR", "MAY", "JUN", "JUL", "AUG", "SEP", "OCT", "NOV", "DEC"],
    start=1,
)}


def _parse_hec_string(s: str):
    m = _HEC_DATE.match(s)
    if not m:
        return None
    day, mon, year, hh, mm, ss = m.groups()
    month = _MONTHS.get(mon.upper())
    if month is None:
        return None
    base = _dt.datetime(int(year), month, int(day))
    hour, minute, second = int(hh or 0), int(mm or 0), int(ss or 0)
    if not (0 <= minute < 60 and 0 <= second < 60 and
            (0 <= hour < 24 or (hour == 24 and minute == second == 0))):
        raise ValueError(f"invalid time of day in {s!r}; only 24:00:00 may roll over")
    secs = hour * 3600 + minute * 60 + second
    return base + _dt.timedelta(seconds=secs)


def to_hec_seconds(value: TimeLike) -> int:
    """Seconds since 1899-12-31 00:00 for a date/time.

    Accepts ``datetime``/``date``/``pandas.Timestamp``/``numpy.datetime64``, HEC-style
    strings (``"01JAN2020 2400"``, ``"01Jan2020 13:00"``, ``"01JAN2020"``) and anything
    ``pandas.Timestamp`` parses (ISO 8601). Time-zone-aware values are converted to naive
    by dropping the zone (no shift), because DSS stamps are naive.
    Fractional seconds are rejected with ``ValueError``; bounds must be whole seconds.
    """
    if isinstance(value, str):
        parsed = _parse_hec_string(value)
        ts = pd.Timestamp(parsed) if parsed is not None else pd.Timestamp(value)
    else:
        ts = pd.Timestamp(value)
    if ts is pd.NaT:
        raise ValueError(f"not a date/time: {value!r}")
    if ts.tzinfo is not None:
        ts = ts.tz_localize(None)
    if ts.microsecond or ts.nanosecond:
        raise ValueError(f"fractional seconds are not supported: {value!r}")
    delta = ts.to_datetime64().astype("datetime64[s]") - HEC_EPOCH
    return int(delta.astype(np.int64))


def hec_seconds_to_datetime64(times: np.ndarray) -> np.ndarray:
    """``datetime64[s]`` array for an array of HEC seconds."""
    return HEC_EPOCH + np.asarray(times, dtype=np.int64).astype("timedelta64[s]")
