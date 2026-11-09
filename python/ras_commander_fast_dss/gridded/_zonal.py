"""Grid to point over ``_native``. Argument checks live here; rules live in the engine."""

from __future__ import annotations

import datetime as _dt
import os
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Dict, List, Mapping, Optional, Sequence, Union

import numpy as np
import pandas as pd

from .. import _native

PathLike = Union[str, os.PathLike]

#: What :func:`grid_to_point` and :func:`zonal_statistics` can compute, as Vortex's option names map:
#: ``Average``, ``Min``, ``Max``, ``Median``, ``1Q``, ``3Q``, ``Pct>0``, ``Pct>1Q``.
STATISTICS = ("average", "min", "max", "median", "q1", "q3", "pct_gt_0", "pct_gt_q1")

_EPOCH = _dt.datetime(1899, 12, 31)


def _path(p: PathLike) -> str:
    return str(Path(os.fspath(p)))


def _time(seconds: Optional[int]) -> Optional[_dt.datetime]:
    return None if seconds is None else _EPOCH + _dt.timedelta(seconds=seconds)


@dataclass
class GridToPointResult:
    """What a run wrote.

    ``records`` holds one dict per record: ``zone``, ``variable``, ``statistic``, ``pathname``,
    ``regular``, ``values``, ``units``, ``data_type`` and ``outcome``. ``zones`` holds one dict per zone:
    ``zone``, ``cells`` (flat indices into the first row order the source states) and
    ``centroid_fallback`` (no cell centre was inside, so the cell at the middle of the extent was used).

    ``outcome`` says what this run did to the blocks it wrote, not what the series now holds. ``added``:
    every block was new. ``unchanged``: every block already held these values. ``window_updated``: at
    least one block held something else, and now holds this run's values at this run's stamps. In each
    case the pathname keeps any value outside this run's window, so no outcome means the series holds
    only this run's values.

    ``reserved_values`` counts values stored although a reader takes them as missing, which only
    ``compatibility="vortex"`` allows.
    """

    output: str
    records: List[Dict[str, Any]] = field(default_factory=list)
    zones: List[Dict[str, Any]] = field(default_factory=list)
    frames: int = 0
    reserved_values: int = 0

    @property
    def pathnames(self) -> List[str]:
        return [r["pathname"] for r in self.records]


def grid_to_point(
    grids: PathLike,
    zones: PathLike,
    zone_field: str,
    output: PathLike,
    *,
    statistics: Sequence[str] = ("average",),
    accumulate: bool = False,
    parts: Optional[Mapping[str, str]] = None,
    variables: Optional[Sequence[str]] = None,
    zones_crs: Optional[str] = None,
    area_weighted: bool = False,
    compatibility: str = "engine",
    version: Optional[int] = None,
    assume_no_datum_shift: bool = False,
    allow_undetectable_readers: bool = True,
    allow_in_place: bool = False,
) -> GridToPointResult:
    """Zonal statistics of each grid, written as one time series per zone and statistic.

    Reproduces HEC-Vortex 1.1.0's grid-to-point records on the committed comparison cases
    (``fixtures/public/vortex_p51``),
    apart from the observed Vortex behaviours listed in ``dss_vortex::zonal``, which this library
    refuses or does differently by default.

    Parameters
    ----------
    grids:
        A DSS file, GRIB2 file, GeoTIFF or ASCII grid.
    zones:
        A shapefile or GeoJSON file. Zones are reprojected to the grid's CRS, and features sharing a
        name are combined. A cell belongs to a zone if its centre does, by GDAL's rule, and a zone
        holding no cell centre takes the cell at the middle of its extent.
    zone_field:
        The field that names each zone, compared with no case as OGR does. It becomes the B part.
    statistics:
        Any of :data:`STATISTICS`. A zone with no valid cell stores HEC's missing marker, never 0.
    accumulate:
        A ``PRECIP-CUM`` record beside each precipitation average, as Vortex's ``isAccumulate``.
    parts:
        ``A`` and ``F``. B names the zone, C the variable and statistic, and D and E follow the frames.
    variables:
        Exact DSS pathnames or source variables. By default every grid in the file.
    zones_crs:
        A CRS for a zone file that states none: ``"SHG"``, ``"HRAP"`` or WKT. A CRS is never guessed.
    area_weighted:
        Adds ``area_weighted_mean`` with the C part suffix ``-AREAWT``, weighing cells by the share of
        their area inside the zone. It answers a different question from the cell-centre statistics,
        is not Vortex's, and is never on by default.
    compatibility:
        ``"vortex"`` restores the observed Vortex behaviours marked in ``dss_vortex::zonal``.
    version:
        DSS version of a new output. An existing file keeps its own.
    allow_in_place:
        Permit ``output`` to be the file holding ``grids``, adding series beside its grids. Otherwise
        an output that is an input is refused, whatever its name: a hard link counts.

    Nothing touches ``output`` until every statistic and record is settled, so a refusal leaves it as
    it was. That includes a valid statistic or accumulated total whose final float32 is -901, -902
    or ``-FLT_MAX``, which readers would take as missing: the run is refused, naming the zone, statistic
    and time. ``compatibility="vortex"`` stores it as Vortex does, and readers then lose it.

    **A rerun updates a window.** A record is merged with what its pathname already holds: values at
    stamps this run does not reach stay. There is no whole-series replacement.

    **A failure while writing proves nothing about what the file holds,** so it does not mean an
    existing file or series is unchanged. The exception's ``phase`` is ``"planning"`` if nothing was
    written, else ``"writing"``. ``completed`` lists the records the writer acknowledged. ``in_doubt``
    names the series being written when it failed, which may hold none, some or all of its new
    values, and may already be readable: a replacement can leave a changed series readable with
    ``completed`` empty. Read the file, or run again.
    """
    names = list(statistics)
    if area_weighted and "area_weighted_mean" not in names:
        names.append("area_weighted_mean")
    if "area_weighted_mean" in names and not area_weighted:
        raise ValueError('"area_weighted_mean" needs area_weighted=True, because it is a different question')
    parts = dict(parts or {})
    unknown = set(parts) - {"A", "F"}
    if unknown:
        raise ValueError(f"parts may hold A and F, not {sorted(unknown)}")
    result = _native.gridded_grid_to_point(
        _path(grids),
        _path(zones),
        zone_field,
        _path(output),
        names,
        accumulate=accumulate,
        part_a=parts.get("A"),
        part_f=parts.get("F"),
        variables=None if variables is None else list(variables),
        zones_crs=zones_crs,
        compatibility=compatibility,
        version=version,
        assume_no_datum_shift=assume_no_datum_shift,
        allow_undetectable_readers=allow_undetectable_readers,
        allow_in_place=allow_in_place,
    )
    return GridToPointResult(
        output=_path(output),
        records=list(result["records"]),
        zones=list(result["zones"]),
        frames=result["frames"],
        reserved_values=result["reserved_values"],
    )


