"""Grid tools over ``_native``: calculator, sanitizer, normalizer and clipper.

Argument checks live here; rules live in the engine (``dss_vortex::grid_tools``). Each tool reads a
DSS, GeoTIFF, ASCII grid or GRIB2 file and writes DSS grids, with no Java. A run plans every record
and computes every frame before it writes, so a refusal leaves the output as it was. A failure while
writing keeps the finished records, and sets ``phase``, ``input``, ``variable``, ``frame``,
``completed`` and ``in_doubt`` on the exception.

``completed`` lists the writes the writer acknowledged, which is not every record the file may hold.
``in_doubt`` names the pathname being written when it failed, if one was: the writer leaves no torn
record, but that record may have been published whole before the failure, so an exception does not
prove it absent. Read the file, or run again, which replaces it.

Results reproduce HEC-Vortex 1.1.0 and 0.13.3 on the committed comparison cases
(``fixtures/public/vortex_p53``, checked by ``tests/python/test_grid_tools.py``), except for
observed Vortex behaviours that this library does not implement.
"""

from __future__ import annotations

import math
import os
import warnings
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Dict, List, Mapping, Optional, Sequence, Union

from .. import _native
from .._time import TimeLike
from ._import import ImportResult, PathLike, _bounds, _seconds

_PART_KEYS = {"A", "B", "C", "F"}


def _path(p: PathLike) -> str:
    return str(Path(os.fspath(p)))


def _common(
    output: PathLike,
    parts: Optional[Mapping[str, str]],
    units: Optional[str],
    data_type: Optional[str],
    version: Optional[int],
    align: str,
    sync: bool,
    allow_undetectable_readers: bool,
) -> Dict[str, Any]:
    parts = dict(parts or {})
    unknown = set(parts) - _PART_KEYS
    if unknown:
        raise ValueError(
            f"parts may hold A, B, C and F, not {sorted(unknown)}: D and E always come from each "
            "frame's time window"
        )
    for key, value in parts.items():
        if not isinstance(value, str):
            raise ValueError(f"part {key} must be str, not {type(value).__name__}")
    if parts.get("C") == "*":
        del parts["C"]
    return dict(
        part_a=parts.get("A", ""),
        part_b=parts.get("B", ""),
        part_c=parts.get("C"),
        part_f=parts.get("F", ""),
        units=units,
        data_type=data_type,
        version=version,
        align=align,
        sync=sync,
        allow_undetectable_readers=allow_undetectable_readers,
    )


def _variables(variables: Optional[Sequence[str]]) -> Optional[List[str]]:
    if variables is None:
        return None
    if isinstance(variables, str):
        return [variables]
    return list(variables)


def _say(output: str, records: Sequence[Mapping[str, Any]]) -> None:
    """A conversion or an old lock scheme is stated, and not only recorded."""
    converted = sorted({(r["source_units"], r["units"]) for r in records if r.get("converted")})
    if converted:
        pairs = ", ".join(f"{a!r} to {b!r}" for a, b in converted)
        warnings.warn(
            f"{output}: cells were converted, as HEC-Vortex converts them on writing: {pairs}. "
            "Each record's source_units, units and converted say which",
            RuntimeWarning,
            stacklevel=3,
        )
    if any(r.get("readers_undetected") for r in records):
        warnings.warn(
            f"{output}: this DSS file uses HEC's old lock scheme, which cannot show whether HEC "
            "software has it open; the run held the HEC write lock but was not checked against "
            "HEC readers (pass allow_undetectable_readers=False to refuse such files)",
            RuntimeWarning,
            stacklevel=3,
        )


def _finish(output: PathLike, records: Sequence[Dict[str, Any]]) -> ImportResult:
    text = _path(output)
    _say(text, records)
    return ImportResult(output=text, records=list(records))


def calculate(
    input: PathLike,
    output: PathLike,
    *,
    multiply: Optional[float] = None,
    divide: Optional[float] = None,
    add: Optional[float] = None,
    subtract: Optional[float] = None,
    variables: Optional[Sequence[str]] = None,
    parts: Optional[Mapping[str, str]] = None,
    units: Optional[str] = None,
    data_type: Optional[str] = None,
    version: Optional[int] = None,
    align: str = "refuse",
    sync: bool = True,
    allow_undetectable_readers: bool = True,
) -> ImportResult:
    """Apply one operation by one constant to every valid cell (Vortex's ``BatchCalculator``).

    Give exactly one of ``multiply``, ``divide``, ``add`` and ``subtract``. The constant is used in
    single precision. A missing cell stays missing. A valid cell whose result a DSS grid cannot hold
    (a division by zero, an overflow, HEC's marker, or -901 or -902, which readers take as missing)
    refuses the run before anything is written.

    ``variables`` selects grids by their names in :func:`list_variables`, all by default; a name the
    file does not hold is refused. ``parts`` states the pathname: A, B and F are empty unless given,
    as in Vortex, and ``{"F": "*"}`` keeps a DSS input's F part. A record keeps its data type unless
    ``data_type`` states another. Each record says what it was: ``source_units``,
    ``source_data_type`` and ``converted``.
    """
    given = {k: v for k, v in dict(multiply=multiply, divide=divide, add=add, subtract=subtract).items() if v is not None}
    if len(given) != 1:
        raise ValueError(
            f"give exactly one of multiply, divide, add and subtract, not {sorted(given) or 'none'}"
        )
    ((operation, value),) = given.items()
    return _finish(
        output,
        _native.grid_calculate(
            _path(input),
            _path(output),
            operation,
            float(value),
            variables=_variables(variables),
            **_common(output, parts, units, data_type, version, align, sync, allow_undetectable_readers),
        ),
    )


