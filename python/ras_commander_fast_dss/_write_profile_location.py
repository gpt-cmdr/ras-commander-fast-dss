"""Profile and location writers.

``write_profile`` stores a regular-interval profile, DSS7 only: a time x depth matrix,
never flattened (``float32`` makes RTS-PROFILE 102, ``float64`` RTD-PROFILE 107). DSS6 has
no profiles (heclib ``ztsStore.c``: "Profiles are not implemented in version 6").

``write_location`` stores location data. On DSS7 that is the location record
``/A/B/Location Info////`` (type 20). DSS6 has no location records: coordinates and codes
live in the time-series headers and the supplemental text in the user header,
so the call updates every block of an existing time-series dataset.

Format notes: ``docs/format/dss7.md`` §11 and ``docs/format/dss6.md`` §13.
"""

from __future__ import annotations

import os
import warnings
from pathlib import Path
from typing import Any, Dict, Optional, Union

import numpy as np

from . import _native
from ._time import TimeLike, to_hec_seconds
from ._write import HEC_UNDEFINED_FLOAT, _precision, _refuse_tz, _sync_mode, _held, _file_version
from ._write_pd import _cast, _numeric, _text
from .exceptions import DssUnsupportedRecordError

__all__ = ["write_profile", "write_location"]

PathLike = Union[str, os.PathLike]
TextLike = Union[str, bytes]


def _warn_undetected(report: Any, dss_file: PathLike) -> None:
    if report.get("readers_undetected"):
        warnings.warn(
            f"{dss_file}: this DSS file uses HEC's old lock scheme, which cannot show "
            "whether HEC software has it open; the write held the HEC write lock but was "
            "not checked against HEC readers (pass allow_undetectable_readers=False to "
            "refuse such files)",
            RuntimeWarning,
            stacklevel=3,
        )


def _quality(quality: Any, n: int) -> Optional[np.ndarray]:
    if quality is None:
        return None
    qa = np.asarray(quality)
    if qa.dtype.kind not in "iu":
        raise ValueError("quality flags must be integers")
    if qa.ndim != 1 or len(qa) != n:
        raise ValueError("quality must be 1-D with one flag per time")
    lim = np.iinfo(np.int32)
    if qa.size and (int(qa.max()) > lim.max or int(qa.min()) < lim.min):
        raise ValueError("quality flags must fit in 32 bits")
    return qa.astype(np.int32)


def write_profile(
    dss_file: PathLike,
    pathname: str,
    depths: Any,
    values: Any,
    *,
    start: TimeLike,
    depth_units: str,
    value_units: str,
    data_type: str,
    quality: Any = None,
    time_zone: Optional[str] = None,
    precision: int = -1,
    value_type: str = "auto",
    raw: bool = False,
    allow_undetectable_readers: bool = True,
    sync: Union[bool, str] = "end",
) -> Dict[str, Any]:
    """Store a regular-interval profile in an existing DSS7 file.

    Parameters
    ----------
    dss_file:
        Existing DSS7 file. A DSS6 file raises ``DssUnsupportedRecordError`` unchanged.
    pathname:
        ``/A/B/C/D/E/F/``; E a standard regular interval; D is replaced per block.
    depths:
        The depth axis, 1-D, ``n_depths`` values. Every block of a dataset has one axis:
        a write whose depths differ from a block it must merge with or leaves in place is
        refused, because HEC readers would misalign the rows.
    values:
        2-D, shape ``(n_times, n_depths)``. Anything else is ``ValueError``; a profile is
        never flattened or reshaped.
    start:
        Time of the first row (as for ``write_timeseries``); rows follow at the interval.
    depth_units, value_units, data_type:
        The two unit strings and the HEC data type.
    quality:
        One integer per time, or ``None``.
    value_type:
        ``"auto"`` stores ``float32`` (102) when both arrays are float32, otherwise
        ``float64`` (107); ``"float"`` or ``"double"`` choose. An existing block keeps its
        width: a write of the other width is refused.
    raw:
        By default ``NaN`` values are stored as HEC's missing flag (-FLT_MAX), and ``NaN``
        or infinite depths are refused. ``raw=True`` stores every bit as given.
    sync:
        Flush policy, as for :func:`ras_commander_fast_dss.write_timeseries`: ``"end"``
        (default; ``True`` means the same) flushes once at the end of this call;
        ``"step"`` also flushes at every flush point of the transaction (the default
        before 0.1.2); ``"none"`` (or ``False``) does not flush. All three make the same
        writes in the same order, so a write interrupted by the death of the writing
        process is reconciled by the next write in the same way. No power-loss claim is
        made for any mode. For a damaged DSS7 file use
        :func:`ras_commander_fast_dss.repair`.

    Returns
    -------
    dict
        The write report, as for ``write_timeseries``.

    Notes
    -----
    Storage follows heclib's storage flag 0: new rows replace stored ones at their times,
    the rest of a block is kept, and leading and trailing all-missing rows are not stored.
    A rewrite drops a stored user header, as heclib does; the location record
    (``/A/B/Location Info////``) is a separate record and is not touched.
    """
    if not isinstance(pathname, str):
        raise TypeError(f"pathname must be str, not {type(pathname).__name__}")
    if _file_version(dss_file) == 6:
        raise DssUnsupportedRecordError(
            f"{dss_file}: DSS6 files cannot store profiles (heclib ztsStore.c: profiles are "
            "not implemented in version 6)"
        )
    da = _numeric(depths, "depths")
    va = _numeric(values, "values")
    if da.ndim != 1 or da.size == 0:
        raise ValueError("depths must be one-dimensional and not empty")
    if va.ndim != 2:
        raise ValueError(
            f"values must be 2-D with shape (n_times, n_depths), not {va.ndim}-D; a profile "
            "is never flattened"
        )
    if va.shape[1] != da.size:
        raise ValueError(f"values has {va.shape[1]} columns for {da.size} depths")
    if va.shape[0] == 0:
        raise ValueError("no times")
    if value_type == "auto":
        dtype = np.float32 if (da.dtype == np.float32 and va.dtype == np.float32) else np.float64
    elif value_type == "float":
        dtype = np.float32
    elif value_type == "double":
        dtype = np.float64
    else:
        raise ValueError('value_type must be "auto", "float" or "double"')
    if not raw and not np.isfinite(da.astype(np.float64)).all():
        raise ValueError("depths contain NaN or infinity (pass raw=True to store the bits)")
    d = _cast(da, dtype, "depths", raw, HEC_UNDEFINED_FLOAT)
    v = _cast(np.ascontiguousarray(va).ravel(), dtype, "values", raw, HEC_UNDEFINED_FLOAT)
    q = _quality(quality, va.shape[0])
    _refuse_tz(start, "start")
    if time_zone is not None:
        time_zone = _text(time_zone, "time_zone")
    report = _native.write_profile(
        str(Path(os.fspath(dss_file))),
        pathname,
        to_hec_seconds(start),
        d,
        v,
        _text(depth_units, "depth_units"),
        _text(value_units, "value_units"),
        _text(data_type, "data_type"),
        q,
        time_zone,
        _precision(precision),
        bool(allow_undetectable_readers),
        sync=_sync_mode(sync),
        **_held(),
    )
    _warn_undetected(report, dss_file)
    return dict(report)


