"""Shared HEC-DSS API for ras-commander and hms-commander, backed by a pure-Rust engine.

Read-only access to DSS6 and DSS7 files without Java::

    import ras_commander_fast_dss as dss

    with dss.open("model.dss") as f:
        print(f.version, f.version_string)
        cat = f.catalog()                       # DataFrame, one row per record
        df = f.read_timeseries("/BASIN/LOC/FLOW//1HOUR/OBS/")

    df = dss.read_timeseries("model.dss", "/BASIN/LOC/FLOW//1HOUR/OBS/")
    cat = dss.get_catalog("model.dss")
    rating = dss.read_paired_data("model.dss", "/BASIN/LOC/STAGE-FLOW///RATING/")
    grid = dss.read_grid("model.dss", "/SHG/BASIN/PRECIP/01JAN2020:0000/01JAN2020:0100/RADAR/")

The DataFrames match what ``ras_commander.RasDss`` returns today (index, column, dtypes,
``attrs``), with HEC missing markers mapped to NaN. Paired data comes back as
:class:`PairedData` and grids as :class:`Grid` (NumPy arrays plus metadata);
:func:`read_paired_data_compat` returns hms-commander's ``DssCore.read_paired_data``
dict. Exceptions are in :mod:`ras_commander_fast_dss.exceptions`.
"""

from __future__ import annotations

import logging
from pathlib import Path
from typing import Any, Dict, Optional, Union

import numpy as np
import pandas as pd

from . import _native
from ._file import MISSING_MARKERS, DssFile, missing_mask, open, record_type_name
from ._records import GRID_UNDEFINED, Grid, PairedData
from ._records import Profile  # P14
from ._time import HEC_EPOCH, to_hec_seconds
from .exceptions import (
    DssError,
    DssFormatError,
    DssLockedError,
    DssPathnameNotFoundError,
    DssUnsupportedRecordError,
)

__all__ = [
    "DssFile",
    "open",
    "get_catalog",
    "read_timeseries",
    "read_paired_data",
    "read_paired_data_compat",
    "read_grid",
    "PairedData",
    "Grid",
    "GRID_UNDEFINED",
    "file_signature",
    "missing_mask",
    "record_type_name",
    "to_hec_seconds",
    "MISSING_MARKERS",
    "HEC_EPOCH",
    "DssError",
    "DssFormatError",
    "DssUnsupportedRecordError",
    "DssPathnameNotFoundError",
    "DssLockedError",
    "FEATURES",
]

