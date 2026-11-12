"""Importer API over ``_native``. Argument checks live here; rules live in the engine."""

from __future__ import annotations

import datetime as _dt
import os
import warnings
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Callable, Dict, List, Mapping, Optional, Sequence, Tuple, Union

import numpy as np

from .. import _native
from .._write import _sync_mode
from . import _select
from ..exceptions import DssUnsupportedRecordError
from .._time import TimeLike, to_hec_seconds

PathLike = Union[str, os.PathLike]

_PART_KEYS = {"A", "B", "C", "F"}


@dataclass
class ImportResult:
    """What an import wrote.

    ``records`` holds one dict per record: ``input``, ``variable``, ``frame``, ``pathname``,
    ``record_type``, ``outcome`` (``"added"``, ``"replaced"``, ``"unchanged"``), ``data_type``,
    ``units``, ``c_part_source`` (which of the source's names gave the C part: ``short_name``,
    ``description``, ``file_name`` or ``short_name_upper``), ``nx``, ``ny``,
    ``lower_left_cell_x``, ``lower_left_cell_y``, ``cell_size`` and ``readers_undetected``.

    With a cell size and a target CRS stated, a record also holds ``cell_size_requested_metres``,
    ``cell_size_stored_metres`` (the record's cell size in metres, by the CRS's own unit) and
    ``cell_size_difference_metres``, stored less requested. The difference is zero on a CRS in
    metres whose record holds the size exactly, and is not zero on a CRS in feet, where a size in
    metres is seldom a 32-bit float of feet: see ``cell_size`` under :func:`import_gridded`.

    Under ``window_rule="accumulation:<duration>"`` a record also states ``accumulation_seconds``;
    ``accumulation_stated_by``, which is ``"caller"`` when nothing in the file states a duration and
    otherwise ``"variable"``, ``"file_name"`` or ``"variable+file_name"``; ``overlaps_earlier``; and
    ``gap_before``. These four are ``None`` under other rules.
    """

    output: str
    records: List[Dict[str, Any]] = field(default_factory=list)

    @property
    def pathnames(self) -> List[str]:
        return [r["pathname"] for r in self.records]


def _bounds(envelope, what="envelope"):
    if envelope is None:
        return None
    try:
        min_x, min_y, max_x, max_y = (float(v) for v in envelope)
    except (TypeError, ValueError):
        raise ValueError(f"{what} must be (min_x, min_y, max_x, max_y)") from None
    if not all(np.isfinite([min_x, min_y, max_x, max_y])):
        raise ValueError(f"{what} must be finite")
    return (min_x, min_y, max_x, max_y)


def _parts(parts: Optional[Mapping[str, str]]) -> Dict[str, Any]:
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
        if value == "*":
            raise ValueError(
                f"part {key} cannot be '*': Vortex's wildcard takes the part from a DSS source, "
                "which this importer does not read yet"
            )
    return dict(
        part_a=parts.get("A", ""),
        part_b=parts.get("B", ""),
        part_c=parts.get("C"),
        part_f=parts.get("F", ""),
    )


def _warn_readers(output, records):
    if any(r.get("readers_undetected") for r in records):
        warnings.warn(
            f"{output}: this DSS file uses HEC's old lock scheme, which cannot show whether HEC "
            "software has it open; the import held the HEC write lock but was not checked against "
            "HEC readers (pass allow_undetectable_readers=False to refuse such files)",
            RuntimeWarning,
            stacklevel=3,
        )


def _warn_accumulation(output, records):
    overlaps = [r["pathname"] for r in records if r.get("overlaps_earlier")]
    gaps = [r["pathname"] for r in records if r.get("gap_before")]
    if overlaps:
        warnings.warn(
            f"{output}: {len(overlaps)} of {len(records)} records overlap an earlier period of this "
            f"run, first {overlaps[0]}. Each keeps its own D and E parts, so all are stored and none "
            "replaced, and a consumer summing them counts the shared rain more than once. Rolling "
            "MRMS accumulations overlap this way; use non-overlapping times, or a shorter duration, "
            "for a series meant to be summed",
            RuntimeWarning,
            stacklevel=3,
        )
    if gaps:
        warnings.warn(
            f"{output}: {len(gaps)} of {len(records)} records leave a gap after the previous period "
            f"of this run, first {gaps[0]}; a sum of them misses that time",
            RuntimeWarning,
            stacklevel=3,
        )


