"""High-level, read-only DSS file API (pandas assembly on top of ``_native``)."""

from __future__ import annotations

import operator
import os
import struct  # P15
from pathlib import Path
from typing import Any, Dict, Optional, Union

import numpy as np
import pandas as pd

from . import _native
from ._records import Grid, PairedData, _check_grid_args
from ._records import Location  # P15
from .exceptions import DssLocationConflictError  # P15
from ._records import Profile  # P14
from ._time import TimeLike, hec_seconds_to_datetime64, to_hec_seconds
from .exceptions import DssPathnameNotFoundError, DssUnsupportedRecordError
from .exceptions import NO_DATA_STATUS, DssNoDataError  # P29

__all__ = [
    "DssFile",
    "open",
    "MISSING_MARKERS",
    "missing_mask",
    "record_type_name",
]

PathLike = Union[str, "os.PathLike[str]"]

#: HEC's missing-value markers: -901.0 (missing), -902.0 (not computed) and -FLT_MAX
#: (UNDEFINED, ``heclib_c/src/headers/zdssVals.h``). NaN is also treated as missing.
MISSING_MARKERS = (-901.0, -902.0, float(-np.finfo(np.float32).max))

# zinit6.f:ZBKDAT and zdssKeys.h record type codes.
_TYPE_NAMES = {
    20: "LOC",
    100: "RTS",
    101: "RTP",
    102: "RTS-PROFILE",
    105: "RTD",
    106: "RTD-PATTERN",
    107: "RTD-PROFILE",
    110: "ITS",
    111: "ITP",
    112: "ITS-PROFILE",
    115: "ITD",
    116: "ITD-PATTERN",
    117: "ITD-PROFILE",
    200: "PD",
    205: "PDD",
    300: "TXT",
    310: "TXT-TABLE",
    400: "GUT",
    401: "GU",
    410: "GHT",
    411: "GH",
    420: "GAT",
    421: "GA",
    430: "GST",
    431: "GS",
    450: "TIN",
}

_LIVE_STATUSES = ("live", "alias")


def record_type_name(code: int) -> str:
    """HEC abbreviation for a record type code (``UND`` when unknown)."""
    return _TYPE_NAMES.get(int(code), "UND")


def missing_mask(values: np.ndarray) -> np.ndarray:
    """Boolean mask of HEC missing markers (and NaN) in ``values`` (float32 or float64).

    Markers are compared in the array's own width, so a float32 -FLT_MAX matches.
    """
    v = np.asarray(values)
    mask = np.isnan(v)
    for m in MISSING_MARKERS:
        mask |= v == v.dtype.type(m)
    return mask


def _safe_resolve(path: Path) -> Path:
    """``Path.resolve()`` that keeps a mapped Windows drive letter instead of turning it
    into a UNC path (same rule as ``ras_commander.RasUtils.safe_resolve``)."""
    path = Path(path)
    if os.name != "nt":
        return path.resolve()
    resolved = path.resolve()
    s = str(path)
    if len(s) >= 2 and s[1] == ":" and str(resolved).startswith("\\\\"):
        return Path(os.path.normpath(str(path.absolute())))
    return resolved


def _trim_missing(times: np.ndarray, values: np.ndarray, regular: bool):
    """Drop leading and trailing missing values (regular data only), as ``ztsTrim``
    does: -901, -902 and -FLT_MAX are trimmed, NaN is not."""
    if not regular or len(values) == 0:
        return times, values
    v = np.asarray(values)
    miss = np.zeros(len(v), dtype=bool)
    for m in MISSING_MARKERS:
        miss |= v == v.dtype.type(m)
    keep = np.flatnonzero(~miss)
    if len(keep) == 0:
        return times[:0], values[:0]
    return times[keep[0] : keep[-1] + 1], values[keep[0] : keep[-1] + 1]


#: Paired-data record types (PD, PDD).
_PD_TYPES = (200, 205)


def _key_without_d(pathname: str) -> Optional[tuple]:
    """Upper-cased (A, B, C, E, F) parts of ``/A/B/C/D/E/F/``, or None when malformed."""
    parts = pathname.split("/")
    if len(parts) != 8 or parts[0] or parts[7]:
        return None
    return tuple(parts[i].upper() for i in (1, 2, 3, 5, 6))


def _index(index: int) -> int:
    """Catalog row as a non-negative int (the native layer takes ``usize``)."""
    i = operator.index(index)
    if i < 0:
        raise IndexError(f"catalog index {i} out of range")
    return i


#: HEC seconds of 01JAN3000 00:00, the pseudo-date of the first value of a regular
#: pattern record (Monolith's convention; DSS7 reader, P08a).
_PATTERN_EPOCH = 34_712_755_200

#: Pattern time-origin keys of the native dict (P25). ``pattern_time_convention`` is
#: ``"first-value-at-3000-01-01T00:00"``; ``pattern_stored_offset_seconds`` is the raw
#: stored offset (not applied; DSS6 header word 0 x 60 is unreliable);
#: ``pattern_stored_offset_source`` names where it came from;
#: ``pattern_heclib_first_seconds`` is the first-value time heclib's
#: ``ztsRetrievePattern`` (pydsstools, Monolith ``startTime``) reports. All are ``None``
#: for non-pattern records.
_PATTERN_ATTR_KEYS = (
    "pattern_time_convention",
    "pattern_stored_offset_seconds",
    "pattern_stored_offset_source",
    "pattern_heclib_first_seconds",
)


def _normalize(d: Dict[str, Any]) -> Dict[str, Any]:
    """Make native results version-independent.

    - ``units`` and ``data_type`` are stripped of blanks on both sides, as HEC's
      ``mallocAndCopyTrim`` (DSS7) and Monolith (DSS6) return them; DSS6 headers can
      store leading blanks (``"   CFS"``).
    - DSS6 regular pattern records (D part ``TS-...``) carry no dates; the DSS6 reader
      numbers them ``k * interval`` from the HEC epoch (k = 1, 2, ...). They are moved to
      the DSS7 convention: first value at 01JAN3000 00:00, one per interval.
    """
    d = dict(d)
    d["units"] = (d.get("units") or "").strip(" \0")
    d["data_type"] = (d.get("data_type") or "").strip(" \0")
    if d.get("version") == 6 and d.get("pattern") and d.get("regular") and len(d["times"]):
        d["times"] = d["times"] - int(d["interval_seconds"]) + _PATTERN_EPOCH
    return d


