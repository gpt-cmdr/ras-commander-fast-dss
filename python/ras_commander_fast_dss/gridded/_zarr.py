"""A local Zarr v2 store as a gridded source (packet P49): the route for AORC v1.1.

No network: fetch the store's metadata and the chunks you need into a folder, and read the folder.
A chunk that is not in the folder reads as missing, never as zero.
"""

from __future__ import annotations

import datetime as _dt
import os
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Callable, Dict, List, Mapping, Optional, Sequence, Tuple, Union

import numpy as np

from .. import _native
from ._import import (
    ImportResult,
    PathLike,
    _bounds,
    _parts,
    _warn_accumulation,
    _warn_readers,
)

from ._select import UtcTime, source_box as _box, time_span as _span

#: A flag to stop a store's read from another thread: ``stop = ZarrStop()``, pass ``stop=stop``,
#: and call ``stop.set()``. The read ends with an error before its next chunk. ``clear()`` and
#: ``is_set()`` as :class:`threading.Event` has them.
ZarrStop = _native.ZarrStop


def zarr_limits() -> Dict[str, Any]:
    """The store reader's limits by name, with their defaults; ``limits=`` takes any of them.

    ``max_cells`` (a window's rows times columns), ``max_read_bytes``, ``max_metadata_bytes``,
    ``max_variables``, ``max_axis_points``, ``max_chunk_bytes``, ``max_chunks_per_read``,
    ``max_decoded_bytes_per_read``, ``chunk_cache_bytes``, ``max_seconds_per_read`` (``None`` for
    no time limit), ``max_cells_per_import`` (steps times cells an import may read, checked while
    it is planned) and ``refuse_absent_chunks`` (``True`` refuses a chunk with no file, where the
    default reads it as missing).
    """
    return dict(_native.zarr_limits())


def _index(value: Any, what: str) -> int:
    """A whole-number index, as given. `int()` alone would take 6288.9 as 6288, and True as 1."""
    if isinstance(value, bool) or int(value) != value:
        raise ValueError(f"{what} must be a whole number, not {value!r}")
    return int(value)


def _pair(values: Any, what: str) -> tuple:
    first, last = values
    return _index(first, what), _index(last, what)


def _limits(limits: Optional[Mapping[str, Any]], strict: bool) -> Optional[Dict[str, Any]]:
    if not isinstance(strict, bool):
        raise ValueError(f"strict must be True or False, not {strict!r}")
    given = dict(limits or {})
    for name, value in given.items():
        if isinstance(value, bool) and name != "refuse_absent_chunks":
            raise ValueError(f"limit {name} must be a number, not {value!r}")
    if strict:
        given["refuse_absent_chunks"] = True
    return given or None


def zarr_variables(path: PathLike) -> List[Dict[str, Any]]:
    """A store's variables, one dict each, reading metadata and axes only.

    Keys: ``name``; ``error``, why it is not read as a grid, or ``None``; ``steps``, ``ny``, ``nx``;
    ``units``; ``description``; ``crs``, the WKT the store states, or ``None`` when it states none
    (never assumed); ``accumulation_seconds``, set when the store itself states what each step is
    accumulated over (3600 for ``APCP_surface`` in a store recognized as AORC v1.1);
    ``period_note``, why a variable that claims to be AORC's has no period of its own, or ``None``;
    ``first_time`` and ``last_time``, seconds since 1970 UTC.
    """
    return [dict(v) for v in _native.zarr_variables(str(Path(os.fspath(path))))]


@dataclass
class ZarrWindow:
    """Cells read from a store.

    ``cells`` has shape ``(steps, ny, nx)``, ``float32``, rows in the store's order, with NaN for
    missing. ``transform`` is GDAL's six numbers for the window, whose row spacing is positive when
    rows run south to north. ``times`` are seconds since 1970 UTC. With ``accumulation_seconds``
    set, each step is the depth over that long **ending** at its time.
    """

    cells: np.ndarray
    transform: Tuple[float, ...]
    crs: Optional[str]
    nodata: Optional[float]
    times: List[Optional[int]]
    accumulation_seconds: Optional[int]
    units: str
    description: Optional[str]
    rows: Tuple[int, int]
    cols: Tuple[int, int]


