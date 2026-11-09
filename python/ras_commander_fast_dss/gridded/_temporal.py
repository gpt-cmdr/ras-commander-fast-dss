"""Gridded time tools over ``_native``. Argument checks live here; rules live in the engine.

Both tools read a DSS file and write another. ``dss_vortex::temporal`` gives the rules and their
sources (HEC-Vortex 1.1.0 and 0.13.3); ``tests/python/test_gridded_time_tools.py`` checks them.
"""

from __future__ import annotations

import datetime as _dt
import os
import warnings
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Callable, Dict, List, Mapping, Optional, Sequence, Tuple, Union

import numpy as np

from .. import _native

PathLike = Union[str, os.PathLike]
TimeText = Union[str, _dt.datetime]

_PART_KEYS = {"A", "B", "C", "F"}
_MONTHS = ("JAN", "FEB", "MAR", "APR", "MAY", "JUN", "JUL", "AUG", "SEP", "OCT", "NOV", "DEC")
_MARKER = np.float32(-3.4028235e38)


@dataclass
class TimeToolResult:
    """What a time tool wrote.

    ``records`` holds one dict per record: ``source`` (the source record, or the series
    ``/A/B/C/*/*/F/``), ``source_type``, ``every_cell_missing``, ``pathname``, ``record_type``,
    ``outcome`` (``"added"``, ``"replaced"``, ``"unchanged"``), ``data_type``, ``units``, ``nx``, ``ny``,
    ``lower_left_cell_x``, ``lower_left_cell_y``, ``cell_size`` and ``readers_undetected``.

    ``skipped`` lists ``(series, reason)`` pairs Vortex also writes nothing for. ``dropped_tail_seconds``
    is the time after the last whole window, summed over series: a volume there is **not in the output**.
    ``windows_every_cell_missing`` counts records written all missing because the window begins before
    the series.
    """

    output: str
    records: List[Dict[str, Any]] = field(default_factory=list)
    skipped: List[Tuple[str, str]] = field(default_factory=list)
    dropped_tail_seconds: int = 0
    windows_every_cell_missing: int = 0

    @property
    def pathnames(self) -> List[str]:
        return [r["pathname"] for r in self.records]


@dataclass
class WindowGrid:
    """One grid from a series. ``data`` has shape ``(ny, nx)``, rows south to north, as DSS stores them."""

    data: np.ndarray
    nx: int
    ny: int
    start: str
    end: str
    data_type: str
    units: str
    cell_size: float
    lower_left_cell_x: int
    lower_left_cell_y: int


def _path(p: PathLike) -> str:
    return str(Path(os.fspath(p)))


def _parts(parts: Optional[Mapping[str, str]]) -> Dict[str, Any]:
    parts = dict(parts or {})
    unknown = set(parts) - _PART_KEYS
    if unknown:
        raise ValueError(
            f"parts may hold A, B, C and F, not {sorted(unknown)}: D and E always come from each "
            "record's window"
        )
    for key, value in parts.items():
        if not isinstance(value, str):
            raise ValueError(f"part {key} must be str, not {type(value).__name__}")
    return dict(
        part_a=parts.get("A", ""),
        part_b=parts.get("B", ""),
        part_c=parts.get("C"),
        part_f=parts.get("F", ""),
    )


def _select(select: Union[None, str, Sequence[str]]) -> Optional[List[str]]:
    if select is None:
        return None
    if isinstance(select, str):
        return [select]
    chosen = [str(s) for s in select]
    if not chosen:
        raise ValueError("select is empty: pass None for every grid, or name at least one record")
    return chosen


def _seconds(value: _dt.timedelta, what: str) -> int:
    if not isinstance(value, _dt.timedelta):
        raise ValueError(f"{what} must be a datetime.timedelta, not {type(value).__name__}")
    if value.microseconds:
        raise ValueError(f"{what} holds a fraction of a second")
    return value.days * 86400 + value.seconds


