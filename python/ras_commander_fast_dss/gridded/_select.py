"""What of a source to read: a box and a time range (packet P49). They select from a Zarr store."""

from __future__ import annotations

import datetime as _dt
from typing import Union

import numpy as np

UtcTime = Union[int, str, _dt.datetime, np.datetime64]
_EPOCH = _dt.datetime(1970, 1, 1)


def utc_seconds(t: UtcTime) -> int:
    """Seconds since 1970 of a UTC time. A store's times are UTC; a naive time is taken as UTC."""
    if isinstance(t, bool):
        raise ValueError("a time is a datetime, an ISO string or seconds since 1970")
    if isinstance(t, (int, np.integer)):
        return int(t)
    if isinstance(t, np.datetime64):
        return int(t.astype("datetime64[s]").astype("int64"))
    if isinstance(t, str):
        t = _dt.datetime.fromisoformat(t)
    if isinstance(t, _dt.datetime):
        if t.tzinfo is not None:
            t = t.astimezone(_dt.timezone.utc).replace(tzinfo=None)
        whole, rest = divmod((t - _EPOCH) / _dt.timedelta(seconds=1), 1)
        if rest:
            raise ValueError(f"{t} has a fraction of a second")
        return int(whole)
    raise ValueError("a time is a datetime, an ISO string or seconds since 1970")


def source_box(bbox):
    """`(west, south, east, north)` as floats, or None."""
    if bbox is None:
        return None
    try:
        west, south, east, north = (float(v) for v in bbox)
    except (TypeError, ValueError):
        raise ValueError("bbox must be (west, south, east, north)") from None
    if not all(np.isfinite([west, south, east, north])):
        raise ValueError("bbox must be finite")
    if not (west < east and south < north):
        raise ValueError("bbox must be (west, south, east, north), west < east and south < north")
    return (west, south, east, north)


def time_span(times):
    """`(first, last)` in seconds since 1970 UTC, or None."""
    if times is None:
        return None
    if isinstance(times, (str, bytes)) or len(times) != 2:
        raise ValueError("times must be (first, last)")
    span = (utc_seconds(times[0]), utc_seconds(times[1]))
    if span[0] > span[1]:
        raise ValueError("times must be (first, last), first no later than last")
    return span