def _slice_window(d, start, end, trim_missing):
    """Slice normalized timestamps inclusively, keeping values and quality aligned."""
    t = d["times"]
    keep = np.ones(len(t), dtype=bool)
    if start is not None:
        keep &= t >= start
    if end is not None:
        keep &= t <= end
    times, values = t[keep], d["values"][keep]
    quality = d["quality"][keep] if d["quality"] is not None else None
    if trim_missing:
        t2, _ = _trim_missing(times, values, d["regular"])
        if len(t2):
            lo = int(np.searchsorted(times, t2[0]))
            hi = lo + len(t2)
        else:
            lo = hi = 0
        times, values = times[lo:hi], values[lo:hi]
        quality = quality[lo:hi] if quality is not None else None
    return dict(d, times=times, values=values, quality=quality)


#: P29: accepted values of the ``no_data`` keyword.
_NO_DATA_CHOICES = ("return", "refuse")


def _refuse_no_data(no_data: str) -> bool:
    """P29: ``True`` for ``no_data="refuse"``; ``ValueError`` for an unknown value."""
    if no_data not in _NO_DATA_CHOICES:
        raise ValueError(f"no_data must be 'return' or 'refuse', not {no_data!r}")
    return no_data == "refuse"


def _no_data_error(pathname: str, exc: Optional[BaseException] = None) -> DssNoDataError:
    status = getattr(exc, "status", NO_DATA_STATUS)
    return DssNoDataError(
        f"No data found for pathname: {pathname} (every value is missing; HEC DSS6 "
        f"status {status})",
        pathname=pathname,
        status=status,
    )


def _hec_no_data(d: Dict[str, Any]) -> bool:
    """P29: the DSS6 window rule (``zrrtsi.f`` lines 618-651) on a normalized dict:
    regular, not a pattern, at least one value, none other than -901/-902."""
    if d.get("version") != 6 or not d.get("regular") or d.get("pattern"):
        return False
    v = np.asarray(d["values"])
    return len(v) > 0 and bool(np.isin(v, (-901.0, -902.0)).all())