#: Qualified capabilities that consumers (ras-commander ``RasDss``, hms-commander)
#: may gate on. A frozenset of str; names are only added, never repurposed.
#:
#: ``grid_srs_monolith_parity``: grid ``srs_definition`` follows HEC Monolith's
#: ``AlbersInfo.getSpatialReferenceSystem`` rule for Albers grids, so
#: native grid metadata can replace the Java path.
#:
#: ``write_ts_quality_words``, ``write_ts_notes``, ``write_ts_patterns``,
#: ``write_ts_pseudo_regular``: ``write_timeseries`` stores multi-word quality, integer and
#: character notes (DSS7), pattern records and pseudo-regular ``~`` E parts (both versions),
#: and DSS7 reads return ``quality_words``, ``inotes`` and ``cnotes``.
#:
#: ``write_profile``: :func:`write_profile` stores DSS7 regular profiles.
#: ``write_location``: :func:`write_location` stores DSS7 location records and DSS6
#: time-series location data.
#: ``grid_to_point``: :func:`ras_commander_fast_dss.gridded.grid_to_point` writes zonal statistics of
#: grids as time series, in the engine's process, with no Java or GDAL.
#:
#: ``gridded_import``: :mod:`ras_commander_fast_dss.gridded` imports GRIB2 files and arrays to DSS
#: grids without Java. ``gridded_time_tools``: its ``shift_times`` and ``resample_times``
#: replace Vortex's time shifter and time-step resampler.
#: ``grid_tools``: :mod:`ras_commander_fast_dss.gridded` calculator, sanitizer, normalizer and clipper
#: without Java.
#: ``gridded_accumulation_window``: ``import_gridded(window_rule="accumulation:<duration>")`` states a
#: GRIB2 record's accumulation, for MRMS 15M to 72H QPE.
#: ``gridded_netcdf``: ``import_gridded`` and ``list_variables`` read NetCDF classic, NetCDF-4 and HDF5
#: files, with the reader's limits and ``gridded.StopHandle``.
#: ``gridded_grib1_stage4``: ``import_gridded`` and ``list_variables`` read NCEP Stage IV GRIB1 (2002
#: to July 2020): total precipitation accumulated over 1, 6 or 24 hours on the polar stereographic
#: HRAP grid, simple packing. Other GRIB1 is a typed refusal.
#: ``write_ts_dss6_irregular_pattern_opt_in``: a DSS6 irregular pattern (type 111) is written
#: only with ``allow_unconfirmed_dss6_irregular_pattern=True`` and refused otherwise with
#: ``DssUnsupportedRecordError``: no HEC reader has confirmed such records. DSS7 patterns
#: and DSS6 regular patterns are unaffected.
#: ``write_sync_policy``: the TS, paired-data, grid, profile and location writers take
#: ``sync=``.
#: ``write_sync_modes``: ``sync=`` names three flush policies, :data:`SYNC_MODES`.
#: ``"end"`` is the default since 0.1.2: one flush at the end of each write
#: call. ``"step"`` also flushes at every flush point of the transaction (the default up to
#: 0.1.1). ``"none"`` does not flush. ``True`` means ``"end"`` and ``False`` means ``"none"``.
#: ``create``, ``repair``, ``gridded.grid_to_point`` and every gridded writer take it too. All
#: three keep the ordering of completed writes and of the recovery marker under process death.
#: No power-loss claim for any mode; ``repair`` is the path for a damaged DSS7 file.
#: ``gridded_threads_all_cores``: ``gridded.import_gridded``, ``import_arrays`` and
#: ``import_zarr`` default to ``threads=None``, every core available to the process
#: (``1`` up to 0.1.1). The written file is byte-identical for any count.
#: ``dssfile_held_handle``: a ``DssFile`` keeps one read+write handle from its first write
#: until ``close()``, ``__exit__`` or garbage collection, and its writes and reads use it.
#: The lock is still tested, taken and released by every write;
#: the handle's identity is checked against the path before each write and reopened if the
#: file was replaced.
FEATURES = frozenset(
    {
        "grid_srs_monolith_parity",
        "write_ts_quality_words",
        "write_ts_notes",
        "write_ts_patterns",
        "write_ts_pseudo_regular",
        "write_profile",
        "write_location",
        "gridded_import",
        "gridded_time_tools",
        "grid_to_point",
        "grid_tools",
        "gridded_accumulation_window",
        "gridded_netcdf",
        # P73: NCEP Stage IV GRIB1 through ``import_gridded`` and ``list_variables``.
        "gridded_grib1_stage4",
        # P49: ``gridded.import_zarr``, ``read_zarr`` and ``zarr_variables`` read a local Zarr v2
        # store, and ``import_gridded`` takes a store's folder.
        "gridded_zarr",
        # P65: ``repair(src, dest)`` copies every readable DSS7 record into a new file.
        "repair_copy_out",
        "write_sync_policy",
        "write_sync_modes",
        "gridded_threads_all_cores",
        "dssfile_held_handle",
        # P71: ``import_gridded`` and ``list_variables`` read ESRI BIL (PRISM, old and 2025 names),
        # and ZIP archives of BIL, ASCII grids and GeoTIFF, as PRISM distributes them.
        "gridded_prism_bil",
        # P71: SNODAS daily tars and ``.dat`` members, with ``snodas_times=`` and
        # ``snodas_georeference=`` on ``import_gridded``.
        "gridded_snodas",
        # P66: DSS6 irregular-pattern writes need an explicit opt-in.
        "write_ts_dss6_irregular_pattern_opt_in",
    }
)


def file_signature(dss_file: Union[str, Path]) -> dict:
    """Return header facts for a DSS6 or DSS7 file without reading its catalog.

    Keys: ``version`` (6 or 7), ``version_string``, ``byte_order``,
    ``number_records``, ``file_size_bytes``, ``dead_words``, ``number_deletes``
    (DSS7 only, otherwise ``None``).
    """
    return _native.file_signature(str(Path(dss_file)))


def get_catalog(dss_file: Union[str, Path], *, recover: bool = False) -> pd.DataFrame:
    """Catalog of a DSS file (see :meth:`DssFile.catalog`)."""
    with open(dss_file, recover=recover) as f:
        return f.catalog()


