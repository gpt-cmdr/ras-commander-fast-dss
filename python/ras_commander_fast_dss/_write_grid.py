"""Grid writer for DSS7 and DSS6 files.

``write_grid`` stores one grid record: HRAP (410/411), Albers / SHG (420/421) or a
specified spatial reference (430/431). The record layout is the one HEC Monolith 3.3.27
writes through ``GriddedData.storeGriddedData`` (DSS7: heclib ``zspatialGridStore``;
DSS6: the Java ``GridInfoFlat`` header and ``zwritx``), with the statistics and range
table Monolith's ``GridData.updateStatistics`` computes. Format notes:
``docs/format/dss7.md`` and ``docs/format/dss6.md``.

Before every write the HEC lock words are tested; the write is refused with
:class:`~ras_commander_fast_dss.exceptions.DssLockedError` when any HEC process has the file open.
"""

from __future__ import annotations

import datetime as _dt
import os
import warnings
from pathlib import Path
from typing import Any, Dict, Mapping, Optional, Sequence, Tuple, Union

import numpy as np

from . import _native
from ._time import TimeLike, to_hec_seconds
from ._write import _sync_mode, _held

__all__ = ["write_grid", "GRID_NODATA", "SHG_WKT"]

PathLike = Union[str, os.PathLike]

#: HEC's grid no-data value (``GridInfo.GRID_NODATA_VALUE``, -FLT_MAX).
GRID_NODATA = float(-np.finfo(np.float32).max)

#: HEC's Standard Hydrologic Grid WKT (``zStructSpatialGrid.h`` ``SHG_SRC_DEFINITION``).
SHG_WKT = (
    'PROJCS["USA_Contiguous_Albers_Equal_Area_Conic_USGS_version",GEOGCS["GCS_North_American_1983",'
    'DATUM["D_North_American_1983",SPHEROID["GRS_1980",6378137.0,298.257222101]],'
    'PRIMEM["Greenwich",0.0],UNIT["Degree",0.0174532925199433]],PROJECTION["Albers"],'
    'PARAMETER["False_Easting",0.0],PARAMETER["False_Northing",0.0],'
    'PARAMETER["Central_Meridian",-96.0],PARAMETER["Standard_Parallel_1",29.5],'
    'PARAMETER["Standard_Parallel_2",45.5],PARAMETER["Latitude_Of_Origin",23.0],'
    'UNIT["Meter",1.0]]'
)

_DATA_TYPES = {"PER-AVER": 0, "PER-CUM": 1, "INST-VAL": 2, "INST-CUM": 3}
_MONTHS = ["JAN", "FEB", "MAR", "APR", "MAY", "JUN", "JUL", "AUG", "SEP", "OCT", "NOV", "DEC"]
_EPOCH = _dt.datetime(1899, 12, 31)
_SHG = dict(datum=2, units="METERS", first_standard_parallel=29.5,
            second_standard_parallel=45.5, central_meridian=-96.0,
            latitude_of_origin=23.0, false_easting=0.0, false_northing=0.0)
_ALBERS_KEYS = ("datum", "units", "first_standard_parallel", "second_standard_parallel",
                "central_meridian", "latitude_of_origin", "false_easting", "false_northing")
_DATUMS = {"UNDEFINED": 0, "NAD27": 1, "NAD83": 2}


def _grid_time(value: TimeLike, what: str, end: bool) -> str:
    """``DDMMMYYYY:HHMM`` of a naive time; an end at midnight is the previous day's
    ``2400`` (HEC's grid convention), a start at midnight ``0000``."""
    if isinstance(value, _dt.datetime) and value.tzinfo is not None:
        raise ValueError(f"{what} is time-zone aware; convert it to the model's clock first")
    if hasattr(value, "tzinfo") and getattr(value, "tzinfo", None) is not None:
        raise ValueError(f"{what} is time-zone aware; convert it to the model's clock first")
    secs = to_hec_seconds(value)
    if secs % 60:
        raise ValueError(f"{what} must be a whole minute (grid pathnames store minutes)")
    t = _EPOCH + _dt.timedelta(seconds=secs)
    if end and t.hour == 0 and t.minute == 0:
        d = t - _dt.timedelta(days=1)
        return f"{d.day:02d}{_MONTHS[d.month - 1]}{d.year:04d}:2400"
    return f"{t.day:02d}{_MONTHS[t.month - 1]}{t.year:04d}:{t.hour:02d}{t.minute:02d}"