class DssFile:
    """A DSS6 or DSS7 file opened read-only.

    Use as a context manager, or call :meth:`close`::

        with ras_commander_fast_dss.open("model.dss") as f:
            cat = f.catalog()
            df = f.read_timeseries("/BASIN/LOC/FLOW//1HOUR/OBS/")
            rating = f.read_paired_data("/BASIN/LOC/STAGE-FLOW///RATING/")
            grid = f.read_grid("/SHG/BASIN/PRECIP/01JAN2020:0000/01JAN2020:0100/RADAR/")

    The file is never written, locked, or created; HEC software may hold it open.
    """

    def __init__(self, path: PathLike, *, recover: bool = False) -> None:
        self._path = Path(os.fspath(path))
        self._native = _native.open(str(self._path), bool(recover))
        self._resolved_path = _safe_resolve(self._path.absolute())

    # -- file facts -------------------------------------------------------------------

    @property
    def path(self) -> Path:
        """Path as given to :func:`open`."""
        return self._path

    @property
    def version(self) -> int:
        """DSS major version: 6 or 7."""
        return self._native.version

    @property
    def version_string(self) -> str:
        """Writer version string from the header, for example ``7-HL`` or ``6-UF``."""
        return self._native.version_string

    @property
    def recover(self) -> bool:
        """Whether the file was opened in DSS7 recovery mode."""
        return self._native.recover

    @property
    def closed(self) -> bool:
        return self._native.closed

    def signature(self) -> Dict[str, Any]:
        """Header facts: ``version``, ``version_string``, ``byte_order``,
        ``number_records``, ``file_size_bytes``, ``dead_words``, ``number_deletes``."""
        return dict(self._native.signature())

    def close(self) -> None:
        """Release the file handle. Reads after this raise ``ValueError``."""
        self._native.close()

    def __enter__(self) -> "DssFile":
        return self

    def __exit__(self, *exc: object) -> None:
        self.close()

    def __repr__(self) -> str:
        state = "closed" if self.closed else self.version_string
        return f"<DssFile {str(self._path)!r} DSS{self.version} {state}>"

    # -- catalog ----------------------------------------------------------------------

    def catalog(self, *, recover: Optional[bool] = None, all_entries: bool = False) -> pd.DataFrame:
        """Catalog as a DataFrame, one row per record, in HEC's unsorted (bin) order.

        Columns: ``pathname`` (as HEC catalogs it; DSS6 pseudo-regular records show
        their ``~`` E part), ``record_type`` (int32), ``record_type_name``, ``status``,
        ``duplicate`` (a later copy of a pathname already listed), ``recovered``
        (DSS7 recovery mode: found in an unlinked bin), ``last_write`` (DSS7 bin
        write time, UTC, naive; NaT for DSS6).

        By default only live (and alias) entries are listed, as HEC's catalog does;
        ``all_entries=True`` adds deleted/renamed entries and the ``raw_status`` and
        ``stored_pathname`` columns. ``recover`` (DSS7) overrides the open-time setting.

        Catalog anomalies (orphan bins, hash mismatches, ...) are in
        ``df.attrs["anomalies"]`` as ``(kind, byte_address, detail)`` tuples.
        """
        raw = self._native.catalog(recover)
        status = raw["status"]
        last = np.asarray(raw["last_write_ms"], dtype=np.int64)
        last_write = np.where(last >= 0, last, np.iinfo(np.int64).min).astype("datetime64[ms]")
        rtype = np.asarray(raw["record_type"], dtype=np.int32)
        df = pd.DataFrame(
            {
                "pathname": raw["pathname"],
                "record_type": rtype,
                "record_type_name": [record_type_name(t) for t in rtype],
                "status": status,
                "duplicate": np.asarray(raw["duplicate"], dtype=bool),
                "recovered": np.asarray(raw["recovered"], dtype=bool),
                "last_write": last_write,
            }
        )
        if all_entries:
            df["raw_status"] = np.asarray(raw["raw_status"], dtype=np.int64)
            df["stored_pathname"] = raw["stored_pathname"]
        else:
            df = df[df["status"].isin(_LIVE_STATUSES).to_numpy()].reset_index(drop=True)
        df.attrs["anomalies"] = list(raw["anomalies"])
        df.attrs["dss_version"] = self.version
        return df

    def pathnames(self) -> list:
        """Live pathnames in catalog order (what ``RasDss.get_catalog`` lists)."""
        return self.catalog()["pathname"].tolist()

    # -- time series ------------------------------------------------------------------

    def read_record_arrays(self, pathname: str, *, no_data: str = "return") -> Dict[str, Any]:
        """One stored record (one block of regular data), as the native dict: ``times``
        (int64 seconds since 1899-12-31), ``values`` (float32/float64, stored bits,
        missing markers kept), ``quality`` (int32 or None), ``units``, ``data_type``,
        ``time_zone``, ``record_type``, ``regular``, ``interval_seconds``,
        ``irregular_block``, ``time_offset_seconds``, ``pattern`` and block counts.
        Pattern records also fill the four ``pattern_*`` time-origin keys (P25; ``None``
        otherwise): ``pattern_time_convention``, ``pattern_stored_offset_seconds``,
        ``pattern_stored_offset_source`` and ``pattern_heclib_first_seconds``.

        **User header (DSS6).** A DSS6 time-series record can carry a *user header*: a
        free-form text area that the writing program fills and HEC never checks. HEC-DSSVue's
        USGS import, for example, writes gage metadata there as ``Keyword: value;`` items
        (``State``, ``County``, ``USGS No``, ``DrainageArea``, ``GageDatum``,
        ``VerticalDatum``, ...). Monolith shows the same text as a record's
        ``supplementalInfo``. Keys:

        - ``user_header_words``: the stored int32 words (raw, empty array when none);
        - ``user_header_bytes``: the same words as bytes in file order;
        - ``user_header_text``: the text, up to the first NUL byte, trailing blanks removed,
          then decoded as standard UTF-8 when valid, else as Java modified UTF-8 when
          valid (surrogate pairs, ``C0 80``; what Monolith's JNI layer writes), else as
          UTF-8 with each malformed sequence replaced by one U+FFFD, as Java's decoder
          does (P28, P31; ``""`` when none). Monolith reads the bytes with JNI
          ``NewStringUTF``, which differs for invalid text (docs/format/dss6.md §14.2);
        - ``user_header_encoding``: the decoding used for ``user_header_text``:
          ``"utf-8"``, ``"modified-utf-8"`` or ``"utf-8-replaced"``; ``None`` when the
          text is empty (P28, P31);
        - ``user_header_items``: ``{keyword: value}`` parsed as Monolith parses it
          (:func:`ras_commander_fast_dss.parse_user_header_items`): split at ``;``, segments with
          exactly one ``:``, trailing blanks removed, last repeat wins; an empty keyword
          or value is kept as ``""`` (Monolith raises on it). Segments without a
          ``:`` (some imports wrote ``StreamSample Creek``) are not items; they stay in the
          text;
        - ``user_header_block``: pathname of the block the header comes from;
        - ``user_header_conflict``: for dataset reads, true when some block read carries
          other text than the one reported. The reported header is HEC's choice: the last
          block found for regular data; for irregular data the last block of the window,
          so it is empty when that block is absent. ``user_header_conflict`` true with
          ``user_header_block`` ``None`` means "no header reported although blocks read
          carry one", not "two headers disagree".

        The header is informational. It is not validated, may be stale or wrong, and a
        datum it names (``VerticalDatum: NAVD88``) is never applied to values or merged
        into typed location fields (decision D37). DSS7 reads return ``None`` for every
        ``user_header_*`` key (not decoded yet).

        **No data (P29).** ``no_data="refuse"`` applies HEC DSS6's rule: a regular
        record (types 100/105, not a pattern) whose stored values are all -901 or
        -902 raises :class:`DssNoDataError` (``status`` 4), as Monolith returns -2
        for it. The default ``"return"`` returns the block. A block with any other
        value (-FLT_MAX and NaN included) is returned either way; DSS7, irregular and
        pattern records ignore the option (docs/format/dss6.md §7.4).
        """
        refuse = _refuse_no_data(no_data)
        try:
            return _normalize(self._native.read_record(pathname, refuse))
        except DssNoDataError as exc:
            raise _no_data_error(pathname, exc) from exc
        except DssPathnameNotFoundError as exc:
            raise DssPathnameNotFoundError(f"No data found for pathname: {pathname}") from exc

    def _read_dataset(self, pathname, start, end, trim_missing, refuse_no_data=False):
        try:
            return _normalize(
                self._native.read_dataset(pathname, start, end, trim_missing, refuse_no_data)
            )
        except DssNoDataError as exc:
            raise _no_data_error(pathname, exc) from exc
        except DssPathnameNotFoundError as exc:
            raise DssPathnameNotFoundError(f"No data found for pathname: {pathname}") from exc

    def read_arrays(
        self,
        pathname: str,
        start: Optional[TimeLike] = None,
        end: Optional[TimeLike] = None,
        *,
        trim_missing: bool = True,
        no_data: str = "return",
    ) -> Dict[str, Any]:
        """Dataset read as the native dict (see :meth:`read_record_arrays`); arguments
        as for :meth:`read_timeseries`. Values keep their stored bits and width.

        ``trim_missing`` (default) removes leading and trailing -901, -902 and -FLT_MAX
        from regular data, as heclib's ``ztsTrim`` does; a stored NaN is a value and is
        kept (P30).

        ``no_data`` (P29): ``"return"`` (default) returns what the window holds, which
        after the default trim can be no values. ``"refuse"`` raises
        :class:`DssNoDataError` when the blocks of a DSS6 regular window exist but no
        value in the window (before the trim) is other than -901/-902, HEC's rule for the
        whole request (``zrrtsi.f`` lines 618-651; docs/format/dss6.md §7.4). A window
        with no stored block raises the plain :class:`DssPathnameNotFoundError`. DSS7,
        irregular and pattern data ignore it. -FLT_MAX and NaN are values for the rule,
        so a window of only those is not refused (the trim may still leave no values).
        """
        refuse = _refuse_no_data(no_data)
        s = None if start is None else to_hec_seconds(start)
        e = None if end is None else to_hec_seconds(end)
        if s is not None and e is not None:
            if e < s:
                raise ValueError("time window end is before its start")
            d = self._read_dataset(pathname, s, e, trim_missing, refuse)
            # heclib's zrrtpa6 / ztsRetrievePattern return the entire pattern.
            # Window selection belongs on the public, normalized timeline.
            return _slice_window(d, s, e, trim_missing) if d["pattern"] else d
        if s is None and e is None:
            return self._read_dataset(pathname, None, None, trim_missing, refuse)
        # One-sided window: the whole dataset, cut, then trimmed.
        d = self._read_dataset(pathname, None, None, False)
        if refuse:
            # P29: the DSS6 window rule on the cut, before the trim. An empty cut
            # reads no block, which stays "no values" (HEC status 5, not 4).
            cut = _slice_window(d, s, e, False)
            if _hec_no_data(cut):
                raise _no_data_error(pathname)
        return _slice_window(d, s, e, trim_missing)

    def read_timeseries(
        self,
        pathname: str,
        start: Optional[TimeLike] = None,
        end: Optional[TimeLike] = None,
        *,
        trim_missing: bool = True,
        raw: bool = False,
        include_quality: bool = False,
        extra_attrs: bool = False,
        no_data: str = "return",
    ) -> pd.DataFrame:
        """Read a time series as a DataFrame shaped like ``RasDss.read_timeseries``.

        ``pathname`` may carry any D part (or none): the whole dataset is read, stitched
        from every block, as HEC's ``HecDss.get(path, True)`` does. ``start``/``end``
        (inclusive) limit the read; HEC moves window ends forward to the next interval
        stamp. With ``trim_missing`` (default) leading and trailing missing values are
        removed (regular data), which is what the Java path returns. The trim is heclib's
        ``ztsTrim`` (P30): -901, -902 and -FLT_MAX are missing (``zisMissingFloat`` /
        ``zisMissingDouble``); a stored NaN is a value and is kept, also at either end,
        where it reads as a NaN row. Irregular data is not trimmed.

        All-missing DSS6 blocks (P28; docs/format/dss6.md §7.4): HEC reports "no data"
        for a regular block whose values are all -901/-902 (Monolith status -2); the
        engine counts the block as found. With ``trim_missing=False`` its rows are
        returned (NaN; stored markers with ``raw=True``). With the default trim they are
        removed as heclib's ``ztsTrim`` removes them, so for a dataset made only of such
        blocks :meth:`read_arrays` returns no values and this method raises
        :class:`DssPathnameNotFoundError` ("no data", as for any read with no values left).
        :meth:`read_record` returns the block unless ``no_data="refuse"``.

        ``no_data="refuse"`` (P29) applies HEC DSS6's rule to the read as a whole: when
        the blocks of the window exist but no value in the window (before the trim) is
        other than -901/-902, :class:`DssNoDataError` (a
        :class:`DssPathnameNotFoundError`, ``status`` 4) is raised, whatever
        ``trim_missing`` is; a window in which no block exists raises the plain
        :class:`DssPathnameNotFoundError`. HEC applies the rule to the whole request,
        not block by block (``zrrtsi.f`` lines 618-651): an all-missing block in a
        window that also holds data is read as found (it counts in ``blocks_found``
        and supplies the units, type and user header when it is the last block
        found), so such reads return exactly as with the default ``"return"``. DSS7,
        irregular and pattern data ignore the option.

        Pattern windows select existing stamps inclusively on the normalized pattern
        timeline (regular patterns start at 01JAN3000 00:00), for both one-sided and
        two-sided bounds. They do not repeat the pattern or synthesize stamps outside
        it. Missing edges are trimmed after selection, with quality kept aligned.
        Fractional-second bounds raise ``ValueError``; use whole-second bounds.

        Result: a ``DatetimeIndex`` named ``datetime`` (``datetime64[s]``, naive,
        end-of-period stamps: 24:00 is the next day's 00:00) and one float64 column
        ``value``. HEC missing markers (-901, -902, -FLT_MAX) become NaN unless ``raw``
        is true, which keeps the stored values. ``include_quality`` adds an int32
        ``quality`` column when the record stores quality.

        ``attrs``: ``pathname`` (as passed), ``units``, ``type`` (HEC data type),
        ``interval`` (int minutes; 0 for irregular data; a float for sub-minute
        intervals), ``dss_file`` (resolved path).
        ``extra_attrs`` adds ``dss_version``, ``record_type``, ``time_zone``,
        ``time_offset_seconds``, ``blocks_found`` and ``warnings``; for pattern records
        also the four ``pattern_*`` time-origin keys; plus ``user_header_text``,
        ``user_header_items`` and ``user_header_encoding`` (P28) when the DSS6 user header
        is not empty, and
        ``user_header_conflict`` when its blocks disagree (see :meth:`read_record_arrays`;
        the user header is informational text, never validated).

        Raises :class:`DssPathnameNotFoundError` (a ``ValueError``) when no data exists,
        and :class:`DssUnsupportedRecordError` for non-time-series records.
        """
        d = self.read_arrays(pathname, start, end, trim_missing=trim_missing, no_data=no_data)
        if len(d["times"]) == 0:
            raise DssPathnameNotFoundError(f"No data found for pathname: {pathname}")
        return self._frame(d, pathname, raw=raw, include_quality=include_quality, extra_attrs=extra_attrs)

    def read_record(
        self,
        pathname: str,
        *,
        raw: bool = False,
        include_quality: bool = False,
        extra_attrs: bool = False,
        no_data: str = "return",
    ) -> pd.DataFrame:
        """One stored record (block) by its full pathname, as a DataFrame shaped like
        :meth:`read_timeseries`. Regular blocks cover every interval of the block, with
        missing values where nothing is stored (NaN unless ``raw``).
        ``no_data="refuse"`` (P29): see :meth:`read_record_arrays`."""
        d = self.read_record_arrays(pathname, no_data=no_data)
        return self._frame(d, pathname, raw=raw, include_quality=include_quality, extra_attrs=extra_attrs)

    # -- paired data ------------------------------------------------------------------

    def read_paired_data(
        self, pathname: str, *, ignore_d_part: bool = True, dtype: str = "stored"
    ) -> PairedData:
        """Read one paired-data record (types 200/205) as :class:`PairedData`.

        The pathname is looked up as given first (case-insensitive in DSS7). With
        ``ignore_d_part`` (default) a miss falls back to the first live paired-data
        record in catalog order whose A, B, C, E and F parts match (case-insensitive),
        as HEC's ``HecDss.get(pathname, True)`` does. ``dtype="stored"`` keeps float32
        (PD) or float64 (PDD); ``"float64"`` widens.

        Raises :class:`DssPathnameNotFoundError` (a ``ValueError``) when no record
        matches, and :class:`DssUnsupportedRecordError` when the matching record is not
        paired data (time series, grid, text, ...).
        """
        try:
            d = self._native.read_paired_data(pathname)
        except DssPathnameNotFoundError as exc:
            match = None
            if ignore_d_part:
                match = self._match_ignoring_d(pathname, _PD_TYPES, "paired data")
            if match is None:
                raise DssPathnameNotFoundError(f"No data found for pathname: {pathname}") from exc
            d = self._native.read_paired_data(match)
        return PairedData._from_native(d, dtype=dtype, dss_file=str(self._resolved_path))

    def read_paired_data_at(self, index: int, *, dtype: str = "stored") -> PairedData:
        """Paired data behind row ``index`` of ``catalog(all_entries=True)`` (the full
        catalog, every status). Useful for duplicated pathnames and DSS7 recovery
        mode. A non-live row or an index out of range raises ``IndexError``."""
        d = self._native.read_paired_data_at(_index(index))
        return PairedData._from_native(d, dtype=dtype, dss_file=str(self._resolved_path))

    # -- grids ------------------------------------------------------------------------

    def read_grid(
        self,
        pathname: str,
        *,
        missing: str = "nan",
        order: str = "native",
        max_cells: Optional[int] = None,
        preserve_legacy_missing: bool = False,
    ) -> Grid:
        """Read one grid record (types 400-431) by its full pathname as :class:`Grid`.

        ``data`` is float32 shaped ``(ny, nx)``. ``order="native"`` keeps the stored
        row order (row 0 at the bottom); ``"north_up"`` puts row 0 at the top.
        ``missing="nan"`` maps missing cells to NaN; ``"raw"`` keeps HEC's sentinels.
        ``max_cells`` raises the 16,777,216-cell safety limit (exceeding it raises
        :class:`DssError`). ``preserve_legacy_missing`` skips Java heclib's legacy
        ZLIB missing normalization (audits; combine with ``missing="raw"``). See
        :class:`Grid` for the row-order, missing-value, geometry and time conventions.

        There is no D-part matching: a grid's D and E parts are its time window.
        Raises :class:`DssPathnameNotFoundError` when the record does not exist and
        :class:`DssUnsupportedRecordError` for non-grid records or grid encodings this
        reader does not decode.
        """
        _check_grid_args(missing, order)
        try:
            d = self._native.read_grid(pathname, max_cells, bool(preserve_legacy_missing))
        except DssPathnameNotFoundError as exc:
            raise DssPathnameNotFoundError(f"No data found for pathname: {pathname}") from exc
        return Grid._from_native(d, missing=missing, order=order, dss_file=str(self._resolved_path))

    def read_grid_at(
        self,
        index: int,
        *,
        missing: str = "nan",
        order: str = "native",
        max_cells: Optional[int] = None,
        preserve_legacy_missing: bool = False,
    ) -> Grid:
        """Grid behind row ``index`` of ``catalog(all_entries=True)``; arguments as for
        :meth:`read_grid`. A non-live row or an index out of range raises
        ``IndexError``."""
        _check_grid_args(missing, order)
        d = self._native.read_grid_at(_index(index), max_cells, bool(preserve_legacy_missing))
        return Grid._from_native(d, missing=missing, order=order, dss_file=str(self._resolved_path))

    # -- profiles (P14) ----------------------------------------------------------------

    def read_profile_arrays(
        self,
        pathname: str,
        start: Optional[TimeLike] = None,
        end: Optional[TimeLike] = None,
        *,
        trim_missing: bool = True,
    ) -> Dict[str, Any]:
        """Profile dataset as the native dict (stored widths, missing markers kept):
        ``times`` (int64 HEC seconds), ``depths`` (1-D), ``values`` (2-D, times x depths),
        ``depth_units``, ``value_units``, ``data_type``, ``time_zone``, ``precision``,
        ``quality`` and the raw header fields. Arguments as for :meth:`read_profile`."""
        s = None if start is None else to_hec_seconds(start)
        e = None if end is None else to_hec_seconds(end)
        if s is not None and e is not None and e < s:
            raise ValueError("time window end is before its start")

        def native(a, b, trim):
            try:
                return self._native.read_profile(pathname, a, b, trim)
            except DssPathnameNotFoundError as exc:
                raise DssPathnameNotFoundError(f"No data found for pathname: {pathname}") from exc

        if (s is None) == (e is None):
            return native(s, e, trim_missing)
        # One-sided window: the whole dataset, cut, then trimmed (as read_arrays does).
        return _profile_slice(native(None, None, False), s, e, trim_missing)

    def read_profile(
        self,
        pathname: str,
        start: Optional[TimeLike] = None,
        end: Optional[TimeLike] = None,
        *,
        raw: bool = False,
        trim_missing: bool = True,
    ) -> Profile:
        """Read a regular-interval profile dataset (DSS7 types 102/107) as :class:`Profile`.

        ``pathname`` may carry any D part (or none): the whole dataset is read, stitched
        from every block, with the same window rules as :meth:`read_timeseries`:
        ``start``/``end`` are inclusive and HEC moves window ends forward to the next
        interval stamp; a one-sided bound reads the whole dataset and cuts it. With
        ``trim_missing`` (default) leading and trailing rows whose every depth is missing
        are removed, as ``ztsTrim`` does. Missing blocks inside the window read as
        missing rows.

        ``values`` is ``(n_times, n_depths)`` in the stored width (float32 for 102,
        float64 for 107); HEC missing markers become NaN unless ``raw`` is true.

        Raises :class:`DssPathnameNotFoundError` (a ``ValueError``) when no data exists,
        and :class:`DssUnsupportedRecordError` when the record is not a regular profile
        (a scalar time series, an irregular profile 112/117, paired data, ...). DSS6 files
        cannot store profiles (heclib ``ztsStore``: "Profiles are not implemented in
        version 6"), so an existing DSS6 record always raises the unsupported error.
        :class:`DssFormatError` is raised when the blocks of the dataset differ in depth
        count or depth values (they are never padded, truncated or realigned).
        """
        d = self.read_profile_arrays(pathname, start, end, trim_missing=trim_missing)
        if len(d["times"]) == 0:
            raise DssPathnameNotFoundError(f"No data found for pathname: {pathname}")
        return Profile._from_native(d, pathname=pathname, raw=raw, dss_file=str(self._resolved_path))

    def read_profile_record(self, pathname: str, *, raw: bool = False) -> Profile:
        """One stored profile record (one block) by its full pathname, as
        :class:`Profile`. Regular blocks cover every interval of the block, with
        missing rows where nothing is stored (NaN unless ``raw``); this is what HEC's
        single-record read (Monolith ``HecTimeSeries.read``) returns. Errors as for
        :meth:`read_profile`; DSS7 recovery mode reads the recovered copy."""
        try:
            d = self._native.read_profile_record(pathname)
        except DssPathnameNotFoundError as exc:
            raise DssPathnameNotFoundError(f"No data found for pathname: {pathname}") from exc
        return Profile._from_native(d, pathname=pathname, raw=raw, dss_file=str(self._resolved_path))

    def _match_ignoring_d(self, pathname: str, types, what: str) -> Optional[str]:
        """First live catalog pathname of a record type in ``types`` that equals
        ``pathname`` with the D part ignored. A matching record of another type raises
        :class:`DssUnsupportedRecordError`."""
        key = _key_without_d(pathname)
        if key is None:
            return None
        cat = self.catalog()
        other = None
        for p, t in zip(cat["pathname"], cat["record_type"]):
            if _key_without_d(p) == key:
                if int(t) in types:
                    return p
                if other is None:
                    other = (p, int(t))
        if other is not None:
            raise DssUnsupportedRecordError(
                f"{self._native.path}: record {other[0]} has type {other[1]}, which is not {what}"
            )
        return None

    def _frame(self, d, pathname, *, raw, include_quality, extra_attrs) -> pd.DataFrame:
        values = np.asarray(d["values"])
        out = values.astype(np.float64)
        if not raw:
            out[missing_mask(values)] = np.nan
        index = pd.DatetimeIndex(hec_seconds_to_datetime64(d["times"]), name="datetime")
        cols = {"value": out}
        if include_quality and d["quality"] is not None:
            cols["quality"] = np.asarray(d["quality"], dtype=np.int32)
        df = pd.DataFrame(cols, index=index)
        isec = d["interval_seconds"]
        df.attrs["pathname"] = pathname
        df.attrs["units"] = d["units"]
        df.attrs["type"] = d["data_type"]
        if isec is None:
            interval = 0
        elif isec % 60 == 0:
            interval = int(isec) // 60
        else:
            interval = isec / 60  # sub-minute data (for example 15Second): fractional minutes
        df.attrs["interval"] = interval
        df.attrs["dss_file"] = str(self._resolved_path)
        if extra_attrs:
            df.attrs["dss_version"] = d["version"]
            df.attrs["record_type"] = d["record_type"]
            df.attrs["time_zone"] = d["time_zone"]
            df.attrs["time_offset_seconds"] = d["time_offset_seconds"]
            df.attrs["blocks_found"] = d["blocks_found"]
            df.attrs["warnings"] = list(d["warnings"])
            if d.get("pattern"):
                # P25: pattern time-origin facts (reported, never applied to the index).
                for key in _PATTERN_ATTR_KEYS:
                    df.attrs[key] = d.get(key)
            # P26: DSS6 user header, only when present (informational, D37).
            if d.get("user_header_text"):
                df.attrs["user_header_text"] = d["user_header_text"]
                df.attrs["user_header_items"] = dict(d["user_header_items"])
                # P28, P31: "utf-8" | "modified-utf-8" | "utf-8-replaced".
                df.attrs["user_header_encoding"] = d.get("user_header_encoding")
            if d.get("user_header_conflict"):
                df.attrs["user_header_conflict"] = True
        return df

    # -- locations (P15; decision D37) ------------------------------------------------

    def read_location(self, pathname: str) -> Location:
        """Read a DSS7 location record (type 20) by its pathname, ``/A/B/Location Info////``.

        To get the location of a data record, use :meth:`read_associated_location`.
        Raises :class:`DssPathnameNotFoundError` when the record does not exist and
        :class:`DssUnsupportedRecordError` when it is not a location record, or for any
        DSS6 file (DSS6 has no location records).
        """
        try:
            d = _native.location_read(self._native, pathname)
        except DssPathnameNotFoundError as exc:
            raise DssPathnameNotFoundError(f"No location record for pathname: {pathname}") from exc
        return Location._from_native(d, requested=pathname, dss_file=str(self._resolved_path))

    def read_associated_location(self, pathname: str) -> Optional[Location]:
        """The location associated with a data record, or ``None`` when there is none.

        DSS7 (HEC ``zlocationRetrieve``): the location record ``/A/B/Location Info////``
        built from the A and B parts of ``pathname``, shared by every dataset with those
        parts. The data record itself is not required to exist, and any D part works.

        DSS6: the coordinates stored in the time-series record's own header. A full block
        pathname reads that block. Otherwise every live time-series block whose A, B, C,
        E and F parts match (case-insensitive) is read: when they all store the same
        location (coordinate bits and codes; all "none" counts as agreeing) it is
        returned, from the block HEC reports for the whole dataset (below; ``pathname``
        is that block's); when they disagree,
        :class:`DssLocationConflictError` is raised rather than picking one by bin order.
        Records that are not time series, and blocks whose coordinates are all zero
        (HEC: none stored), give ``None``.

        DSS6 supplemental text (P27): ``supplemental`` is the time-series user-header
        text (``supplemental_source="dss6_user_header"``), decoded exactly as
        ``user_header_text`` (UTF-8, modified UTF-8, else UTF-8 with U+FFFD; P28, P31;
        ``supplemental_encoding``), as Monolith fills
        ``supplementalInfo``; HEC's C library leaves it empty. A full block pathname uses
        that block's header. A pathname read as a dataset uses the block whose header HEC
        reports for the whole dataset (docs/format/dss6.md §14.4): the last block by
        D-part date (blocks whose D part is not a date sort first), and the location's
        ``pathname`` is that block. Headers that differ between blocks do not raise and
        are not merged; only the coordinate rule above raises. A location read is not a
        window read: a D part that names a block that is not stored (before, between or
        after the stored blocks) also reads the whole dataset, so for irregular data it
        gives the last stored block's text, where a P26 window read ending in an absent
        irregular block reports no header. The text is informational (decision D37): a
        datum named in it never changes values or the typed fields, and a block without
        coordinates still gives ``None``.

        Raises :class:`DssPathnameNotFoundError` for a malformed pathname (or a location
        pathname longer than 393 bytes), and for DSS6 when no matching record exists.
        """
        try:
            d = _native.location_read_associated(self._native, pathname)
        except DssPathnameNotFoundError as exc:
            if self.version != 6:
                raise DssPathnameNotFoundError(f"No data found for pathname: {pathname}") from exc
            matches = self._p15_ts_blocks().get(_key_without_d(pathname), [])
            if not matches:
                raise DssPathnameNotFoundError(f"No data found for pathname: {pathname}") from exc
            found = [(p, _native.location_read_associated(self._native, p)) for p in matches]
            d = found[0][1]
            ref = _location_key(d)
            for p, other in found[1:]:
                if _location_key(other) != ref:
                    raise DssLocationConflictError(
                        f"{self._native.path}: blocks of {pathname} store different "
                        f"coordinates ({found[0][0]} and {p}); read one block by its full "
                        "pathname"
                    )
            if d is not None:
                # P27: report the block HEC reports for the whole dataset (dss6.md
                # §14.4): the last block by D-part date, ties by pathname, as
                # DatasetIndex orders blocks. Its coordinates and codes equal found[0]'s
                # (checked above); its pathname and user-header text are its own.
                d = max(found, key=lambda pd: _p27_block_order(pd[0], pd[1]))[1]
        if d is None:
            return None
        return Location._from_native(d, requested=pathname, dss_file=str(self._resolved_path))

    def _p15_ts_blocks(self) -> Dict[tuple, list]:
        """Live DSS6 time-series pathnames grouped by (A, B, C, E, F), in catalog order.
        Built once per open file, so repeated D-part fallbacks do not re-read the
        catalog."""
        index = self.__dict__.get("_p15_ts_index")
        if index is None:
            index = {}
            cat = self.catalog()
            for p, t in zip(cat["pathname"], cat["record_type"]):
                if int(t) in _DSS6_TS_RECORD_TYPES:
                    key = _key_without_d(p)
                    if key is not None:
                        index.setdefault(key, []).append(p)
            self.__dict__["_p15_ts_index"] = index
        return index


    # -- forensic catalog-index reads (P16, DSS_TASK DT6) -----------------------------

    def read_record_at(
        self,
        index: int,
        *,
        raw: bool = False,
        include_quality: bool = False,
        extra_attrs: bool = False,
    ) -> pd.DataFrame:
        """Forensic read: the stored time-series record (one block) behind row
        ``index`` of ``catalog(all_entries=True)``, the full catalog in this file's open
        mode (``recover``). Reaches entries an ordinary pathname read cannot: later
        copies of duplicated pathnames, DSS7 orphan-bin entries that no hash-table slot
        reaches, and entries listed only in recovery mode. The record's integrity
        checks (info flag and info hash) are applied as for any read.

        The DataFrame is shaped like :meth:`read_record` (``raw``,
        ``include_quality`` and ``extra_attrs`` as there; ``attrs["pathname"]`` is the
        stored pathname). It is marked as forensic: ``attrs["read_route"]`` is
        ``"catalog_index"`` and ``attrs["provenance"]`` holds the catalog-entry
        provenance (see :meth:`diagnose`), whose ``outcome`` is ``"ordinary"`` (the
        pathname read reaches this same entry), ``"recovered"`` (DSS7 recovery mode:
        an entry in a bin the bin-block chain no longer links) or
        ``"unreachable_by_pathname"`` (decodable by address only). Ordinary reads
        carry neither key.

        Raises :class:`DssRecordUnavailableError` (a :class:`DssFormatError`) when the
        entry's info block is zeroed or invalid, :class:`DssUnsupportedRecordError`
        for records that are not time series (paired data, grids, profiles, text,
        ...), :class:`DssError` for password-protected DSS6 records (as the pathname
        readers do), and ``IndexError`` for an index out of range or a non-live
        entry.
        """
        prov, d = _native.read_record_at(self._native, _index(index))
        d = _normalize(d)
        prov = dict(prov)
        prov["record_type_name"] = record_type_name(prov["record_type"])
        prov["outcome"] = _entry_outcome(prov)
        df = self._frame(d, prov["pathname"], raw=raw, include_quality=include_quality, extra_attrs=extra_attrs)
        df.attrs["read_route"] = "catalog_index"
        df.attrs["catalog_index"] = prov["index"]
        df.attrs["provenance"] = prov
        return df

    def diagnose(self, target: Union[int, str]) -> Union[Dict[str, Any], list]:
        """Why a catalog entry is or is not readable, without decoding its values.

        ``target`` is a row of ``catalog(all_entries=True)`` (an int), or a pathname
        (a str): every row whose stored or displayed pathname equals it (ASCII
        case-insensitive, as the Rust lookups compare) is diagnosed and a list is
        returned (empty when none). Stored pathnames that are not valid UTF-8 are
        shown with Latin-1 substitution; reach those rows by index.

        Each result is the catalog-entry provenance: ``index``, ``pathname`` (stored),
        ``record_type``, ``record_type_name``, ``status``, ``raw_status``,
        ``recover_mode``, ``recovered``, ``duplicate``, ``duplicate_of``,
        ``hash_chain`` (DSS7 live/alias entries: ``own_chain``, ``other_slot``,
        ``orphan``; ``not_checked`` for DSS6 and for non-live DSS7 entries, which the
        catalog walk does not chain-check), ``bin_entry_address`` and
        ``info_address`` (byte offsets), ``reachable_by_lookup`` (a HEC hash lookup of
        the pathname lands on this entry), ``selected_by_pathname`` (the ordinary
        pathname read of this file, in its open mode, reads this entry),
        ``info_problem`` / ``info_detail`` (``None`` when the info block verifies),
        ``ts_supported`` (the time-series decoder accepts the record type),
        ``password_protected`` (DSS6), plus ``outcome`` and ``readable``. ``outcome``
        is one of ``"ordinary"``, ``"recovered"``, ``"unreachable_by_pathname"`` (all
        readable by :meth:`read_record_at`), ``"unavailable"`` (bad info block),
        ``"unsupported_record_type"`` (not a time-series type the decoder reads, or a
        password-protected DSS6 record; use :meth:`read_paired_data_at` or
        :meth:`read_grid_at` for paired data and grids) or ``"not_live"`` (deleted,
        renamed, ...).
        """
        if isinstance(target, str):
            raw = self._native.catalog(None)
            want = _ascii_upper(target)
            rows = [
                i
                for i, (p, s) in enumerate(zip(raw["pathname"], raw["stored_pathname"]))
                if _ascii_upper(p) == want or _ascii_upper(s) == want
            ]
            return [self.diagnose(i) for i in rows]
        prov = dict(_native.ts_entry_provenance(self._native, _index(target)))
        prov["record_type_name"] = record_type_name(prov["record_type"])
        prov["outcome"] = _entry_outcome(prov)
        prov["readable"] = prov["outcome"] in _READABLE_OUTCOMES
        return prov

    # -- P20: writing -----------------------------------------------------------------

    def write_timeseries(self, pathname: str, values: Any, **kwargs: Any) -> Dict[str, Any]:
        """Store a regular- or irregular-interval series in this DSS7 file (P20, P21).

        Same arguments and result as :func:`ras_commander_fast_dss.write_timeseries`. The write
        uses its own handle and lock; this object's read handle is closed for the write
        and reopened afterwards, so later reads see the new records.
        """
        from ._write import write_timeseries as _write_timeseries

        if self.closed:
            raise ValueError("I/O operation on a closed DssFile")
        # P22: write in the open file's DSS version unless the caller says otherwise.
        kwargs.setdefault("version", self.version)
        recover = self.recover
        self._native.close()
        try:
            return _write_timeseries(self._path, pathname, values, **kwargs)
        finally:
            self._native = _native.open(str(self._path), bool(recover))

    def write_paired_data(self, pathname: str, x: Any, y: Any, **kwargs: Any) -> Dict[str, Any]:
        """Store one paired-data record in this file (P23), in the file's DSS version.

        Same arguments and result as :func:`ras_commander_fast_dss.write_paired_data`. The read
        handle is closed for the write and reopened afterwards, as for
        :meth:`write_timeseries`.
        """
        from ._write_pd import write_paired_data as _write_paired_data

        if self.closed:
            raise ValueError("I/O operation on a closed DssFile")
        kwargs.setdefault("version", self.version)
        recover = self.recover
        self._native.close()
        try:
            return _write_paired_data(self._path, pathname, x, y, **kwargs)
        finally:
            self._native = _native.open(str(self._path), bool(recover))

    # -- P39: profile and location writers --------------------------------------------

    def write_profile(self, pathname: str, depths: Any, values: Any, **kwargs: Any) -> Dict[str, Any]:
        """Store a regular profile in this file (P39). Same arguments and result as
        :func:`ras_commander_fast_dss.write_profile`; the read handle is closed for the
        write and reopened afterwards."""
        from ._write_profile_location import write_profile as _write_profile

        if self.closed:
            raise ValueError("I/O operation on a closed DssFile")
        recover = self.recover
        self._native.close()
        try:
            return _write_profile(self._path, pathname, depths, values, **kwargs)
        finally:
            self._native = _native.open(str(self._path), bool(recover))

    def write_location(self, pathname: str, **kwargs: Any) -> Dict[str, Any]:
        """Store location data in this file (P39). Same arguments and result as
        :func:`ras_commander_fast_dss.write_location`; the read handle is closed for the
        write and reopened afterwards."""
        from ._write_profile_location import write_location as _write_location

        if self.closed:
            raise ValueError("I/O operation on a closed DssFile")
        recover = self.recover
        self._native.close()
        try:
            return _write_location(self._path, pathname, **kwargs)
        finally:
            self._native = _native.open(str(self._path), bool(recover))
            if hasattr(self, "_p15_block_index"):
                self._p15_block_index = None

    # -- P24: grid writer -------------------------------------------------------------

    def write_grid(self, pathname: str, data: Any, **kwargs: Any) -> Dict[str, Any]:
        """Store one grid record in this file (P24).

        Same arguments and result as :func:`ras_commander_fast_dss.write_grid`; ``version``
        defaults to this file's DSS version. The write uses its own handle and lock;
        this object's read handle is closed for the write and reopened afterwards.
        """
        from ._write_grid import write_grid as _write_grid

        if self.closed:
            raise ValueError("I/O operation on a closed DssFile")
        kwargs.setdefault("version", self.version)
        recover = self.recover
        self._native.close()
        try:
            return _write_grid(self._path, pathname, data, **kwargs)
        finally:
            self._native = _native.open(str(self._path), bool(recover))