def read_zarr(
    path: PathLike,
    variable: str,
    *,
    steps: Tuple[int, int],
    bbox: Optional[Sequence[float]] = None,
    rows: Optional[Tuple[int, int]] = None,
    cols: Optional[Tuple[int, int]] = None,
    limits: Optional[Mapping[str, Any]] = None,
    stop: Optional[Any] = None,
    strict: bool = False,
) -> ZarrWindow:
    """Read steps ``steps[0]`` up to ``steps[1]`` of a variable, on a window.

    ``bbox`` is ``(west, south, east, north)`` in the store's CRS and takes every cell whose extent
    it touches; ``rows`` and ``cols`` are index ranges instead. Only the chunks the window touches
    are opened. A stored value equal to the array's fill is missing, as ``xarray`` reads it; other
    values are ``float32(scale_factor * v + add_offset)`` computed in double.

    A chunk with no file reads as missing; ``strict=True`` refuses it instead, for a caller who
    needs the local copy to be complete over the window. ``limits`` replaces any of
    :func:`zarr_limits`, and ``stop`` is a :data:`ZarrStop`. Nothing below the store's folder is
    read through a link or junction. A hard link is an ordinary file and is read: use a store whose
    files you trust.
    """
    first, last = _pair(steps, "steps")
    d = _native.read_zarr(
        str(Path(os.fspath(path))),
        variable,
        first,
        last,
        bbox=_box(bbox),
        rows=None if rows is None else _pair(rows, "rows"),
        cols=None if cols is None else _pair(cols, "cols"),
        limits=_limits(limits, strict),
        stop=stop,
    )
    nx, ny = d["nx"], d["ny"]
    return ZarrWindow(
        cells=np.asarray(d["cells"]).reshape(last - first, ny, nx),
        transform=tuple(d["transform"]),
        crs=d["crs"],
        nodata=d["nodata"],
        times=list(d["times"]),
        accumulation_seconds=d["accumulation_seconds"],
        units=d["units"],
        description=d["description"],
        rows=tuple(d["rows"]),
        cols=tuple(d["cols"]),
    )


def import_zarr(
    inputs: Union[PathLike, Sequence[PathLike]],
    output: PathLike,
    *,
    variable: str,
    bbox: Optional[Sequence[float]] = None,
    times: Optional[Tuple[UtcTime, UtcTime]] = None,
    envelope: Optional[Sequence[float]] = None,
    envelope_crs: Optional[str] = None,
    shapefile: Optional[PathLike] = None,
    target_crs: Optional[str] = "SHG",
    cell_size: Optional[float] = 2000.0,
    method: str = "bilinear",
    parts: Optional[Mapping[str, str]] = None,
    units: Optional[str] = None,
    data_type: Optional[str] = None,
    version: Optional[int] = None,
    align: str = "refuse",
    window_rule: str = "refuse",
    duplicates: str = "refuse",
    threads: int = 1,
    sync: bool = True,
    allow_undetectable_readers: bool = True,
    progress: Optional[Callable[[Dict[str, Any]], Any]] = None,
    limits: Optional[Mapping[str, Any]] = None,
    stop: Optional[Any] = None,
    strict: bool = False,
) -> ImportResult:
    """Import a variable of local Zarr v2 stores into a DSS file.

    ``times`` ``(first, last)``, UTC and inclusive as ``xarray``'s ``sel(time=slice(first, last))``
    is, says which steps are read. Only the cells the ``envelope`` needs are read, with a margin
    for the warp; ``bbox`` ``(west, south, east, north)``, in the store's CRS, replaces that. With
    neither an envelope nor a box the whole grid is read. A run that would read over
    ``max_cells_per_import`` is refused while it is planned, with what to add. ``limits``, ``stop``
    and ``strict`` are :func:`read_zarr`'s. The remaining arguments are :func:`import_gridded`'s,
    which takes a store's path, ``bbox`` and ``times`` too: this is the same route.

    **Time.** In a store recognized as AORC v1.1, ``APCP_surface`` is the depth over the hour
    *ending* at each time, so a step at T is written PER-CUM from T - 1 h to T, with midnight as
    ``2400`` of the day it ends, whatever the spacing of the steps kept. Recognized means its
    ``aorc_version`` is ``"v1.1"``, its times are at the top of the hour, and nothing it states
    says otherwise. Any other precipitation has no period of its own and is refused unless
    ``window_rule="accumulation:<duration>"`` states one; other variables keep their instant.
    """
    if isinstance(inputs, (str, os.PathLike)):
        inputs = [inputs]
    paths = [str(Path(os.fspath(p))) for p in inputs]
    if not paths:
        raise ValueError("no inputs")
    if isinstance(version, bool) or version not in (None, 6, 7):
        raise ValueError(f"version must be 6 or 7, not {version!r}")
    if isinstance(threads, bool) or int(threads) < 1:
        raise ValueError(f"threads must be at least 1, not {threads!r}")
    span = _span(times)
    box = _bounds(envelope)
    out = str(Path(os.fspath(output)))
    records = _native.import_zarr(
        paths,
        out,
        variable,
        bbox=_box(bbox),
        times=span,
        envelope=box,
        envelope_crs=envelope_crs if envelope_crs is not None else (target_crs if box else None),
        shapefile=None if shapefile is None else str(Path(os.fspath(shapefile))),
        target_crs=target_crs,
        cell_size=None if cell_size is None else float(cell_size),
        method=method,
        units=units,
        data_type=data_type,
        version=version,
        align=align,
        window_rule=window_rule,
        duplicates=duplicates,
        threads=int(threads),
        sync=bool(sync),
        allow_undetectable_readers=bool(allow_undetectable_readers),
        progress=progress,
        limits=_limits(limits, strict),
        stop=stop,
        **_parts(parts),
    )
    records = [dict(r) for r in records]
    _warn_readers(out, records)
    _warn_accumulation(out, records)
    return ImportResult(output=out, records=records)