def read_timeseries(
    dss_file: Union[str, Path],
    pathname: str,
    start=None,
    end=None,
    *,
    trim_missing: bool = True,
    raw: bool = False,
    recover: bool = False,
    no_data: str = "return",
    **kwargs,
) -> pd.DataFrame:
    """Read one time series (see :meth:`DssFile.read_timeseries`, including ``no_data``)."""
    from ._file import _refuse_no_data

    _refuse_no_data(no_data)  # validate before opening the file
    with open(dss_file, recover=recover) as f:
        return f.read_timeseries(
            pathname, start, end, trim_missing=trim_missing, raw=raw, no_data=no_data, **kwargs
        )


def read_paired_data(
    dss_file: Union[str, Path],
    pathname: str,
    *,
    ignore_d_part: bool = True,
    dtype: str = "stored",
    recover: bool = False,
) -> PairedData:
    """Read one paired-data record (see :meth:`DssFile.read_paired_data`)."""
    with open(dss_file, recover=recover) as f:
        return f.read_paired_data(pathname, ignore_d_part=ignore_d_part, dtype=dtype)


def read_grid(
    dss_file: Union[str, Path],
    pathname: str,
    *,
    missing: str = "nan",
    order: str = "native",
    max_cells: Optional[int] = None,
    preserve_legacy_missing: bool = False,
    recover: bool = False,
) -> Grid:
    """Read one grid record (see :meth:`DssFile.read_grid`)."""
    with open(dss_file, recover=recover) as f:
        return f.read_grid(
            pathname,
            missing=missing,
            order=order,
            max_cells=max_cells,
            preserve_legacy_missing=preserve_legacy_missing,
        )


_log = logging.getLogger(__name__)


def read_paired_data_compat(dss_file: Union[str, Path], pathname: str) -> Optional[Dict[str, Any]]:
    """Drop-in for hms-commander's ``DssCore.read_paired_data`` (Java ``HecDss``).

    Returns exactly the dict that function returns, or ``None`` on any failure
    (missing file, no record, not paired data, damaged file), logging the reason:

    - ``x_values``: ordinates as float64 (Java ``xOrdinates``, a ``double[]``);
    - ``y_values``: **curve 0 only**, float64 (``yOrdinates[0]``);
    - ``x_units``, ``y_units``: independent and dependent units;
    - ``x_label``, ``y_label``: the independent and dependent **type** strings
      (``xType``/``yType``, for example ``LINEAR`` or ``PROB``), not the curve labels.
      This quirk of the original is kept on purpose; use :func:`read_paired_data`
      for curve labels and every curve;
    - ``pathname``: the pathname as passed.

    The D part is ignored when the exact pathname is not found, as
    ``HecDss.get(pathname, True)`` does.
    """
    path = Path(dss_file)
    if not path.exists():
        _log.warning("DSS file not found: %s", path)
        return None
    try:
        pdata = read_paired_data(path, pathname, dtype="float64")
    except DssPathnameNotFoundError:
        _log.warning("No data found for pathname: %s", pathname)
        return None
    except Exception as e:  # noqa: BLE001 - the original returns None on any failure
        _log.error("Error reading paired data from %s: %s", pathname, e)
        return None
    if pdata.n_curves == 0:
        _log.warning("No Y ordinates found for pathname: %s", pathname)
        return None
    return {
        "x_values": np.asarray(pdata.ordinates, dtype=np.float64),
        "y_values": np.asarray(pdata.curve(0), dtype=np.float64),
        "x_units": pdata.units_independent,
        "y_units": pdata.units_dependent,
        "x_label": pdata.type_independent,
        "y_label": pdata.type_dependent,
        "pathname": pathname,
    }


# -- locations ------------------------------------------------------

from ._records import Location, find_supplemental_keyword, parse_supplemental  # noqa: E402
from .exceptions import DssLocationConflictError  # noqa: E402


def read_location(dss_file: Union[str, Path], pathname: str, *, recover: bool = False) -> Location:
    """Read one DSS7 location record (see :meth:`DssFile.read_location`)."""
    with open(dss_file, recover=recover) as f:
        return f.read_location(pathname)