def _location_key(d: Optional[Dict[str, Any]]) -> Optional[tuple]:
    """Comparable location content of a native location dict (pathname and record type
    excluded): stored coordinate bits and the six codes."""
    if d is None:
        return None
    keys = ("coordinate_system", "coordinate_id", "horizontal_units", "horizontal_datum",
            "vertical_units", "vertical_datum")
    return (struct.pack("<3d", d["x"], d["y"], d["z"]),) + tuple(int(d[k]) for k in keys)


#: DSS6 time-series record types grouped into datasets. Single source of truth:
#: ``dss_core::dss6::DatasetIndex::build`` (crates/dss-core/src/dss6/dataset.rs, live
#: records of types 100-119); keep the two equal (P27 review).
_DSS6_TS_RECORD_TYPES = range(100, 120)


def _p27_block_order(pathname: str, d: Dict[str, Any]) -> tuple:
    """Sort key of a DSS6 block as ``dss6::DatasetIndex`` orders a dataset's blocks: block
    date from the D part (blocks whose D part does not parse first), then pathname
    bytes. The last block in this order is the one whose user header HEC reports for a
    whole-dataset read (docs/format/dss6.md §14.4; P27)."""
    j = d.get("block_julian")
    try:
        raw = pathname.encode("latin-1")
    except UnicodeEncodeError:
        raw = pathname.encode("utf-8")
    return (j is not None, j if j is not None else 0, raw)