def _f32(value: Any, what: str) -> float:
    """``value`` rounded to the nearest float32, as HEC stores header floats; refuses
    non-finite values and values whose float32 is not finite."""
    try:
        v = float(value)
    except (TypeError, ValueError):
        raise ValueError(f"{what} must be a number, not {value!r}") from None
    with np.errstate(over="ignore"):
        f = np.float32(v)
    if not np.isfinite(v) or not np.isfinite(f):
        raise ValueError(f"{what} must be finite and within float32 range, not {value!r}")
    return float(f)


def _cells(data: Any, north_up: bool, raw: bool) -> Tuple[np.ndarray, int, int]:
    mask = np.ma.getmaskarray(data) if np.ma.isMaskedArray(data) else None
    a = np.asarray(np.ma.getdata(data) if mask is not None else data)
    if a.ndim != 2:
        raise ValueError(f"data must be 2-D (rows, columns); got shape {a.shape}")
    if a.dtype.kind not in "fiub":
        raise ValueError(f"data must be numeric, not {a.dtype}")
    if a.dtype.kind in "iu":
        big = np.abs(a.astype(np.float64)) > 2 ** 24
        if big.any():
            raise ValueError("integer cells above 2**24 do not fit a float32 exactly")
    with np.errstate(over="ignore", invalid="ignore"):
        f = a.astype(np.float32)
    finite_in = np.isfinite(a.astype(np.float64)) if a.dtype.kind == "f" else np.ones(a.shape, bool)
    if (finite_in & ~np.isfinite(f)).any():
        raise ValueError("cells overflow float32")
    if not raw:
        if np.isinf(f).any():
            raise ValueError("cells are infinite; HEC grids have no infinity convention "
                             "(pass raw=True to store the bits as given)")
        f[np.isnan(f)] = np.float32(GRID_NODATA)
    if mask is not None:
        f[mask] = np.float32(GRID_NODATA)
    if north_up:
        f = f[::-1, :]
    ny, nx = f.shape
    return np.ascontiguousarray(f.reshape(-1), dtype=np.float32), nx, ny