#: A stop for a run in progress. Create one, pass it as ``stop=`` to :func:`import_gridded`, and call
#: ``request()`` from another thread: the run ends before its next frame, and a NetCDF read in
#: progress ends before its next chunk or filter stage, once the stage in progress has finished.
#: ``clear()`` withdraws the request, and ``requested`` reads it.
StopHandle = _native.GriddedStop


def netcdf_limits() -> Dict[str, Any]:
    """The NetCDF reader's defaults for the limits :func:`import_gridded` takes: ``max_decoded_bytes_per_read``,
    ``chunk_cache_bytes`` and ``max_seconds_per_read`` (``None``: no limit)."""
    limits = _native.gridded_limits()
    return {
        "max_decoded_bytes_per_read": limits["netcdf_max_decoded_bytes_per_read"],
        "chunk_cache_bytes": limits["netcdf_chunk_cache_bytes"],
        "max_seconds_per_read": limits["netcdf_max_seconds_per_read"],
    }


def _limit(name: str, value: Any) -> Optional[int]:
    if value is None:
        return None
    if isinstance(value, bool) or not isinstance(value, (int, np.integer)) or int(value) < 1:
        raise ValueError(f"{name} must be a positive whole number or None, not {value!r}")
    return int(value)


def list_variables(path: PathLike) -> List[str]:
    """Variables a file offers, by the names ``variable=`` takes. Reads only metadata.

    For a NetCDF or HDF5 file these are the variables with a coordinate system, by their full names,
    group path included, as Vortex lists them. A listed variable can still be refused when it is
    imported, for example one on two-dimensional axes."""
    return list(_native.gridded_variables(str(Path(os.fspath(path)))))


def _threads(threads: Optional[int]) -> Optional[int]:
    """The ``threads=`` of the importers: ``None`` (all cores) or an int >= 1."""
    if threads is None:
        return None
    if isinstance(threads, bool) or int(threads) < 1:
        raise ValueError(f"threads must be at least 1, not {threads!r}")
    return int(threads)


