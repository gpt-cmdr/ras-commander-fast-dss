"""Paired-data writer, DSS7 and DSS6.

``write_paired_data`` stores one paired-data record (rating curves, stage-damage and
frequency curves, HEC-HMS storage-outflow and Atlas 14 temporal tables): a set of
ordinates (X, the independent variable) and one or more curves (Y, the dependent
variable). ``float32`` input makes a PD (200) record and ``float64`` a PDD (205) record;
the width given is stored, never converted.

Curve labels are the names of the curves; ``x_type`` and ``y_type`` are the axis types
(``Linear``, ``Log``, ``Probability``, ...). They are separate arguments and never
filled from one another (hms-commander's Java writer passes axis labels as the types).

Every write tests the HEC lock words first and holds the write lock while writing, as
the time-series writers do; a write refused for any reason leaves the file unchanged.
"""

from __future__ import annotations

import os
import warnings
from pathlib import Path
from typing import Any, Dict, Optional, Sequence, Union

import numpy as np
import pandas as pd

from . import _native
from ._write import HEC_MISSING_DSS6, HEC_UNDEFINED_FLOAT, _precision, _sync_flag

__all__ = ["write_paired_data"]

PathLike = Union[str, os.PathLike]


def _numeric(a: Any, what: str) -> np.ndarray:
    if isinstance(a, (pd.Series, pd.DataFrame)):
        a = a.to_numpy()
    arr = np.asarray(a)
    if arr.dtype.kind not in "fiub":
        raise ValueError(f"{what} must be numeric, not {arr.dtype}")
    return arr


def _cast(arr: np.ndarray, dtype: Any, what: str, raw: bool, missing: float) -> np.ndarray:
    """Convert to the stored width, refusing silent damage (as the time-series writers):
    float overflow to infinity, integers the width cannot hold exactly, and infinities
    unless ``raw``. ``NaN`` becomes ``missing`` unless ``raw``."""
    with np.errstate(over="ignore", invalid="ignore"):
        out = np.ascontiguousarray(arr, dtype=dtype).copy()
    if arr.dtype.kind in "iub":
        limit = 2**24 if dtype == np.float32 else 2**53
        if arr.size and ((arr > limit).any() or (arr.dtype.kind == "i" and (arr < -limit).any())):
            raise ValueError(
                f"integer {what} do not fit {np.dtype(dtype).name} exactly; convert them first"
            )
    elif arr.dtype.kind == "f":
        src = arr.astype(np.float64)
        bad = np.isinf(out) & np.isfinite(src)
        if bad.any():
            i = int(np.nonzero(bad.ravel())[0][0])
            raise ValueError(
                f"{what}[{i}] ({src.ravel()[i]!r}) overflows {np.dtype(dtype).name}; "
                "use value_type='double'"
            )
    if not raw:
        if np.isinf(out).any():
            raise ValueError(
                f"{what} contain infinity; HEC has no infinity convention (pass raw=True to "
                "store the bits as given)"
            )
        out[np.isnan(out)] = out.dtype.type(missing)
    return out


def _labels(labels: Any, n_curves: int) -> Optional[list]:
    if labels is None:
        return None
    if isinstance(labels, (str, bytes)):
        raise ValueError("labels must be a sequence of strings, one per curve")
    out = []
    for i, x in enumerate(labels):
        if not isinstance(x, str):
            raise ValueError(f"labels[{i}] must be str, not {type(x).__name__}")
        if not x.isascii():
            raise ValueError(f"labels[{i}] is not ASCII; HEC stores 8-bit text")
        out.append(x)
    if len(out) != n_curves:
        raise ValueError(f"{len(out)} labels for {n_curves} curves (give one per curve)")
    return out


def _text(s: Any, what: str) -> str:
    if not isinstance(s, str):
        raise TypeError(f"{what} must be str, not {type(s).__name__}")
    if not s.isascii():
        raise ValueError(f"{what} is not ASCII")
    return s