def zonal_statistics(
    grids: PathLike,
    zones: PathLike,
    zone_field: str,
    *,
    variables: Optional[Sequence[str]] = None,
    zones_crs: Optional[str] = None,
    area_weighted: bool = False,
    compatibility: str = "engine",
    assume_no_datum_shift: bool = False,
    raw: bool = False,
) -> pd.DataFrame:
    """The same statistics as a table, one row per frame and zone, writing nothing.

    Missing statistics are NaN unless ``raw=True``, which keeps the stored marker.
    """
    table = _native.gridded_zonal_table(
        _path(grids),
        _path(zones),
        zone_field,
        variables=None if variables is None else list(variables),
        zones_crs=zones_crs,
        compatibility=compatibility,
        area_weighted=area_weighted,
        assume_no_datum_shift=assume_no_datum_shift,
    )
    stats = np.asarray(table["stats"])
    n_frames, n_zones = stats.shape[0], stats.shape[1]
    ids = [z["zone"] for z in table["zones"]]
    frames = table["frames"]
    out: Dict[str, Any] = {
        "zone": ids * n_frames,
        "variable": [f["variable"] for f in frames for _ in ids],
        "units": [f["units"] for f in frames for _ in ids],
        "start": [_time(f["start"]) for f in frames for _ in ids],
        "end": [_time(f["end"]) for f in frames for _ in ids],
    }
    flat = stats.reshape(n_frames * n_zones, 8)
    columns = {name: flat[:, i].copy() for i, name in enumerate(STATISTICS)}
    if table["area_weighted"] is not None:
        columns["area_weighted_mean"] = np.asarray(table["area_weighted"]).reshape(-1).copy()
    if not raw:
        for values in columns.values():
            values[values == table["missing"]] = np.nan
    out.update(columns)
    return pd.DataFrame(out)


def zone_masks(
    grids: PathLike,
    zones: PathLike,
    zone_field: str,
    *,
    variables: Optional[Sequence[str]] = None,
    zones_crs: Optional[str] = None,
    assume_no_datum_shift: bool = False,
) -> Dict[str, np.ndarray]:
    """Each zone's flat cell indices on the source's last grid, in the row order that grid states."""
    table = _native.gridded_zonal_table(
        _path(grids),
        _path(zones),
        zone_field,
        variables=None if variables is None else list(variables),
        zones_crs=zones_crs,
        assume_no_datum_shift=assume_no_datum_shift,
    )
    return {z["zone"]: np.asarray(z["cells"]) for z in table["zones"]}