def calculate_raster(
    input: PathLike,
    raster: PathLike,
    output: PathLike,
    *,
    operation: str,
    method: str = "bilinear",
    raster_variable: Optional[str] = None,
    variables: Optional[Sequence[str]] = None,
    parts: Optional[Mapping[str, str]] = None,
    units: Optional[str] = None,
    data_type: Optional[str] = None,
    version: Optional[int] = None,
    align: str = "refuse",
    sync: bool = True,
    allow_undetectable_readers: bool = True,
    static_raster: bool = False,
) -> ImportResult:
    """Combine each grid, on the left, with a raster resampled to it (``BatchGridCalculator``).

    ``operation`` is ``multiply``, ``divide``, ``add`` or ``subtract``; ``method`` is ``near``,
    ``bilinear`` or ``average``. A cell missing on either side is missing in the result. **Observed
    behaviour we did not implement:** Vortex 1.1.0 and 0.13.3 apply the operation to both sides'
    markers as numbers. The selected grids must share one grid, and the raster must resample to it
    exactly.

    A DSS raster and an input grid that state different times are refused. Pass
    ``static_raster=True`` to reuse one timed grid as a factor for all of them. A DSS raster is this
    library's own extension: HEC-Vortex 1.1.0 and 0.13.3 fail on one. A raster with no time, such as
    a GeoTIFF, needs no option.
    :func:`calculate` describes the other arguments.
    """
    return _finish(
        output,
        _native.grid_calculate_raster(
            _path(input),
            _path(raster),
            _path(output),
            operation,
            method=method,
            raster_variable=raster_variable,
            variables=_variables(variables),
            static_raster=static_raster,
            **_common(output, parts, units, data_type, version, align, sync, allow_undetectable_readers),
        ),
    )


def sanitize(
    input: PathLike,
    output: PathLike,
    *,
    minimum: Optional[float] = None,
    maximum: Optional[float] = None,
    minimum_replacement: float = math.nan,
    maximum_replacement: float = math.nan,
    variables: Optional[Sequence[str]] = None,
    parts: Optional[Mapping[str, str]] = None,
    units: Optional[str] = None,
    data_type: Optional[str] = None,
    version: Optional[int] = None,
    align: str = "refuse",
    sync: bool = True,
    allow_undetectable_readers: bool = True,
) -> ImportResult:
    """Replace valid cells at or beyond thresholds (Vortex's ``BatchSanitizer``).

    Both thresholds are inclusive, as in Vortex, compared in double precision; the minimum is tested
    first. A replacement of NaN, the default, makes the cell missing. A missing cell stays missing.
    **Observed behaviour we did not implement:** Vortex compares HEC's marker with the thresholds,
    so a stated replacement turns a missing cell into a value. :func:`calculate` describes the other
    arguments.
    """
    return _finish(
        output,
        _native.grid_sanitize(
            _path(input),
            _path(output),
            minimum=minimum,
            maximum=maximum,
            minimum_replacement=float(minimum_replacement),
            maximum_replacement=float(maximum_replacement),
            variables=_variables(variables),
            **_common(output, parts, units, data_type, version, align, sync, allow_undetectable_readers),
        ),
    )


def clip(
    input: PathLike,
    output: PathLike,
    *,
    envelope: Optional[Sequence[float]] = None,
    envelope_crs: Optional[str] = None,
    shapefile: Optional[PathLike] = None,
    variables: Optional[Sequence[str]] = None,
    parts: Optional[Mapping[str, str]] = None,
    units: Optional[str] = None,
    data_type: Optional[str] = None,
    version: Optional[int] = None,
    align: str = "refuse",
    sync: bool = True,
    allow_undetectable_readers: bool = True,
) -> ImportResult:
    """Clip grids to an envelope, by nearest neighbour, in their own CRS (``BatchSubsetter``).

    Give ``envelope=(min_x, min_y, max_x, max_y)`` with ``envelope_crs``, or a ``shapefile`` with a
    ``.prj``, whose features' combined extent is used. A CRS is never assumed, and a grid in a
    geographic CRS is refused, which Vortex also does not clip.

    An extent not on whole cells is refused. **Observed behaviour we did not implement:** Vortex
    uses it as given and rounds the record's corner to whole cells, moving the grid. Snap the
    envelope to whole cells, or pass ``align="vortex"`` for Vortex's record. :func:`calculate`
    describes the other arguments.
    """
    return _finish(
        output,
        _native.grid_clip(
            _path(input),
            _path(output),
            envelope=_bounds(envelope),
            envelope_crs=envelope_crs,
            shapefile=None if shapefile is None else _path(shapefile),
            variables=_variables(variables),
            **_common(output, parts, units, data_type, version, align, sync, allow_undetectable_readers),
        ),
    )