def read_associated_location(
    dss_file: Union[str, Path], pathname: str, *, recover: bool = False
) -> Optional[Location]:
    """Location of a data record, or ``None`` (see :meth:`DssFile.read_associated_location`)."""
    with open(dss_file, recover=recover) as f:
        return f.read_associated_location(pathname)


__all__ += [
    "Location",
    "parse_supplemental",
    "find_supplemental_keyword",
    "read_location",
    "read_associated_location",
    "DssLocationConflictError",
]


# -- profiles ---------------------------------------------------------------------


def read_profile(
    dss_file: Union[str, Path],
    pathname: str,
    start=None,
    end=None,
    *,
    raw: bool = False,
    trim_missing: bool = True,
    recover: bool = False,
) -> Profile:
    """Read one regular profile dataset, DSS7 types 102/107 (see :meth:`DssFile.read_profile`)."""
    with open(dss_file, recover=recover) as f:
        return f.read_profile(pathname, start, end, raw=raw, trim_missing=trim_missing)


__all__ += ["read_profile", "Profile"]


# P16: typed outcome of the catalog-index recovery route.
from .exceptions import DssRecordUnavailableError  # noqa: E402

__all__.append("DssRecordUnavailableError")


# ---------------------------------------------------------------------------------
# P19a: read-only HEC lock detection.
# ---------------------------------------------------------------------------------


def lock_status(dss_file: Union[str, Path]) -> dict:
    """Report whether HEC software holds lock words on a DSS6/DSS7 file.

    A diagnostic for future writers. It opens its own read-only handle,
    takes no locks and writes nothing; reads never call it and never raise
    :class:`DssLockedError`.

    Returns a dict:

    - ``version``: 6 or 7 (0 when Windows refused the open: ``sharing_violation``);
    - ``version_string``;
    - ``locked``: True when any reason was found;
    - ``reasons``: distinct reasons, first-seen order: ``primary_write``,
      ``write_array``, ``exclusive``, ``request``, ``read_array`` (an HEC process has
      the file open), ``stale_write_marker`` (DSS6 ``KFSIZE = -1357``),
      ``sharing_violation`` (Windows exclusive open);
    - ``held``: list of ``{"reason", "byte", "length"}`` for every held word;
    - ``readers_detectable``: False for DSS7 old-scheme files (``7-10``) and DSS6
      files before ``6-K``, where an open-only HEC process cannot be seen;
    - ``holder_pids``: holder PIDs where the OS reports them (Linux/macOS), else empty;
    - ``method`` (``read_probe`` on Windows, ``ofd_getlk`` on Linux, ``getlk`` on
      macOS) and ``ranges_tested``.

    On POSIX, closing any descriptor drops the calling process's own ``lockf`` locks on
    the file: do not call this on a file that heclib (pydsstools, hecdss) has open in
    the same process.
    """
    return _native.lock_status(str(Path(dss_file)))


__all__.append("lock_status")


# P26: record user-header text.
from ._records import parse_user_header_items  # noqa: E402

__all__ += ["parse_user_header_items"]

# P29: opt-in HEC DSS6 "no data" refusal (``no_data="refuse"``).
from .exceptions import NO_DATA_STATUS, DssNoDataError  # noqa: E402

__all__ += ["DssNoDataError", "NO_DATA_STATUS"]

# P20: DSS7 regular time-series writer.
from ._write import HEC_UNDEFINED_FLOAT, SYNC_MODES, create, repair, write_timeseries  # noqa: E402

__all__ += ["create", "repair", "write_timeseries", "HEC_UNDEFINED_FLOAT", "SYNC_MODES"]

# P22: DSS6 time-series writer (version=6 of create / write_timeseries).
from ._write import HEC_MISSING_DSS6  # noqa: E402

__all__ += ["HEC_MISSING_DSS6"]

# P23: paired-data writer (DSS7 and DSS6).
from ._write_pd import write_paired_data  # noqa: E402

__all__ += ["write_paired_data"]
# P24: grid writer (DSS7 and DSS6).
from ._write_grid import GRID_NODATA, SHG_WKT, write_grid  # noqa: E402

__all__ += ["write_grid", "GRID_NODATA", "SHG_WKT"]

# P39: profile and location writers.
from ._write_profile_location import write_location, write_profile  # noqa: E402

__all__ += ["write_profile", "write_location"]

# P46: gridded import.
from . import gridded  # noqa: E402

__all__ += ["gridded"]