def write_paired_data(
    dss_file: PathLike,
    pathname: str,
    x: Any,
    y: Any,
    *,
    x_units: str,
    x_type: str,
    y_units: str,
    y_type: str,
    labels: Optional[Sequence[str]] = None,
    value_type: str = "auto",
    raw: bool = False,
    x_precision: int = -1,
    y_precision: int = -1,
    independent_is_x_axis: Union[bool, int] = True,
    time_zone: Optional[str] = None,
    version: int = 7,
    allow_undetectable_readers: bool = True,
    sync: bool = True,
) -> Dict[str, Any]:
    """Store one paired-data record (a new record, or a full replacement).

    Parameters
    ----------
    dss_file:
        Existing DSS file (:func:`ras_commander_fast_dss.create`). Write only to files you own;
        never to a file HEC software has open.
    pathname:
        ``/A/B/C/D/E/F/`` with exactly 7 slashes; C is usually ``<X>-<Y>`` (for example
        ``STAGE-FLOW``). D, E and F are free text. Invalid pathnames (8 slashes, control
        bytes, parts over 128 bytes) raise ``ValueError``; nothing is truncated or
        cleaned. DSS6 stores the pathname in upper case, as HEC does.
    x:
        The ordinates (independent variable), 1-D.
    y:
        The curves: 1-D for one curve, or 2-D with shape ``(len(x), n_curves)``, one
        column per curve (a ``DataFrame``'s columns are curves; its column names are not
        used as labels).
    x_units, x_type, y_units, y_type:
        Units and axis types (``Linear``, ``Log``, ``Probability``, ...) of X and Y.
        DSS6 stores at most 8 characters each (longer is ``ValueError``).
    labels:
        Optional curve names, one per curve. DSS6: at most 100 characters each, no
        trailing blanks, and at least one label non-blank in its first 8 characters
        (DSS6 readers return no labels otherwise).
    value_type:
        ``"auto"`` stores ``float32`` (PD, 200) when both ``x`` and ``y`` are float32
        and ``float64`` (PDD, 205) otherwise; ``"float"`` or ``"double"`` choose.
        ``"float"`` rounds float64 input to the nearest float32 without a warning: digits
        beyond float32 precision are lost and magnitudes below about 1.2e-38 become
        subnormal or 0. Overflow to infinity and integers that the width cannot hold
        exactly are refused. Use ``"auto"`` or ``"double"`` to keep float64 values exact.
    raw:
        By default ``NaN`` is stored as HEC's missing flag (DSS7: -FLT_MAX; DSS6: -901)
        and infinities are refused. ``raw=True`` stores every value's bits as given.
    x_precision, y_precision:
        Decimal places for display, 0-9 each, or both -1 (unset, the default).
    independent_is_x_axis:
        ``True`` (default) stores 1; ``False`` stores 0 (DSS7 only; DSS6 stores 1 or 2,
        pass the integer 2). An integer is stored as given.
    time_zone:
        Record time-zone name (DSS7 only).
    version:
        6 or 7: the version of ``dss_file``. It must match the file: the DSS7 writer
        refuses a DSS6 file and the reverse, leaving it unchanged.
        ``DssFile.write_paired_data`` passes the open file's version and is the
        version-safe call.
    allow_undetectable_readers:
        As for :func:`ras_commander_fast_dss.write_timeseries`.
    sync:
        Flush policy, as for :func:`ras_commander_fast_dss.write_timeseries`: ``True``
        (default, unchanged) flushes at the transaction's existing flush points (the
        data is flushed together with the end-of-file flag) and ends with one
        ``sync_all``; ``False`` (opt-in) does not flush. ``False`` keeps the
        ordering of completed writes and of the recovery marker when the writing process
        dies, so the next write reconciles an interrupted one as with ``True``. No
        power-loss claim is made for either mode. For a damaged DSS7 file use
        :func:`ras_commander_fast_dss.repair`.

    Returns
    -------
    dict
        ``blocks``: one dict (``pathname``, ``outcome`` = ``"added"``, ``"replaced"`` or
        ``"unchanged"``, ``record_type``, ``number_data``, ``logical_number``);
        ``lock_method``; ``readers_undetected``; ``reconciled_interrupted_write``;
        ``version``.

    Notes
    -----
    A replacement writes the whole record (heclib's partial curve and ordinate stores
    are not supported) as a new copy at the end of the file; the old copy becomes dead
    space. Replacing a float record with doubles (or the reverse) changes the record
    type, as heclib does. An existing record of another type (a time series, a grid)
    at the pathname is refused; heclib would delete it.

    ``ELEV`` parameters (a C part such as ``ELEV-STORAGE``): no vertical-datum conversion
    is applied, as heclib applies none without a datum request. Two cases heclib treats
    specially are refused (``DssUnsupportedRecordError``): a ``|`` in an elevation-side
    unit (heclib reads ``U=<unit>|V=<datum>`` unit specifications), and, for DSS7, a
    location record (``/A/B/Location Info////``) holding vertical-datum information. A
    user header stored on an existing DSS7 ELEV record is dropped by a rewrite, as heclib
    drops it; DSS6 refuses that case (heclib keeps the header there).
    """
    if not isinstance(pathname, str):
        raise TypeError(f"pathname must be str, not {type(pathname).__name__}")
    if version not in (6, 7):
        raise ValueError(f"version must be 6 or 7, not {version!r}")
    xa = _numeric(x, "x")
    ya = _numeric(y, "y")
    if xa.ndim != 1:
        raise ValueError("x must be one-dimensional")
    if ya.ndim == 1:
        ya = ya.reshape(-1, 1)
    elif ya.ndim != 2:
        raise ValueError("y must be 1-D (one curve) or 2-D (one column per curve)")
    if len(xa) == 0:
        raise ValueError("no ordinates")
    if ya.shape[0] != len(xa):
        raise ValueError(
            f"y has {ya.shape[0]} rows for {len(xa)} ordinates (shape must be "
            "(len(x), n_curves))"
        )
    n_curves = ya.shape[1]
    if n_curves == 0:
        raise ValueError("y has no curves")
    if value_type == "auto":
        dtype = np.float32 if (xa.dtype == np.float32 and ya.dtype == np.float32) else np.float64
    elif value_type == "float":
        dtype = np.float32
    elif value_type == "double":
        dtype = np.float64
    else:
        raise ValueError('value_type must be "auto", "float" or "double"')
    missing = HEC_MISSING_DSS6 if version == 6 else HEC_UNDEFINED_FLOAT
    ords = _cast(xa, dtype, "x", raw, missing)
    # Curve-major, as HEC stores the curves: column 0, then column 1, ...
    vals = _cast(np.ascontiguousarray(ya.T).ravel(), dtype, "y", raw, missing)
    if isinstance(independent_is_x_axis, (bool, np.bool_)):
        axis = 1 if independent_is_x_axis else 0
    elif isinstance(independent_is_x_axis, (int, np.integer)):
        axis = int(independent_is_x_axis)
    else:
        raise ValueError("independent_is_x_axis must be a bool or an integer")
    if time_zone is not None:
        time_zone = _text(time_zone, "time_zone")
        if version == 6:
            raise ValueError("time_zone is not written to DSS6 files; pass time_zone=None")
    report = _native.write_pd(
        str(Path(os.fspath(dss_file))),
        pathname,
        ords,
        vals,
        int(n_curves),
        _text(x_units, "x_units"),
        _text(x_type, "x_type"),
        _text(y_units, "y_units"),
        _text(y_type, "y_type"),
        _labels(labels, n_curves),
        axis,
        _precision(x_precision),
        _precision(y_precision),
        time_zone,
        int(version),
        bool(allow_undetectable_readers),
        sync=_sync_flag(sync),
    )
    if report.get("readers_undetected"):
        warnings.warn(
            f"{dss_file}: this DSS file uses HEC's old lock scheme, which cannot show "
            "whether HEC software has it open; the write held the HEC write lock but was "
            "not checked against HEC readers (pass allow_undetectable_readers=False to "
            "refuse such files)",
            RuntimeWarning,
            stacklevel=2,
        )
    return dict(report)