def import_gridded(
    inputs: Union[PathLike, Sequence[PathLike]],
    output: PathLike,
    *,
    variable: Optional[str] = None,
    product: Optional[str] = None,
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
    threads: Optional[int] = None,
    sync: Union[bool, str] = "end",
    allow_undetectable_readers: bool = True,
    progress: Optional[Callable[[Dict[str, Any]], Any]] = None,
    bbox: Optional[Sequence[float]] = None,
    times: Optional[Tuple[Any, Any]] = None,
    max_seconds_per_read: Optional[int] = None,
    max_decoded_bytes_per_read: Optional[int] = None,
    chunk_cache_bytes: Optional[int] = None,
    stop: Optional["StopHandle"] = None,
    snodas_times: str = "header",
    snodas_georeference: str = "vortex",
) -> ImportResult:
    """Import grids from files into a DSS file, one frame at a time.

    A folder that is a Zarr v2 store is an input too (see :func:`import_zarr`). For a store,
    ``times=(first, last)``, UTC and inclusive, says which steps are read, and only the cells the
    ``envelope`` needs are read, or those ``bbox=(west, south, east, north)`` touches, in the
    store's CRS, when it is given. A run that would read too much of a store is refused while it
    is planned, with what to add. Other inputs refuse ``bbox`` and ``times``.

    The whole run is planned first: every pathname, window, unit and placement is decided before a
    cell is decoded. A refusal then leaves the output untouched, and does not create it. A failure
    while writing keeps the records already written, and never leaves a partial record. A new file
    is created just before its first record, so a failure of that first write can leave a DSS file
    with no records.

    Parameters
    ----------
    inputs:
        One path or a sequence, read in the order given. GRIB2, NetCDF (classic and NetCDF-4), HDF5,
        GeoTIFF and ASCII grids are read today, each known by its first bytes and not its name. GRIB1
        is read for NCEP Stage IV (2002 to July 2020): total precipitation accumulated over hours, on
        the polar stereographic HRAP grid, simple packing; other GRIB1 is refused.
        A NetCDF step's period is the file's own: its time bounds, or its time for an instant. See
        "NetCDF" below.
        A file whose columns run east to west must be warped; ``target_crs=None`` with ``cell_size=None``
        and no bounds refuses it. A raster stating no nodata marker is imported under -9999, as Vortex
        does, so a valid -9999 cell in such a file becomes missing and leaves the statistics.
    output:
        A DSS file. An existing file keeps its version; a new one is created at the first write.
    variable, product:
        Exactly one. ``variable`` is an exact name from :func:`list_variables`. ``product`` is
        ``"mrms_qpe"`` or ``"total_precipitation"``, and must match one variable in each input. An
        input without the selection is refused.
    envelope, envelope_crs, shapefile:
        At most one of ``envelope`` and ``shapefile``. ``envelope`` is
        ``(min_x, min_y, max_x, max_y)`` in ``envelope_crs``, which defaults to ``target_crs``, or
        with no target to each source's own CRS. A shapefile gives its bounding box and ``.prj``,
        which must exist; there is no polygon mask, as in Vortex.
    target_crs, cell_size, method:
        ``"SHG"``, ``"HRAP"`` or WKT; a cell size in the target's units; ``"near"``, ``"bilinear"``
        or ``"average"``. Defaults are those of ``ras_commander.precip.VortexCli``. Set all of
        ``target_crs``, ``cell_size``, ``envelope`` and ``shapefile`` to ``None`` to write the
        source grid unwarped. A WKT target may be Transverse Mercator, UTM included, in metres, US
        survey feet or international feet. An envelope is in the CRS's own units. **The cell size
        is always metres**, and on a target in feet it is converted:

        - by default with the CRS's own foot, at the spacing a DSS record stores. A grid record
          holds its cell size as a 32-bit float and its origin as whole cells, so the converted
          size is rounded to that float first and the grid is laid out, snapped and resampled on
          it: 2,000 m is 6,561.6665 US survey feet, 0.05 mm short of 2 km in all, and 10,000 m is
          32,808.332, 0.4 mm short of 10 km. Each record of the result states
          ``cell_size_requested_metres``, ``cell_size_stored_metres`` and
          ``cell_size_difference_metres``.
        - with ``align="vortex"``, as Vortex converts it: with the international foot of 0.3048 m
          whichever foot the CRS states. On a US survey foot zone 10,000 m is then stored as
          32,808.3984 ft, which is 10,000.0198 m, and the lattice shifts with it. Awaiting the
          maintainer's confirmation.
    parts:
        Pathname parts ``A``, ``B``, ``C`` and ``F``, as given, case kept. D and E always come from
        each frame's window. A stated ``C`` replaces the name only; the data type still follows
        the variable, as in Vortex.
    units:
        Relabels the record's units. It does not convert. Conversions follow the source's units and
        Vortex's ``DataConverter``: precipitation rates to depth, ``m`` to ``mm``, ``K`` to
        Celsius, ``Pa`` to kPa, dimensionless humidity to percent.
    data_type:
        ``"PER-AVER"``, ``"PER-CUM"``, ``"INST-VAL"`` or ``"INST-CUM"``. A period type on an instant
        is refused: state a window instead, as no interval is inferred. Precipitation at an instant
        is refused unless a type is stated, since it has no period for D and E to state; for GRIB2,
        ``window_rule="accumulation:<duration>"`` states the period. By default:
        an instant is INST-VAL; a period is PER-CUM when the C part is ``PRECIPITATION``, unless its units are a
        temperature, and PER-AVER otherwise. A statistic stated in the source does not change this,
        as in Vortex.
    version:
        6 or 7 for a new file (default 7). On an existing file it must match.
    align:
        ``"refuse"`` (default) or ``"vortex"``; see below.
    window_rule:
        ``"refuse"`` (default), ``"vortex"``, or ``"accumulation:<duration>"`` for GRIB input, with a
        duration of ``15M``, ``01H``, ``03H``, ``06H``, ``12H``, ``24H``, ``48H`` or ``72H``. The last
        is not Vortex's result, and states that each record is a depth over that duration: it ends at
        the file's time, starts that much earlier, and is PER-CUM. Only stamps follow from it; no cell
        changes. A variable, a file name's QPE stem or the record's own interval stating another
        duration is refused; other text in a file name is a label, so ``24H_RadarOnly_QPE_01H.grib2``
        states one hour. Also refused: a source that is not a rain depth, by its name or units, such as
        a rate, or MRMS's ``FLASH_QPE_ARI24H``, in years; ``units`` that are not a depth; any stated
        ``data_type`` but ``"PER-CUM"``; and any input that is not GRIB2 or GRIB1. When nothing in the file
        states a duration the caller's statement stands, recorded in ``accumulation_stated_by``. A run whose periods overlap
        or leave a gap writes every record and warns, as rolling products cannot be summed; records
        already in the output are not looked at. Without this, an instant of precipitation stays
        refused.
    duplicates:
        ``"refuse"`` (default) or ``"last"``; see below.
    threads:
        Warp workers, at least 1. ``None`` (the default since 0.1.2) uses every
        core available to the process (Rust's ``std::thread::available_parallelism``); give
        ``threads=1`` for a single worker. The warp caps the count at 64 and at the rows of a
        chunk. The records written do not depend on this: the
        output file is byte-identical whatever the count.
    sync:
        Flush policy of the run: ``"end"`` (default; ``True`` means the same) flushes once,
        after the last record; ``"step"`` flushes at every flush point of every record;
        ``"none"`` (or ``False``) does not flush. See
        :func:`ras_commander_fast_dss.write_timeseries`. No power-loss claim is made for
        any mode.
    progress:
        Called after each record with ``done``, ``total``, ``input`` (an index), ``variable``,
        ``frame`` and ``pathname``. If it raises, the run stops, that exception propagates with
        ``completed`` set, and the records already written remain.
    max_seconds_per_read, max_decoded_bytes_per_read, chunk_cache_bytes:
        Limits on the NetCDF reader; other formats ignore them. ``None`` keeps the reader's default,
        which :func:`netcdf_limits` reports. One read is one frame, or a variable's axes. A read may
        decode 32 GiB by default, and a frame needing more is refused, naming this setting. No time
        limit is set by default, and one small crafted file can take about 45 s in a read, so set
        ``max_seconds_per_read`` for files you did not make. ``max_seconds_per_read`` is not a
        deadline: the clock is looked at before each chunk and each filter stage, and a deflate or
        szip stage cannot be interrupted, so a read can return well after the limit. A limit of 1 s
        returned after 17.6 s on a test file whose chunks each inflate to 256 MiB twice; on ordinary
        files a stage is a fraction of a second. ``chunk_cache_bytes`` matters when one
        chunk holds many steps, as in a year of hourly grids: the cache must hold a whole step's
        chunks for the next step to be cheap.
    snodas_times:
        For SNODAS inputs; other formats ignore it. Each SNODAS file is a
        UTC product of the day ending at 06:00 UTC.

        ``"header"`` (default) takes the times from the NOHRSC header beside each grid, checked
        against the file name, and the data type from the product. A daily total (liquid and solid
        precipitation, melt, the two sublimation products; time code ``T0024``) is PER-CUM over the
        24 hours ending at 06:00 UTC of the file's date. SWE and snow depth (``T0001``) are INST-VAL
        at 06:00 UTC of that date. Every timing field of the file name is checked by the NOHRSC
        naming rules: a real date, ``TS`` (the start of the final hourly step) ``05``, the step
        ``D`` for a total and ``H`` for SWE and depth, and the offset ``P001`` for precipitation,
        SWE and depth and ``P000`` for melt and sublimation. A grid with no header, a file-name
        field with another value, or a header that disagrees with the name is refused.

        These have no qualified interpretation and are refused, naming the two options below: the
        snowpack average temperature (product ``1038``); melt with time code ``T0001``
        (historical files such as 2010's); and sublimation (``1039``, ``1050``) whose header stops
        at 05:00, an hour before the 24 hours its name means, as the 2010 headers do. The last is an
        open item until it is qualified.

        ``"vortex"`` reads as Vortex 1.1.0 does, for compatibility: times from the file name, which
        puts each daily total one day later than its header (06:00 of the file's date to 06:00 the
        next day), every daily total typed PER-AVER (precipitation included), and the snowpack
        temperature an instant. ``"header_as_stated"`` takes the header's ``Start`` and ``Stop`` as
        written, typed as Vortex types these products: PER-AVER over a period, INST-VAL at an
        instant.

        The default differs from Vortex's timing; pass ``"vortex"`` for Vortex-compatible
        records. Records already in a DSS file are never rewritten: with ``duplicates="refuse"``
        (default) a re-import that would replace one is refused.
    snodas_georeference:
        ``"vortex"`` (default) places SNODAS grids at Vortex's fixed corners; ``"header"`` takes
        them from the NOHRSC header. Before 1 October 2013 the headers' corners differ from Vortex's
        by 0.0004167 degrees. A grid with no header is refused when ``"header"`` is asked for.

        SNODAS missing cells and grid size: see the notes.
    stop:
        A :data:`StopHandle`. ``stop.request()`` from another thread ends the run before its next
        frame, and a NetCDF read in progress before its next chunk or filter stage. It does not return
        at once: a filter stage, a warp or a write in progress finishes first. A stop asked 0.2 s into
        a read returned after 17.1 s on a test file whose chunks each inflate to 256 MiB twice; on
        ordinary files the wait is a frame's work or less. The run raises ``DssError`` with
        ``completed`` set; the records already written remain, and ``in_doubt`` is ``None``, since no
        write was in progress. A Zarr store has its own stop, ``ZarrStop`` on :func:`import_zarr`,
        which raises ``DssUnsupportedRecordError`` instead.

    Returns
    -------
    ImportResult

    Raises
    ------
    DssLockedError
        HEC software holds the output. Before any decoding, or at any later write.
    DssUnsupportedRecordError
        A refusal: an unsupported input or anything below.
    DssFormatError
        An input's bytes could not be decoded.
    ValueError
        Invalid arguments, or a record the writer cannot store.
    OSError
        A file could not be read or written.

    Every error from the run has ``phase``, ``input``, ``variable``, ``frame``, ``completed`` and
    ``in_doubt``. ``completed`` lists the writes the writer acknowledged. ``in_doubt`` names the
    pathname being written when the writer failed: it may or may not be in the file, so an exception
    does not prove it absent.

    Notes
    -----
    Observed behaviour we did not implement. Each was observed on Vortex 1.1.0; each is refused by
    default, and the argument named gives Vortex's result where one exists:

    - Precipitation at an instant is stored as INST-VAL with a D part only, also on Vortex 0.13.3
      (``data_type="INST-VAL"``).
    - An origin not on whole cells, or unequal dx and dy, is rounded or averaged when stored
      (``align="vortex"``). This moves the grid. Vortex's own sample ``hms_cn_grid.tif``, in US
      survey feet with 100 ft cells, has its lower-left corner at (361198.764, 1738147.088) and is
      stored at (361200, 1738100): a move of (+0.3768, -14.3524) m, 14.357 m in all, in Vortex and
      under ``align="vortex"`` here alike. The default refuses that file unwarped and names the
      corner; state a cell size or envelope to warp it to a lattice of your choosing instead.
    - A cell size on a target in feet is converted with the international foot, also on a US
      survey foot zone (``align="vortex"``; see ``cell_size``).
    - A name holding ``qpe`` is called precipitation whatever it holds, so the real ``FLASH_QPE_ARI01H``, a
      recurrence interval in years, is stored as PRECIPITATION: INST-VAL, or PER-CUM under a one-hour file
      name (``window_rule="vortex"``).
    - A name like ``*_QPE_01H_*.grib2`` gives one-hour stamps to a variable stating another duration, as with
      a 24H file renamed. The refusal names both durations (``window_rule="vortex"``).
    - A file-name rule gives a window other than the record's own interval, as in
      ``hrrr*wrfsfcf*`` where a 0-6 h total is reported as one hour; or gives several instants in
      one file windows between midpoints (``window_rule="vortex"``).
    - One pathname is written twice in a run and the last stays (``duplicates="last"``).
    - An input without the variable is skipped; a missing shapefile means no envelope; a time with
      seconds loses them; a directory in the input's path can set the C part; a caller's WKT is
      rewritten by GDAL, with AXIS and AUTHORITY nodes added, where this library stores it as given.
      These have no opt-in.

    NetCDF. The file is read as Vortex 1.1.0's ``GridDatasetReader`` reads it, and the rules above
    then apply. Three things differ from Vortex by default, and ``window_rule="vortex"`` gives its
    record for the first two:

    - Vortex stamps some files by their name, such as ``01.nc``, ``gfs.nc``, ``prec.2017.nc`` and
      ``hrrr*wrfsfcf*``. Where that moves a step off the window the file itself states, the step is
      refused, and the refusal names both windows. One such rule is taken by default, for AORC
      precipitation. There the file's name selects the product's convention and the file's own times
      select the end instants: a name matching ``*AORC*APCP*nc4*`` says each value is the depth over
      the hour ending at its time, as NWS documents AORC, and each record ends at a time read from
      the file's ``time`` variable. No digits in the name are read or compared, so a renamed file
      keeps the stamps of its own times. The variable must be ``APCP_surface``, the file must state no
      period of its own, and its steps, if several, must be one hour apart, or the step is refused. A
      file of one step has no spacing to check, so for it the hour rests on the name and the
      variable's name. A file with time 1984-01-01 00:00 gives ``31DEC1983:2300`` to
      ``31DEC1983:2400``.
    - A variable Vortex names precipitation must be in units of depth, or a rate it converts to a
      depth. A count, an index or a percentage is refused, where Vortex stores it as PRECIPITATION.
    - Vortex reports a period type on an instant for a variable whose ``cell_methods`` says
      ``time: sum`` or ``time: mean`` and whose time axis has no bounds. Those steps are refused. So
      are variables on two-dimensional or uneven axes, or with several vertical levels, a nonzero
      false origin on Lambert or Albers, and a time with a fraction of a second.

    Grid size. A record holds at most 16,777,216 cells (2^24); this library's writer and its
    default reader both stop there, and a larger destination is refused while the run is planned,
    before a cell is decoded. The whole SNODAS grid is 6,935 x 3,351 = 23,239,185 cells, so it
    cannot be written unwarped. The normal path is to clip to the model's envelope, with a buffer
    of a few destination cells so bilinear resampling has neighbours at the edge::

        from ras_commander_fast_dss import gridded

        # The basin's bounds in SHG (Albers, metres), from the model's shapefile or HMS grid.
        west, south, east, north = 1_020_000.0, 1_830_000.0, 1_160_000.0, 1_950_000.0
        cell = 2000.0
        pad = 4 * cell
        gridded.import_gridded(
            ["SNODAS_20200101.tar"], "snow.dss",
            variable="SWE",
            target_crs="SHG", cell_size=cell, method="bilinear",
            envelope=(west - pad, south - pad, east + pad, north + pad),
        )

    ``shapefile=`` gives the same clip from the basin's bounding box. A configurable higher cap is
    not offered: it needs HEC-DSSVue, HEC-HMS and HEC library qualification first.

    SNODAS missing cells. A cell of ``-9999`` is masked before it is scaled or resampled, is
    stored as a DSS missing value, and is left out of the record's minimum, maximum, mean and range
    counts. The warp carries the mask as GDAL's ``-srcnodata`` does. Real negative values
    (sublimation) are kept. Vortex keeps ``-9999`` divided by the product's scale as a number.

    Observed through Vortex 0.13.3, which writes DSS6: most PER-CUM grids are stored as PRECIP_2_BYTE,
    which rounds each cell to 1/100; this library stores ZLIB, which keeps the cell's bits. INST-VAL
    stated over a period keeps the period's start in the DSS6 header; this library refuses that.
    """
    if isinstance(inputs, (str, os.PathLike)):
        inputs = [inputs]
    paths = [str(Path(os.fspath(p))) for p in inputs]
    if not paths:
        raise ValueError("no inputs")
    if (variable is None) == (product is None):
        raise ValueError("give exactly one of variable=, product=")
    if isinstance(version, bool) or version not in (None, 6, 7):
        raise ValueError(f"version must be 6 or 7, not {version!r}")
    threads = _threads(threads)
    if stop is not None and not isinstance(stop, StopHandle):
        raise ValueError(f"stop must be a gridded.StopHandle, not {type(stop).__name__}")
    box = _bounds(envelope)
    out = str(Path(os.fspath(output)))
    records = _native.import_gridded(
        paths,
        out,
        variable=variable,
        product=product,
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
        threads=threads,
        sync=_sync_mode(sync),
        allow_undetectable_readers=bool(allow_undetectable_readers),
        progress=progress,
        bbox=_select.source_box(bbox),
        times=_select.time_span(times),
        max_seconds_per_read=_limit("max_seconds_per_read", max_seconds_per_read),
        max_decoded_bytes_per_read=_limit("max_decoded_bytes_per_read", max_decoded_bytes_per_read),
        chunk_cache_bytes=_limit("chunk_cache_bytes", chunk_cache_bytes),
        stop=stop,
        snodas_times=snodas_times,
        snodas_georeference=snodas_georeference,
        **_parts(parts),
    )
    records = [dict(r) for r in records]
    if cell_size is not None and target_crs is not None:
        _, stored_metres = _native.gridded_cell_size(str(target_crs), float(cell_size), align)
        for r in records:
            r["cell_size_requested_metres"] = float(cell_size)
            r["cell_size_stored_metres"] = stored_metres
            r["cell_size_difference_metres"] = stored_metres - float(cell_size)
    _warn_readers(out, records)
    _warn_accumulation(out, records)
    return ImportResult(output=out, records=records)