def _encode(s: Optional[TextLike], what: str) -> Optional[bytes]:
    if s is None or isinstance(s, bytes):
        return s
    if not isinstance(s, str):
        raise TypeError(f"{what} must be str or bytes, not {type(s).__name__}")
    # Monolith passes text through JNI as UTF-8 bytes; P28 reads DSS6 text as UTF-8.
    return s.encode("utf-8")


def write_location(
    dss_file: PathLike,
    pathname: str,
    *,
    x: Optional[float] = None,
    y: Optional[float] = None,
    z: Optional[float] = None,
    coordinate_system: Optional[int] = None,
    coordinate_id: Optional[int] = None,
    horizontal_units: Optional[int] = None,
    horizontal_datum: Optional[int] = None,
    vertical_units: Optional[int] = None,
    vertical_datum: Optional[int] = None,
    time_zone: Optional[TextLike] = None,
    supplemental: Optional[TextLike] = None,
    keep_existing: bool = True,
    allow_undetectable_readers: bool = True,
    sync: Union[bool, str] = "end",
) -> Dict[str, Any]:
    """Store location data for ``pathname``, in the file's DSS version.

    Parameters
    ----------
    dss_file:
        Existing DSS7 or DSS6 file.
    pathname:
        DSS7: any pathname of the location (only A and B are used); the record written is
        ``/A/B/Location Info////``. DSS6: a time-series pathname (any D part); every block
        with the same A, B, C, E and F parts is rewritten, so that all report one location.
        ``DssUnsupportedRecordError`` when DSS6 has no such time series.
    x, y, z, coordinate_system, coordinate_id, horizontal_units, horizontal_datum,
    vertical_units, vertical_datum:
        The fields of HEC's location struct (codes are not validated, as heclib does not).
    time_zone:
        DSS7 only (at most 71 bytes). DSS6 has no such location field: refused.
    supplemental:
        Text without NUL bytes. DSS7: the record's text. DSS6: the user header, at most
        2,000 bytes. Text is informational: a datum named in it is never applied.
    keep_existing:
        ``True`` (default): a field left ``None`` keeps what is stored, so a partial update
        loses nothing. ``False``: every field left ``None`` is cleared, as heclib stores a
        whole struct.
    sync:
        Flush policy, as for :func:`ras_commander_fast_dss.write_timeseries`: ``"end"``
        (default; ``True`` means the same) flushes once at the end of this call;
        ``"step"`` also flushes at every flush point of the transaction (the default
        before 0.1.2); ``"none"`` (or ``False``) does not flush. All three make the same
        writes in the same order, so a write interrupted by the death of the writing
        process is reconciled by the next write in the same way. No power-loss claim is
        made for any mode. For a damaged DSS7 file use
        :func:`ras_commander_fast_dss.repair`.

    Returns
    -------
    dict
        The write report, as for ``write_timeseries``.

    Raises
    ------
    ValueError
        Nothing to store (heclib would silently store nothing); or on DSS6 all coordinates
        zero, which readers report as no location.
    """
    if not isinstance(pathname, str):
        raise TypeError(f"pathname must be str, not {type(pathname).__name__}")
    floats = {}
    for name, val in (("x", x), ("y", y), ("z", z)):
        floats[name] = None if val is None else float(val)
    report = _native.write_location(
        str(Path(os.fspath(dss_file))),
        pathname,
        floats["x"],
        floats["y"],
        floats["z"],
        coordinate_system,
        coordinate_id,
        horizontal_units,
        horizontal_datum,
        vertical_units,
        vertical_datum,
        _encode(time_zone, "time_zone"),
        _encode(supplemental, "supplemental"),
        bool(keep_existing),
        _file_version(dss_file),
        bool(allow_undetectable_readers),
        sync=_sync_mode(sync),
        **_held(),
    )
    _warn_undetected(report, dss_file)
    return dict(report)