def _stamp(t: TimeText, what: str) -> str:
    if isinstance(t, str):
        return t
    if isinstance(t, _dt.datetime):
        if t.tzinfo is not None:
            raise ValueError(f"{what} must be naive: DSS stamps state no zone")
        if t.second or t.microsecond:
            raise ValueError(f"{what} must be a whole minute")
        return f"{t.day:02}{_MONTHS[t.month - 1]}{t.year:04}:{t.hour:02}{t.minute:02}"
    raise ValueError(f"{what} must be a str like 01JAN2024:2400, or a datetime")


def _warn_readers(output: str, records) -> None:
    if any(r.get("readers_undetected") for r in records):
        warnings.warn(
            f"{output}: this DSS file uses HEC's old lock scheme, which cannot show whether HEC "
            "software has it open; the run held the HEC write lock but was not checked against "
            "HEC readers (pass allow_undetectable_readers=False to refuse such files)",
            RuntimeWarning,
            stacklevel=3,
        )


def _result(out: str, d: Dict[str, Any]) -> TimeToolResult:
    records = [dict(r) for r in d["records"]]
    _warn_readers(out, records)
    return TimeToolResult(
        output=out,
        records=records,
        skipped=[tuple(s) for s in d["skipped"]],
        dropped_tail_seconds=int(d["dropped_tail_seconds"]),
        windows_every_cell_missing=int(d["windows_every_cell_missing"]),
    )


def shift_times(
    input: PathLike,
    output: PathLike,
    *,
    shift: Optional[_dt.timedelta] = None,
    shift_start: Optional[_dt.timedelta] = None,
    shift_end: Optional[_dt.timedelta] = None,
    select: Union[None, str, Sequence[str]] = None,
    parts: Optional[Mapping[str, str]] = None,
    units: Optional[str] = None,
    data_type: Optional[str] = None,
    version: Optional[int] = None,
    sync: bool = True,
    allow_undetectable_readers: bool = True,
    progress: Optional[Callable[[int, int], Any]] = None,
) -> TimeToolResult:
    """Move grid records in time, replacing Vortex's time shifter. Cells keep their bits, except for
    what writing a grid always does: unit conversion and the missing marker.

    The result matches Vortex 1.1.0 and 0.13.3 on the compared cases. Stamps have no
    zone: a shift is plain arithmetic, also across a date on which some place changes its clocks.

    Parameters
    ----------
    shift:
        Moves both ends. Give this, or ``shift_start`` and ``shift_end``, in whole minutes. Moving the ends
        apart makes an instant a period; bringing them together makes a period an instant. A record that
        would end before it starts is refused, and so is a shift of nothing.
    select:
        Pathnames, or ``None`` for every grid. A name not in the file is refused.
    parts:
        A, B, C and F of the output. **A source's A and B parts are not kept**, as in Vortex: unstated,
        they are empty. ``F`` of ``"*"`` keeps the source's F part. ``A`` or ``B`` of ``"*"`` is refused.
    data_type:
        The output's type. Unstated, it follows Vortex's rule from the C part and the window, not the
        source's type: a period is PER-CUM under PRECIPITATION and PER-AVER otherwise, and an instant is
        INST-VAL. So an INST-CUM or a non-precipitation PER-CUM source changes type; compare
        ``source_type`` and ``data_type`` in the records. Precipitation typed INST-CUM needs this stated.

    The run is planned first, reading each selected record once, so a refusal leaves the output untouched
    and does not create it. A failure while writing keeps finished records and never leaves a partial one;
    the exception's ``completed`` lists the writes the writer acknowledged. ``in_doubt`` names the
    pathname being written if the writer failed: it may or may not be in the file, whole, so read the
    file, or run again, which replaces it. A new file is created just before its first record.
    The input cannot be the output.
    """
    if shift is not None and (shift_start is not None or shift_end is not None):
        raise ValueError("give shift, or shift_start and shift_end, not both")
    if shift is not None:
        start = end = _seconds(shift, "shift")
    else:
        if shift_start is None and shift_end is None:
            raise ValueError("give shift, or at least one of shift_start, shift_end")
        start = 0 if shift_start is None else _seconds(shift_start, "shift_start")
        end = 0 if shift_end is None else _seconds(shift_end, "shift_end")
    if start == 0 and end == 0:
        raise ValueError("the shift is zero, so there is nothing to do")
    if isinstance(version, bool) or version not in (None, 6, 7):
        raise ValueError(f"version must be 6 or 7, not {version!r}")
    out = _path(output)
    d = _native.shift_grid_times(
        _path(input),
        out,
        start,
        end,
        select=_select(select),
        units=units,
        data_type=data_type,
        version=version,
        sync=bool(sync),
        allow_undetectable_readers=bool(allow_undetectable_readers),
        progress=progress,
        **_parts(parts),
    )
    return _result(out, d)