TimeSpan = Union[None, TimeLike, Tuple[TimeLike, TimeLike]]


def _time_pair(t: TimeSpan):
    if t is None:
        return None
    if isinstance(t, tuple):
        if len(t) != 2:
            raise ValueError("a time is an instant or (start, end)")
        return (_seconds(t[0]), _seconds(t[1]))
    s = _seconds(t)
    return (s, s)


def _seconds(t: TimeLike) -> int:
    if isinstance(t, _dt.datetime) and t.tzinfo is not None:
        raise ValueError("times must be naive; convert them to the model's clock first")
    return int(to_hec_seconds(t))


def import_arrays(
    data: Any,
    output: PathLike,
    *,
    times: Sequence[TimeSpan],
    transform: Sequence[float],
    crs: str,
    variable: str,
    source_units: str,
    nodata: float = float("nan"),
    description: Optional[str] = None,
    file_name: str = "",
    envelope: Optional[Sequence[float]] = None,
    envelope_crs: Optional[str] = None,
    shapefile: Optional[PathLike] = None,
    target_crs: Optional[str] = None,
    cell_size: Optional[float] = None,
    method: str = "bilinear",
    parts: Optional[Mapping[str, str]] = None,
    units: Optional[str] = None,
    data_type: Optional[str] = None,
    version: Optional[int] = None,
    align: str = "refuse",
    duplicates: str = "refuse",
    threads: Optional[int] = None,
    sync: Union[bool, str] = "end",
    allow_undetectable_readers: bool = True,
    progress: Optional[Callable[[Dict[str, Any]], Any]] = None,
) -> ImportResult:
    """Import arrays through the same warp, unit, naming and writer steps as a file.

    Parameters
    ----------
    data:
        Shape ``(frames, rows, columns)``, or ``(rows, columns)`` for one frame. Float32 cells; a
        wider type is rounded to float32, and values that overflow, or that round onto ``nodata``, are
        refused. Masked cells become ``nodata``. Cell values that a DSS grid cannot hold as values
        are refused while writing, with the records already written reported.
    times:
        One per frame: ``None`` (no time), an instant, or ``(start, end)``. Naive times.
        Precipitation needs ``(start, end)``. AORC's stamp T is the end of the hour, so its window
        is ``(T - 1 hour, T)``, which is what Vortex reads from those files.
    transform:
        ``(origin_x, dx, origin_y, dy)``, with the origin at the outer corner of the first cell of
        each frame. ``dy`` negative means row 0 is the northern row. Negative ``dx`` is refused unless
        a cell size, target or envelope warps the grid.
    crs:
        ``"SHG"``, ``"HRAP"`` or WKT.
    variable, description, file_name:
        Names, which set the C part as Vortex's ``getCPart`` does.
    source_units:
        The units the cells are in, which set conversions.
    nodata:
        The cells' missing marker; NaN is always missing as well. Missing cells are decided on these
        values, before any unit conversion. If a warp computes a value equal to the marker from valid
        cells, the frame is refused: state a marker the data cannot reach.

    The remaining arguments are :func:`import_gridded`'s. Without a target, cell size or bounds, the
    grid is written unwarped, and must be on whole cells unless ``align="vortex"``.
    """
    mask = np.ma.getmaskarray(data) if np.ma.isMaskedArray(data) else None
    a = np.asarray(np.ma.getdata(data) if mask is not None else data)
    if a.dtype.kind not in "fiub":
        raise ValueError(f"data must be numeric, not {a.dtype}")
    if a.ndim == 2:
        a = a[np.newaxis]
        mask = None if mask is None else mask[np.newaxis]
    if a.ndim != 3:
        raise ValueError(f"data must be 2-D or 3-D; got shape {a.shape}")
    frames, ny, nx = a.shape
    if len(times) != frames:
        raise ValueError(f"{frames} frame(s) and {len(times)} time(s)")
    # The engine's own limits, before any cell is copied: copying costs what the array holds.
    limits = _native.gridded_limits()
    if frames > limits["max_records"]:
        raise DssUnsupportedRecordError(f"{frames} frames is over the limit of {limits['max_records']} records")
    if nx * ny > limits["max_source_cells"]:
        raise DssUnsupportedRecordError(
            f"{ny} x {nx} is over the limit of {limits['max_source_cells']} cells in a source grid"
        )
    marker = float(nodata)
    if not np.isfinite(marker) and not np.isnan(marker):
        raise ValueError("nodata must be finite or NaN")
    with np.errstate(over="ignore", invalid="ignore"):
        marker32 = np.float32(marker)
        if not np.isnan(marker) and not np.isfinite(marker32):
            raise ValueError(f"nodata {marker} overflows float32")
        cells = a.astype(np.float32, copy=mask is not None)
    if a.dtype.kind == "f" and (np.isfinite(a) & ~np.isfinite(cells)).any():
        raise ValueError("cells overflow float32")
    # A wider type is rounded here, so a valid cell can round onto the marker: refused, not lost.
    if a.dtype != np.float32 and not np.isnan(marker):
        rounded = (cells == marker32) & (a != marker)
        if mask is not None:
            rounded &= ~mask
        if rounded.any():
            raise ValueError(
                f"{int(rounded.sum())} valid cell(s) round to the nodata marker {marker} in float32, and "
                "would read as missing; state another marker"
            )
    if mask is not None:
        cells[mask] = marker32
    try:
        origin_x, dx, origin_y, dy = (float(v) for v in transform)
    except (TypeError, ValueError):
        raise ValueError("transform must be (origin_x, dx, origin_y, dy)") from None
    if isinstance(version, bool) or version not in (None, 6, 7):
        raise ValueError(f"version must be 6 or 7, not {version!r}")
    threads = _threads(threads)
    box = _bounds(envelope)
    out = str(Path(os.fspath(output)))
    records = _native.import_arrays(
        np.ascontiguousarray(cells.reshape(-1), dtype=np.float32),
        nx,
        ny,
        (origin_x, dx, origin_y, dy),
        crs,
        marker,
        [_time_pair(t) for t in times],
        variable,
        description,
        source_units,
        out,
        file_name=file_name,
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
        duplicates=duplicates,
        threads=threads,
        sync=_sync_mode(sync),
        allow_undetectable_readers=bool(allow_undetectable_readers),
        progress=progress,
        **_parts(parts),
    )
    records = [dict(r) for r in records]
    _warn_readers(out, records)
    return ImportResult(output=out, records=records)