def open(path: PathLike, *, recover: bool = False) -> DssFile:  # noqa: A001 - mirrors io.open
    """Open a DSS6 or DSS7 file read-only.

    ``recover`` (DSS7 only): list records in bins the bin-block chain no longer links
    (interrupted writes), and read the best-preserved copy of duplicated pathnames
    (P06 recovery mode). HEC's own catalog omits these; default is HEC parity.

    Raises ``FileNotFoundError`` for a missing file and :class:`DssFormatError` for a
    file that is not DSS6/DSS7.
    """
    return DssFile(path, recover=recover)


# -- profiles (P14) ---------------------------------------------------------------------

#: Profile record types: regular 102/107 are read; irregular 112/117 are refused.
PROFILE_RECORD_TYPES = (102, 107)


def _profile_slice(d: Dict[str, Any], start, end, trim_missing: bool) -> Dict[str, Any]:
    """Cut a native profile dict to ``start``/``end`` (inclusive) and optionally drop
    leading and trailing rows whose every value is a HEC missing marker (``ztsTrim``:
    -901, -902 and -FLT_MAX; NaN is not trimmed)."""
    t = np.asarray(d["times"])
    keep = np.ones(len(t), dtype=bool)
    if start is not None:
        keep &= t >= start
    if end is not None:
        keep &= t <= end
    idx = np.flatnonzero(keep)
    lo, hi = (int(idx[0]), int(idx[-1]) + 1) if len(idx) else (0, 0)
    if trim_missing and hi > lo:
        v = np.asarray(d["values"])[lo:hi]
        miss = np.zeros(v.shape, dtype=bool)
        for m in MISSING_MARKERS:
            miss |= v == v.dtype.type(m)
        ok = np.flatnonzero(~miss.all(axis=1)) if v.shape[1] else np.arange(len(v))
        if len(ok):
            lo, hi = lo + int(ok[0]), lo + int(ok[-1]) + 1
        else:
            hi = lo
    out = dict(d, times=t[lo:hi], values=np.asarray(d["values"])[lo:hi])
    q = d.get("quality")
    if q is not None:
        out["quality"] = np.asarray(q)[lo:hi]
    return out