def resample_times(
    input: PathLike,
    output: PathLike,
    step_minutes: int,
    *,
    rule: str = "strict",
    select: Union[None, str, Sequence[str]] = None,
    parts: Optional[Mapping[str, str]] = None,
    units: Optional[str] = None,
    data_type: Optional[str] = None,
    version: Optional[int] = None,
    sync: bool = True,
    allow_undetectable_readers: bool = True,
    progress: Optional[Callable[[int, int], Any]] = None,
) -> TimeToolResult:
    """Resample each series to a fixed step, replacing Vortex's time-step resampler.

    **How windows fall.** They start at midnight of the day holding the series' first time, not at that
    time, and have one fixed length: ``time_steps()`` lists them, and ``10080``, ``43200`` and ``525600``
    are 7, 30 and 365 days, not calendar periods. A window beginning before the series is written with
    every cell missing. A window that would run past the series is not written, and the time it leaves
    out is ``dropped_tail_seconds``. Both are what Vortex does, and both lose time at the ends.

    **What a cell holds.** PER-CUM: each source's depth times the share of that source in the window, so
    complete windows keep the volume. PER-AVER: each source's value times the share of the window it covers.
    INST-VAL: the trapezoid between neighbours. A cell missing in any source a window uses is missing there,
    never zero. Where sources overlap, earlier ends and then shorter records win, as in Vortex.

    **What the output's CRS is.** SHG stays Albers. Any other NAD83 Albers grid becomes a specified grid
    holding GDAL's text, as in Vortex, for both tools. Another datum is refused.

    **What the output's type is.** Vortex's rule on the C part, as for :func:`shift_times`. INST-VAL input
    gives PER-AVER records, or PER-CUM under PRECIPITATION.

    Parameters
    ----------
    rule:
        ``"strict"`` refuses a window with a gap inside it, and stores a missing INST-VAL cell as missing.
        ``"vortex"`` reproduces Vortex in both: the sources present are summed, so such a window holds a
        partial volume or an average with nothing for the gap, and a missing INST-VAL cell holds about
        ``-3.4e38 / minutes``, which readers take as a value.
    select:
        Pathnames, or ``None`` for every grid. One pathname selects its whole series: all records
        sharing its A, B, C and F parts, as in Vortex.
    parts:
        As for :func:`shift_times`.

    A series whose records hold no time is skipped and listed in ``skipped``, as Vortex skips it. An INST-CUM
    series takes its rule from its first record: an instant is INST-VAL's rule, and a period PER-CUM's under
    PRECIPITATION and PER-AVER's otherwise, as Vortex 1.1.0 infers (0.13.3 writes nothing for most such
    periods). A series whose records differ in grid, CRS, units or type, a series mixing timed records and
    untimed ones, one window stated twice, and a run with no complete window are refused. The run is planned
    first, as for :func:`shift_times`.
    """
    if isinstance(step_minutes, bool) or not isinstance(step_minutes, (int, np.integer)):
        raise ValueError(f"step_minutes must be an int, not {step_minutes!r}")
    if isinstance(version, bool) or version not in (None, 6, 7):
        raise ValueError(f"version must be 6 or 7, not {version!r}")
    out = _path(output)
    d = _native.resample_grid_times(
        _path(input),
        out,
        int(step_minutes),
        rule=rule,
        select=_select(select),
        units=units,
        data_type=data_type,
        version=version,
        sync=bool(sync),
        allow_undetectable_readers=bool(allow_undetectable_readers),
        progress=progress,
        **_parts(parts),
    )
    return _result(out, d)