@dataclass
class NormalizeResult(ImportResult):
    """What a normalization wrote, and what its windows left out.

    ``intervals`` holds one dict per interval: ``start`` and ``end`` in HEC seconds, and the number
    of ``sources`` and ``normals`` wholly inside it. ``sources_left_out`` and ``normals_left_out``
    name grids not wholly inside any interval. ``long_hash_bin`` says the last bits may differ from
    Vortex's: see ``java_order.rs``. ``cells_made_missing_by_normal`` counts the valid source cells
    written as missing because their normal was missing, over all records; a warning says so when it
    is not 0. A cell the source already held as missing is not counted.
    """

    intervals: List[Dict[str, Any]] = field(default_factory=list)
    sources_left_out: List[str] = field(default_factory=list)
    normals_left_out: List[str] = field(default_factory=list)
    long_hash_bin: bool = False
    cells_made_missing_by_normal: int = 0


def normalize(
    input: PathLike,
    normals: PathLike,
    output: PathLike,
    *,
    start: TimeLike,
    end: TimeLike,
    interval: Any,
    source_variables: Optional[Sequence[str]] = None,
    normals_variables: Optional[Sequence[str]] = None,
    parts: Optional[Mapping[str, str]] = None,
    units: Optional[str] = None,
    data_type: Optional[str] = None,
    version: Optional[int] = None,
    align: str = "refuse",
    sync: bool = True,
    allow_undetectable_readers: bool = True,
) -> NormalizeResult:
    """Scale each interval's source grids so their total matches its normals (``Normalizer``).

    ``interval`` is a ``datetime.timedelta`` or a number of seconds. Each interval from ``start``
    uses the grids wholly inside it. A cell becomes ``normalTotal / sourceTotal * value`` in single
    precision, and 0 if its source total is 0. Totals sum in Vortex's order, which changes last bits.

    A missing source cell is left out of its total and stays missing. **A cell whose normal is missing
    is written as missing**, so a result never mixes scaled and unscaled cells. That holds where the
    source's total is 0, and for a normal missing in one grid of an interval's several. Statistics
    leave those cells out. A normal missing everywhere writes records with no valid cell, and does not
    refuse; such a record stores the same statistics as any other grid with no valid cell, a minimum
    above its maximum. ``cells_made_missing_by_normal`` says how many valid source cells this took, and a
    ``RuntimeWarning`` reports it. **Observed behaviour we did not implement:** Vortex tests only NaN,
    so it sums and scales HEC's marker as a number, in the source and normal totals alike. Given a
    NaN normal it passes the source through.

    Sources and normals in an interval must agree in size, spacing, origin, CRS and units. An
    interval with sources and no normals is refused. :func:`calculate` describes the other arguments.
    """
    seconds = interval.total_seconds() if hasattr(interval, "total_seconds") else interval
    if float(seconds) != int(seconds):
        raise ValueError(f"interval must be whole seconds, not {seconds}")
    outcome = _native.grid_normalize(
        _path(input),
        _path(normals),
        _path(output),
        _seconds(start),
        _seconds(end),
        int(seconds),
        source_variables=_variables(source_variables),
        normals_variables=_variables(normals_variables),
        **_common(output, parts, units, data_type, version, align, sync, allow_undetectable_readers),
    )
    text = _path(output)
    _say(text, outcome["records"])
    made_missing = int(outcome["cells_made_missing_by_normal"])
    if made_missing:
        warnings.warn(
            f"{text}: {made_missing} valid cell(s) had a missing normal and were written as missing. "
            "See NormalizeResult.cells_made_missing_by_normal",
            RuntimeWarning,
            stacklevel=2,
        )
    return NormalizeResult(
        output=text,
        records=list(outcome["records"]),
        intervals=list(outcome["intervals"]),
        sources_left_out=list(outcome["sources_left_out"]),
        normals_left_out=list(outcome["normals_left_out"]),
        long_hash_bin=bool(outcome["long_hash_bin"]),
        cells_made_missing_by_normal=made_missing,
    )


__all__ = ["NormalizeResult", "calculate", "calculate_raster", "clip", "normalize", "sanitize"]