__all__ += ["PROFILE_RECORD_TYPES"]


# -- P16 (DSS_TASK DT6): catalog-index recovery route ----------------------------------

#: Outcomes :meth:`DssFile.read_record_at` returns data for.
_READABLE_OUTCOMES = ("ordinary", "recovered", "unreachable_by_pathname")


def _entry_outcome(prov: Dict[str, Any]) -> str:
    """Classify a catalog-entry provenance dict (see :meth:`DssFile.diagnose`).
    Precedence: not_live, unavailable, unsupported_record_type, recovered,
    unreachable_by_pathname, ordinary."""
    if prov["status"] not in _LIVE_STATUSES:
        return "not_live"
    if prov["info_problem"] is not None:
        return "unavailable"
    # The native layer reports what the decoder accepts (``ts_supported``: DSS7
    # 100/101/105/106/110/111/115/116, DSS6 100/101/105/110/111/115) and DSS6 password
    # protection, so diagnose and read_record_at cannot disagree.
    if not prov["ts_supported"] or prov["password_protected"]:
        return "unsupported_record_type"
    if prov["recovered"]:
        return "recovered"
    if not prov["selected_by_pathname"]:
        return "unreachable_by_pathname"
    return "ordinary"


_ASCII_UPPER = str.maketrans("abcdefghijklmnopqrstuvwxyz", "ABCDEFGHIJKLMNOPQRSTUVWXYZ")


def _ascii_upper(s: str) -> str:
    """ASCII-only upper case, matching Rust ``eq_ignore_ascii_case``."""
    return s.translate(_ASCII_UPPER)