def time_steps() -> List[int]:
    """The steps :func:`resample_times` takes, in minutes (Vortex's ``TimeStep``)."""
    return list(_native.grid_time_steps())


def _grid(d: Mapping[str, Any], missing: str) -> WindowGrid:
    if missing not in ("nan", "raw"):
        raise ValueError(f"missing must be nan or raw, not {missing!r}")
    nx, ny = int(d["nx"]), int(d["ny"])
    data = np.frombuffer(d["cells"], dtype="<f4").reshape(ny, nx)
    if missing == "nan":
        data = np.where(data == _MARKER, np.float32(np.nan), data)
    else:
        data = data.copy()
    return WindowGrid(
        data=data,
        nx=nx,
        ny=ny,
        start=d["start"],
        end=d["end"],
        data_type=d["data_type"],
        units=d["units"],
        cell_size=float(d["cell_size"]),
        lower_left_cell_x=int(d["lower_left_cell_x"]),
        lower_left_cell_y=int(d["lower_left_cell_y"]),
    )


def read_window(
    input: PathLike,
    pathname: str,
    start: TimeText,
    end: Optional[TimeText] = None,
    *,
    rule: str = "strict",
    missing: str = "nan",
) -> WindowGrid:
    """One window of the series holding ``pathname``, computed as :func:`resample_times` computes it.

    ``end=None`` asks for an instant. A window the series does not span comes back all missing. Units stay
    as stored. ``missing="nan"`` maps ``-FLT_MAX`` to NaN; ``"raw"`` keeps stored bits. -901 and -902 are
    ordinary values to this tool, as they are to Vortex's.
    ``rule`` is :func:`resample_times`'s, and ``"strict"`` also refuses a period series read at an instant.
    """
    first = _stamp(start, "start")
    last = first if end is None else _stamp(end, "end")
    return _grid(_native.read_grid_window(_path(input), str(pathname), first, last, rule=rule), missing)


def read_nearest(input: PathLike, pathname: str, time: TimeText, *, missing: str = "nan") -> WindowGrid:
    """The record nearest ``time``, by start time, then the shorter, then the earlier."""
    return _grid(_native.read_grid_nearest(_path(input), str(pathname), _stamp(time, "time")), missing)


def read_min_max(
    input: PathLike, pathname: str, start: TimeText, end: TimeText, *, missing: str = "nan"
) -> Tuple[WindowGrid, WindowGrid]:
    """The (minimum, maximum) grids over a window. For INST-VAL, the last instant found is left out and
    so are instants before the window, as in Vortex. A cell no record holds has a minimum of ``FLT_MAX``."""
    lo, hi = _native.read_grid_min_max(_path(input), str(pathname), _stamp(start, "start"), _stamp(end, "end"))
    return _grid(lo, missing), _grid(hi, missing)


def series_info(input: PathLike, pathname: str) -> Dict[str, Any]:
    """What a series states: ``series``, ``records``, ``start``, ``end``, ``data_type``, ``units`` and its grid.

    ``end`` is what Vortex's interval tree reports. With records of mixed lengths it can be earlier than the
    latest end, and :func:`resample_times` stops there too."""
    return dict(_native.grid_series_info(_path(input), str(pathname)))