def write_grid(
    dss_file: PathLike,
    pathname: str,
    data: Any,
    *,
    grid_type: str,
    cell_size: float,
    units: str,
    data_type: str,
    lower_left_cell: Optional[Sequence[int]] = None,
    origin: Optional[Sequence[float]] = None,
    cell_zero: Sequence[float] = (0.0, 0.0),
    start: Optional[TimeLike] = None,
    end: Optional[TimeLike] = None,
    north_up: bool = False,
    time_stamped: bool = True,
    compression: Optional[str] = None,
    scale_factor: Optional[float] = None,
    base: Optional[float] = None,
    albers: Optional[Mapping[str, Any]] = None,
    srs_definition: Optional[str] = None,
    srs_name: str = "",
    data_source: str = "",
    null_value: Optional[float] = None,
    time_zone: str = "UTC",
    time_zone_offset_ms: int = 0,
    is_interval: Optional[bool] = None,
    raw: bool = False,
    version: int = 7,
    allow_undetectable_readers: bool = True,
    sync: Union[bool, str] = "end",
) -> Dict[str, Any]:
    """Store one grid record in an existing DSS7 or DSS6 file.

    Parameters
    ----------
    dss_file:
        Existing file (:func:`create`). ``version`` must be its DSS version
        (``DssFile.write_grid`` passes it).
    pathname:
        ``/A/B/C/D/E/F/``. D is the period start (or the instant) and E the period end,
        both ``DDMMMYYYY:HHMM``; or leave D and E empty and give ``start=`` / ``end=``.
        PER-AVER and PER-CUM grids need an E part later than D (equal start and end is
        refused); INST-VAL and INST-CUM grids may leave E empty. Time-invariant grids
        (``time_stamped=False``) have empty D and E and no times. Their record type is
        what HEC writes and Monolith reads back: 431 for DSS7 specified grids, otherwise
        410/420/430 (HEC's DSS6 grid store writes no 4x1 type, and Monolith cannot read
        DSS7 411/421). Invalid
        pathnames raise ``ValueError``; nothing is truncated. DSS6 stores the pathname in
        upper case, as HEC does.
    data:
        2-D cells, shape ``(rows, columns)``. Row 0 is the **southern** row (HEC order,
        x varying fastest); pass ``north_up=True`` for a raster whose row 0 is north.
        ``NaN`` and masked cells are stored as HEC's no-data value
        (:data:`GRID_NODATA`, -FLT_MAX) unless ``raw=True``, for every grid type (also
        with a custom specified ``null_value``; readers treat both as missing). Cells are stored as float32
        (float64 input is rounded to the nearest float32; overflow is refused).
    grid_type:
        ``"shg"`` (Albers with HEC's Standard Hydrologic Grid parameters), ``"albers"``
        (give ``albers=``), ``"hrap"`` or ``"specified"`` (give ``srs_definition=``, a
        WKT string).
    cell_size:
        One cell size in CRS units (square cells).
    lower_left_cell / origin:
        Exactly one. ``lower_left_cell=(i, j)``: HEC cell indexes of the grid's
        lower-left cell, counted in cells from ``cell_zero``. ``origin=(x, y)``: the CRS
        coordinates of the grid's lower-left **corner** (not a cell centre); it must lie
        a whole number of cells from ``cell_zero``, tested on the float32 values that
        are stored (the index is never rounded).
    cell_zero:
        CRS coordinates of the lower-left corner of cell (0, 0) (Albers and specified;
        HRAP grids have a fixed origin and need ``(0, 0)``).
    units, data_type:
        Units (at most 12 characters on DSS6) and ``"PER-AVER"``, ``"PER-CUM"``,
        ``"INST-VAL"`` or ``"INST-CUM"``.
    compression:
        ``"zlib"`` (default; what Monolith writes to DSS7), ``"none"`` (DSS7 only) or
        ``"precip_2_byte"`` (DSS6 only; HEC's 15-bit precipitation code: values are
        stored as ``round((v - base) * scale_factor)``, so ordinary values are rounded to
        the nearest 1/scale step, which is accepted). Without ``scale_factor`` and
        ``base`` the writer chooses them as Monolith does (scale 100, base 0, adjusted
        for negative minima and large ranges). Changes of missing state are refused: a
        value cell that would be stored as missing (below the base, more than 32767
        steps above it, or NaN), or a missing cell that would be stored as a value.

    Floating-point metadata (``cell_size``, ``cell_zero``, ``origin``, ``null_value``,
    ``scale_factor``, ``base`` and the Albers parameters) is stored as float32, as HEC
    stores it: each value is rounded to the nearest float32. Values that are not finite,
    or whose float32 is not finite (beyond about 3.4e38), are refused.
    albers:
        For ``grid_type="albers"``: ``datum`` (``"NAD83"``, ``"NAD27"``, ``"UNDEFINED"``
        or 2/1/0), ``units``, ``first_standard_parallel``, ``second_standard_parallel``,
        ``central_meridian``, ``latitude_of_origin``, ``false_easting``,
        ``false_northing``.
    srs_definition, srs_name, null_value, time_zone, time_zone_offset_ms, is_interval:
        Specified grids: the WKT, its name, the no-data value of the cells (default
        -FLT_MAX), the time zone name and offset in milliseconds (default ``UTC``, 0) and
        whether the data are period data (default: PER-AVER/PER-CUM).
    data_source:
        HRAP grids: the data source (at most 12 characters on DSS6).
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
        ``pathname`` (as stored), ``record_type``, ``outcome`` (``"added"``,
        ``"replaced"`` or ``"unchanged"``), ``statistics`` (``min``, ``max``, ``mean``,
        ``range_limits``, ``range_counts`` as stored), ``lock_method``,
        ``readers_undetected``.

    Notes
    -----
    A record of another type at the pathname is refused (the writer does not change a
    record's type). Every check runs before the file is opened for writing; a refused
    write leaves the file unchanged. The write is one transaction ordered "data first,
    pointers last" (crash safety); not power-loss safe. On DSS7 the null value
    stored for HRAP and Albers grids is -FLT_MAX (Monolith stores 0.0 there, a value its
    readers ignore).
    """
    if not isinstance(pathname, str):
        raise TypeError(f"pathname must be str, not {type(pathname).__name__}")
    if version not in (6, 7):
        raise ValueError(f"version must be 6 or 7, not {version!r}")
    parts = pathname.split("/")
    if len(parts) != 8:
        raise ValueError(f"pathname must have exactly 7 slashes: {pathname!r}")
    if start is not None or end is not None:
        if parts[4] or parts[5]:
            raise ValueError("give D/E in the pathname or start=/end=, not both")
        if start is None:
            raise ValueError("end= needs start=")
        parts[4] = _grid_time(start, "start", False)
        parts[5] = "" if end is None else _grid_time(end, "end", True)
        pathname = "/".join(parts)
    dt = data_type.upper() if isinstance(data_type, str) else data_type
    if dt not in _DATA_TYPES:
        raise ValueError(f"data_type must be one of {sorted(_DATA_TYPES)}, not {data_type!r}")
    code = _DATA_TYPES[dt]
    cells, nx, ny = _cells(data, north_up, raw)
    x0, y0 = (_f32(cell_zero[0], "cell_zero x"), _f32(cell_zero[1], "cell_zero y"))
    cs = _f32(cell_size, "cell_size")
    if cs <= 0:
        raise ValueError("cell_size must be positive")
    if (lower_left_cell is None) == (origin is None):
        raise ValueError("give exactly one of lower_left_cell=(i, j) or origin=(x, y)")
    if origin is not None:
        ll = []
        for v, z, axis in ((origin[0], x0, "x"), (origin[1], y0, "y")):
            r = (_f32(v, "origin " + axis) - z) / cs
            k = round(r)
            if abs(r - k) > 1e-6:
                raise ValueError(
                    f"origin {axis} lies {r:.6f} cells from cell zero, not a whole number of "
                    "cells; give the lower-left corner (not a cell centre) or lower_left_cell"
                )
            ll.append(int(k))
    else:
        ll = [lower_left_cell[0], lower_left_cell[1]]
        for v in ll:
            if isinstance(v, bool) or not isinstance(v, (int, np.integer)):
                raise ValueError("lower_left_cell must be two integers")
        ll = [int(v) for v in ll]
    gt = grid_type.lower()
    albers_t = None
    kw: Dict[str, Any] = {}
    if gt in ("shg", "albers"):
        params = dict(_SHG) if gt == "shg" else dict(albers or {})
        if gt == "albers" and albers is None:
            raise ValueError('grid_type="albers" needs albers={...} (or use grid_type="shg")')
        missing = [k for k in _ALBERS_KEYS if k not in params]
        if missing:
            raise ValueError(f"albers parameters missing: {missing}")
        datum = params["datum"]
        if isinstance(datum, str):
            if datum.upper() not in _DATUMS:
                raise ValueError(f"albers datum must be NAD83, NAD27 or UNDEFINED, not {datum!r}")
            datum = _DATUMS[datum.upper()]
        albers_t = (int(datum), str(params["units"]),
                    *(_f32(params[k], "albers " + k) for k in _ALBERS_KEYS[2:]))
        native_type = "albers"
    elif gt == "hrap":
        native_type = "hrap"
    elif gt == "specified":
        if not srs_definition:
            raise ValueError('grid_type="specified" needs srs_definition= (WKT)')
        native_type = "specified"
        kw = dict(
            srs_name=srs_name,
            srs_definition=srs_definition,
            null_value=GRID_NODATA if null_value is None else _f32(null_value, "null_value"),
            time_zone_id=time_zone,
            time_zone_raw_offset=int(time_zone_offset_ms),
            is_interval=(code in (0, 1)) if is_interval is None else bool(is_interval),
        )
    else:
        raise ValueError(f'grid_type must be "shg", "albers", "hrap" or "specified", not {grid_type!r}')
    if gt != "specified" and null_value is not None:
        raise ValueError("null_value applies to specified grids (HRAP and Albers use -FLT_MAX)")
    comp = "zlib" if compression is None else str(compression).lower().replace("-", "_")
    if comp not in ("zlib", "none", "precip_2_byte"):
        raise ValueError(f'compression must be "zlib", "none" or "precip_2_byte", not {compression!r}')
    report = _native.write_grid(
        str(Path(os.fspath(dss_file))), pathname, cells, nx, ny, native_type,
        bool(time_stamped), ll[0], ll[1], cs, x0, y0, units, code, comp,
        scale_factor=None if scale_factor is None else _f32(scale_factor, "scale_factor"),
        base=None if base is None else _f32(base, "base"),
        data_source=data_source, albers=albers_t, version=int(version),
        allow_undetectable_readers=bool(allow_undetectable_readers), sync=_sync_mode(sync),
        **kw,
        **_held(),
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
