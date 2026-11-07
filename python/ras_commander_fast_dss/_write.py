"""DSS7 time-series writer: regular series (packet P20) and irregular series (P21).

``create`` makes an empty DSS7 file; ``write_timeseries`` stores a regular-interval
series with HEC's default store semantics (storage flag 0): values replace the stored
values at the same times, the rest of each block is kept, and blocks follow HEC's
standard layout (``packets/P20_dss7_ts_writer.md``). An ``IR-`` E part
(``IR-Day`` .. ``IR-Century``) selects the irregular writer, which takes one time per
value and follows heclib's "replace" semantics (storage flag 1, what hecdss uses;
``packets/P21_dss7_its_writer.md``).

Before every write the HEC lock words are tested; the write is refused with
:class:`~ras_commander_fast_dss.exceptions.DssLockedError` when any HEC process (HEC-RAS,
HEC-HMS, HEC-DSSVue, Monolith, hecdss, pydsstools) has the file open. The write lock is
held while writing.
"""

from __future__ import annotations

import os
import warnings
from pathlib import Path
from typing import Any, Dict, Optional, Union

import numpy as np
import pandas as pd

from . import _native
from ._time import TimeLike, to_hec_seconds

__all__ = ["create", "write_timeseries", "HEC_UNDEFINED_FLOAT"]

PathLike = Union[str, os.PathLike]

#: HEC's missing-value flag as written by current heclib: -FLT_MAX (dss7.md §7.6).
HEC_UNDEFINED_FLOAT = float(-np.finfo(np.float32).max)

_EPOCH = np.datetime64("1899-12-31T00:00:00", "s")


def _check_version(version: int) -> None:
    if version == 6:
        raise NotImplementedError("writing DSS6 files is not supported yet (packet P22)")
    if version != 7:
        raise ValueError(f"version must be 7, not {version!r}")


def create(path: PathLike, version: int = 7) -> Path:
    """Create an empty DSS7 file and return its path.

    The header defaults are those of an empty file created by HEC's ``hecdss`` 0.1.33
    (8192 hash slots, 200-word bins, version string ``7-JA``). An existing path is never
    replaced: ``FileExistsError``.

    ``version=6`` creates an empty DSS6 file with the layout HEC Monolith 3.3.27 gives a
    new DSS6 file (version string ``6-YO``, 2048 hash codes, 32 bins of 127 words a block;
    packet P22).
    """
    if version == 6:
        return _create6(path)
    _check_version(version)
    p = Path(os.fspath(path))
    _native.create_dss7(str(p))
    return p


def _values_array(values: Any, value_type: str, raw: bool) -> np.ndarray:
    arr = np.asarray(values)
    if arr.ndim != 1:
        raise ValueError("values must be one-dimensional")
    if arr.dtype.kind not in "fiub":
        raise ValueError(f"values must be numeric, not {arr.dtype}")
    if value_type == "auto":
        dtype = np.float32 if arr.dtype == np.float32 else np.float64
    elif value_type == "float":
        dtype = np.float32
    elif value_type == "double":
        dtype = np.float64
    else:
        raise ValueError('value_type must be "auto", "float" or "double"')
    # Refuse rather than convert silently (P20 §2.3): no float overflow to inf, no
    # integer that the target type cannot hold exactly, no infinities unless raw.
    with np.errstate(over="ignore", invalid="ignore"):
        out = np.ascontiguousarray(arr, dtype=dtype).copy()
    if arr.dtype.kind in "iub":
        # Explicit range test (no reliance on how a platform casts out-of-range floats):
        # integers are exact in float32 up to 2**24 and in float64 up to 2**53.
        limit = 2**24 if dtype == np.float32 else 2**53
        if arr.dtype.kind == "u":
            too_big = (arr > limit).any() if arr.size else False
        else:
            too_big = ((arr > limit) | (arr < -limit)).any() if arr.size else False
        if too_big:
            raise ValueError(
                f"integer values do not fit {np.dtype(dtype).name} exactly; convert them first"
            )
    elif arr.dtype.kind == "f":
        src = arr.astype(np.float64)
        overflow = np.isinf(out) & np.isfinite(src)
        if overflow.any():
            i = int(np.nonzero(overflow)[0][0])
            raise ValueError(
                f"value {i} ({src[i]!r}) overflows {np.dtype(dtype).name}; use value_type='double'"
            )
    if not raw:
        if np.isinf(out).any():
            i = int(np.nonzero(np.isinf(out))[0][0])
            raise ValueError(
                f"value {i} is infinite; HEC has no infinity convention (pass raw=True to store "
                "the bits as given)"
            )
        out[np.isnan(out)] = dtype(HEC_UNDEFINED_FLOAT)
    return out


def _refuse_tz(value: Any, what: str) -> None:
    if isinstance(value, pd.DatetimeIndex):
        tz = value.tz
    elif isinstance(value, str):
        try:
            tz = pd.Timestamp(value).tzinfo
        except (ValueError, TypeError):
            tz = None  # HEC-style strings such as "01JAN2020 2400" carry no zone
    else:
        tz = getattr(value, "tzinfo", None)
    if tz is not None:
        raise ValueError(
            f"{what} is time-zone aware; DSS stamps are naive. Convert explicitly, for example "
            "index.tz_convert('Etc/GMT+6').tz_localize(None), and pass time_zone= to record "
            "the zone name"
        )


_NUMERIC_TIMES = (
    "times must be dates or times (datetime64, pandas Timestamps, datetime objects or "
    "strings such as '01JAN2021 0100'), not numbers: a number has no unit or epoch"
)


def _times_array(times: Any) -> np.ndarray:
    if isinstance(times, (str, bytes)):
        raise ValueError("times must be a sequence of dates or times, one per value")
    kind = getattr(getattr(times, "dtype", None), "kind", None)
    if kind in ("i", "u", "f", "b", "c"):
        raise ValueError(_NUMERIC_TIMES)
    if isinstance(times, (pd.DatetimeIndex, pd.Series)) or (
        isinstance(times, np.ndarray) and times.dtype.kind == "M"
    ):
        t = pd.DatetimeIndex(times)
        _refuse_tz(t, "times")
        if t.isna().any():
            raise ValueError(f"times[{int(np.nonzero(t.isna())[0][0])}] is not a time (NaT)")
        if (t.nanosecond != 0).any() or (t.microsecond != 0).any():
            raise ValueError("fractional seconds are not supported")
        secs = t.values.astype("datetime64[s]") - _EPOCH
        return secs.astype(np.int64)
    out = []
    for i, x in enumerate(times):
        if isinstance(x, (bool, int, float, np.number)):
            raise ValueError(_NUMERIC_TIMES)
        if x is None or (not isinstance(x, str) and pd.isna(x)):
            raise ValueError(f"times[{i}] is not a time ({x!r})")
        _refuse_tz(x, "times")
        out.append(to_hec_seconds(x))
    return np.array(out, dtype=np.int64)


def _precision(precision: Any) -> int:
    """Decimal places: an integer only (no silent truncation of 2.7 to 2)."""
    if isinstance(precision, (bool, np.bool_)) or not isinstance(precision, (int, np.integer)):
        raise ValueError(f"precision must be an integer (-1 for unset), not {precision!r}")
    return int(precision)


def write_timeseries(
    dss_file: PathLike,
    pathname: str,
    values: Any,
    *,
    start: Optional[TimeLike] = None,
    times: Any = None,
    units: str,
    data_type: str,
    quality: Any = None,
    time_zone: Optional[str] = None,
    precision: int = -1,
    value_type: str = "auto",
    raw: bool = False,
    version: int = 7,
    allow_undetectable_readers: bool = True,
    time_granularity: Optional[int] = None,
    notes: Any = None,
) -> Dict[str, Any]:
    """Store a regular- or irregular-interval time series in an existing DSS7 file.

    Parameters
    ----------
    dss_file:
        Existing DSS7 file (create one with :func:`create`). Write only to files you own;
        never to a file HEC software has open.
    pathname:
        ``/A/B/C/D/E/F/``. E must be a standard regular interval (``1Hour``, ``15Minute``,
        ``1Day``, ``1Month``, ... or the DSS6 names ``1HOUR``, ``15MIN``, ``1MON``); D
        may be empty or a date and is replaced by each block's start date. Invalid
        pathnames raise ``ValueError``; nothing is truncated.
    values:
        1-D numeric values. ``float32`` input makes float (RTS) records and anything else
        double (RTD) records, unless ``value_type`` is ``"float"`` or ``"double"``.
        ``value_type="float"`` rounds float64 input to the nearest float32 (digits beyond
        float32 precision are lost; overflow to infinity and integers above 2**24 are
        refused). A
        ``pandas.Series`` with a ``DatetimeIndex`` supplies ``times`` when neither
        ``start`` nor ``times`` is given.
    start:
        Time of the first value (end-of-period stamp; ``"01JAN2020 2400"`` equals
        ``"02JAN2020 0000"``). A start between standard boundaries is kept as HEC's time
        offset.
    times:
        Alternative to ``start``: one stamp per value. They must be exactly regular for
        the E part (calendar steps for monthly and yearly data), else ``ValueError``.
    units, data_type:
        HEC units and type (``INST-VAL``, ``PER-AVER``, ``PER-CUM``, ``INST-CUM``).
    quality:
        Optional integer quality flag per value. A 2-D array of shape ``(n, k)`` stores
        ``k`` quality words per value (DSS7 only; packet P38).
    notes:
        DSS7 only (P38; DSS6 has no notes). One string per value (``""`` for no note,
        Latin-1, no NUL) stores character notes; an integer array of shape ``(n, k)``
        stores ``k`` integer-note words per value. As heclib stores them, notes make the
        block full length. A write without notes keeps stored character notes and drops
        stored integer notes in the block it merges with; a write whose window covers a
        whole block replaces the block, notes included.
    raw:
        By default ``NaN`` is stored as HEC's missing flag (-FLT_MAX). With ``raw=True``
        every value's bits are stored as given (NaN stays NaN). -901 and -902 are always
        stored as given; HEC treats them as missing.
    allow_undetectable_readers:
        Old-scheme DSS7 files cannot show whether HEC software has them open. By default
        (maintainer decision 2026-10-28, P19) the write goes ahead while the writer holds
        the lock, with a ``RuntimeWarning``. ``False`` refuses such files with
        :class:`~ras_commander_fast_dss.exceptions.DssLockedError` (``readers_undetectable``).
    time_granularity:
        Irregular series only (P21): seconds per stored time unit, 1, 60, 3600 or 86400.
        ``None`` (default) chooses 60 when every time is a whole minute (always for
        ``IR-Century``) and 1 otherwise. Every time must be a whole multiple of an
        explicit granularity; nothing is rounded.

    Pattern records (P38, D part ``TS-...``, both versions): a regular E part stores the
    values positionally, with no ``start=`` (the only time accepted is the reader's
    convention, the first value at 01JAN3000 00:00, P25); an ``IR-`` E part needs ``times=``
    as the reader returns them. The stored time offset is 0, as HEC stores it. HEC stores no
    quality or notes in a pattern, and DSS6 patterns are floats only. DSS6 irregular
    patterns are written as ``zsitsi6.f`` stores them but not confirmed by a HEC reader:
    Monolith 3.3.27 misreads the tested records from both writers (P33 §4, P38).

    Pseudo-regular series (P38, E part ``~1Day``, both versions): irregular data with
    ``times=``, stored as HEC stores it (``~1Day`` on DSS7; ``IR-YEAR`` with the interval
    kept in the bin on DSS6).

    Irregular series (P21): an E part ``IR-Day``, ``IR-Month``, ``IR-Year``,
    ``IR-Decade`` or ``IR-Century`` (any case) stores an irregular series. Give
    ``times=`` (or a ``Series`` with a ``DatetimeIndex``), one per value, strictly
    ascending; ``start=`` is refused, and so are duplicate times (heclib would store
    both). A ``00:00`` stamp is ``24:00`` of the previous day and lands in that day's
    block, as heclib stores it. ``IR-Century`` stores minutes, so its times must be whole
    minutes. Store semantics follow heclib's storage flag 1 ("replace", the flag hecdss
    uses): in each block the new values replace every stored value inside the new time
    window (first to last new time), stored values outside the window are kept, and
    existing blocks that lie wholly inside the window but receive no values are deleted
    (``outcome`` ``"deleted"``). Missing values are stored as given (irregular blocks are
    never trimmed). heclib 7-IU-15 and 7-JA drop some of the stored values after the
    window when more stored values precede the window than it replaces; this writer
    keeps them all (P21 REPORT §3).

    Kept values in a merged block (regular or irregular), as heclib stores them:
    a write without ``quality`` drops the quality of every value kept in the block
    (heclib reads the stored quality only when the new write has quality; with quality,
    kept values without stored quality get 0); the new ``units``, ``data_type``,
    ``time_zone`` and ``precision`` apply to the whole block, so values kept from an
    earlier write are relabeled without conversion. Write the full block with its
    quality, or the same units, to keep them.

    Refusals are checked for every block before any block is written: a refused write
    (``ValueError``, :class:`~ras_commander_fast_dss.exceptions.DssUnsupportedRecordError`)
    leaves the file unchanged.

    Crash safety (P20 REPORT §4, §7): every block is its own transaction, ordered so that a
    killed process leaves the previous record intact or the new one complete; the next
    native write repairs an interrupted one (``reconciled_interrupted_write``). If a write
    is interrupted between publishing a new record's bin entry and linking a new bin, and
    HEC software writes to the file before any native write does, the unlinked entry
    stays listed in HEC catalogs (``zcheckFile`` reports it)
    until the next native write links it; an entry added to an existing bin and interrupted
    before the counters leaves ``knumberRecords`` one low until then. No bin is ever shared
    and no other record is affected. Not power-loss safe.

    version:
        ``6`` writes to an existing DSS6 file (packet P22; ``DssFile.write_timeseries``
        passes the open file's version). DSS6 differences: E parts are the DSS6 or DSS7
        names of the minute-to-year intervals (no second intervals); an ``IR-DAY`` ..
        ``IR-CENTURY`` E part writes irregular data from ``times=`` (whole minutes), and
        values in the window from the first to the last time replace the stored ones
        (HEC's default); ``NaN`` is stored as -901 (``HEC_MISSING_DSS6``) unless ``raw``;
        units and type are at most 8 characters; ``time_zone`` must be ``None``;
        pathnames are stored in upper case, as HEC stores them. A rewritten block is
        written as a new copy at the end of the file and the old copy becomes dead space
        (crash safety): files grow under repeated updates (about 4 bytes per stored word,
        170 KB per rewrite of a monthly 1-minute block) and nothing here squeezes them.
        A rewrite drops a stored user header, as HEC does. A write refused in any block
        changes nothing.

    Returns
    -------
    dict
        ``blocks``: one dict per block (``pathname``, ``outcome`` = ``"added"``,
        ``"replaced"`` or ``"unchanged"``, or ``"deleted"`` for irregular blocks the
        window emptied, ``record_type``, ``number_data``, ``logical_number``);
        ``lock_method``; ``readers_undetected``.
    """
    if not isinstance(pathname, str):
        raise TypeError(f"pathname must be str, not {type(pathname).__name__}")
    # P38: patterns, and multi-word quality or notes, take their own paths.
    if _is_pattern(pathname):
        if version not in (6, 7):
            _check_version(version)
        return _write_pattern(
            dss_file, pathname, values, start=start, times=times, units=units,
            data_type=data_type, quality=quality, notes=notes, time_zone=time_zone,
            precision=precision, value_type=value_type, raw=raw, version=version,
            allow_undetectable_readers=allow_undetectable_readers,
            time_granularity=time_granularity,
        )
    if notes is not None or _is_quality_words(quality):
        if version == 6:
            from .exceptions import DssUnsupportedRecordError

            raise DssUnsupportedRecordError(
                "DSS6 stores no notes and one quality word per value (zsrtsc6.f, zsitsi6.f; "
                "P33 section 1)"
            )
        _check_version(version)
        return _write_variants(
            dss_file, pathname, values, start=start, times=times, units=units,
            data_type=data_type, quality=quality, notes=notes, time_zone=time_zone,
            precision=precision, value_type=value_type, raw=raw,
            allow_undetectable_readers=allow_undetectable_readers,
            time_granularity=time_granularity,
        )
    if version == 6:
        return _write_timeseries6(
            dss_file, pathname, values, start=start, times=times, units=units,
            data_type=data_type, quality=quality, time_zone=time_zone, precision=precision,
            value_type=value_type, raw=raw, time_granularity=time_granularity,
            allow_undetectable_readers=allow_undetectable_readers,
        )
    _check_version(version)
    if _is_irregular(pathname):
        return _write_irregular(
            dss_file,
            pathname,
            values,
            start=start,
            times=times,
            units=units,
            data_type=data_type,
            quality=quality,
            time_zone=time_zone,
            precision=precision,
            value_type=value_type,
            raw=raw,
            allow_undetectable_readers=allow_undetectable_readers,
            time_granularity=time_granularity,
        )
    if time_granularity is not None:
        raise ValueError("time_granularity applies to irregular (IR-) series only")
    if isinstance(values, pd.Series) and start is None and times is None:
        if isinstance(values.index, pd.DatetimeIndex):
            times = values.index
        values = values.to_numpy()
    vals = _values_array(values, value_type, raw)
    n = len(vals)
    if n == 0:
        raise ValueError("no values")
    if (start is None) == (times is None):
        raise ValueError("give exactly one of start= or times=")
    if times is not None:
        t = _times_array(times)
        if len(t) != n:
            raise ValueError(f"{len(t)} times for {n} values")
        expected = _native.rts_value_times(pathname, int(t[0]), n)
        bad = np.nonzero(expected != t)[0]
        if len(bad):
            i = int(bad[0])
            raise ValueError(
                f"times are not regular for {pathname!r}: value {i} is at "
                f"{_EPOCH + np.timedelta64(int(t[i]), 's')}, expected "
                f"{_EPOCH + np.timedelta64(int(expected[i]), 's')}"
            )
        start_s = int(t[0])
    else:
        _refuse_tz(start, "start")
        start_s = to_hec_seconds(start)
    q = None
    if quality is not None:
        qa = np.asarray(quality)
        i32 = np.iinfo(np.int32)
        if qa.dtype.kind == "u" and qa.size and (qa > i32.max).any():
            # Before any signed cast: 2**64 - 1 would wrap to -1.
            raise ValueError("quality flags must fit in 32 bits")
        if qa.dtype.kind not in "iub":
            if qa.dtype.kind != "f" or not np.all(np.isfinite(qa)) or not np.all(qa == np.floor(qa)):
                raise ValueError("quality flags must be integers (no fractions, NaN or inf)")
        q = np.ascontiguousarray(qa, dtype=np.int64)
        if q.ndim != 1 or len(q) != n:
            raise ValueError("quality must be 1-D with one flag per value")
        if (q < np.iinfo(np.int32).min).any() or (q > np.iinfo(np.int32).max).any():
            raise ValueError("quality flags must fit in 32 bits")
        q = q.astype(np.int32)
    report = _native.write_rts(
        str(Path(os.fspath(dss_file))),
        pathname,
        start_s,
        vals,
        units,
        data_type,
        q,
        time_zone,
        _precision(precision),
        bool(allow_undetectable_readers),
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


# -- P21: irregular series ---------------------------------------------------------


def _is_irregular(pathname: str) -> bool:
    """True when the E part names an irregular block (``IR-...``, any case) or a
    pseudo-regular interval (``~1Day``, P38)."""
    parts = pathname.split("/")
    return len(parts) == 8 and (parts[5].upper().startswith("IR-") or parts[5].startswith("~"))


def _is_pattern(pathname: str) -> bool:
    """True when the D part names a pattern record (``TS-...``, any case; P38)."""
    parts = pathname.split("/")
    return len(parts) == 8 and parts[4].upper().startswith("TS-")


def _is_quality_words(quality: Any) -> bool:
    return quality is not None and np.ndim(quality) == 2


def _word_matrix(a: Any, n: int, what: str) -> tuple:
    """An ``(n, k)`` integer array as flat int32 words and ``k``."""
    m = np.asarray(a)
    if m.ndim != 2 or m.shape[0] != n or m.shape[1] < 1:
        raise ValueError(f"{what} must have shape (n, k) with one row per value")
    if m.dtype.kind not in "iub":
        if m.dtype.kind != "f" or not np.all(np.isfinite(m)) or not np.all(m == np.floor(m)):
            raise ValueError(f"{what} must be integers (no fractions, NaN or inf)")
    i32 = np.iinfo(np.int32)
    if m.dtype.kind == "u" and m.size and (m > i32.max).any():
        raise ValueError(f"{what} must fit in 32 bits")
    w = m.astype(np.int64)
    if (w < i32.min).any() or (w > i32.max).any():
        raise ValueError(f"{what} must fit in 32 bits")
    return np.ascontiguousarray(w.reshape(-1), dtype=np.int32), int(m.shape[1])


def _warn_old_scheme(report: Dict[str, Any], dss_file: PathLike) -> None:
    if report.get("readers_undetected"):
        warnings.warn(
            f"{dss_file}: this DSS file uses HEC's old lock scheme, which cannot show "
            "whether HEC software has it open; the write held the HEC write lock but was "
            "not checked against HEC readers (pass allow_undetectable_readers=False to "
            "refuse such files)",
            RuntimeWarning,
            stacklevel=4,
        )


def _write_variants(
    dss_file: PathLike,
    pathname: str,
    values: Any,
    *,
    start: Any,
    times: Any,
    units: str,
    data_type: str,
    quality: Any,
    notes: Any,
    time_zone: Optional[str],
    precision: int,
    value_type: str,
    raw: bool,
    allow_undetectable_readers: bool,
    time_granularity: Optional[int],
) -> Dict[str, Any]:
    """DSS7 write with multi-word quality or notes (packet P38)."""
    irregular = _is_irregular(pathname)
    if isinstance(values, pd.Series):
        if start is None and times is None and isinstance(values.index, pd.DatetimeIndex):
            times = values.index
        values = values.to_numpy()
    vals = _values_array(values, value_type, raw)
    n = len(vals)
    if n == 0:
        raise ValueError("no values")
    q = qes = None
    if quality is not None:
        if _is_quality_words(quality):
            q, qes = _word_matrix(quality, n, "quality")
        else:
            q, qes = _quality_array(quality, n), 1
    inotes = cnotes = None
    nes = 0
    if notes is not None:
        arr = None if isinstance(notes, (list, tuple)) and any(isinstance(x, (str, bytes)) for x in notes) else np.asarray(notes)
        if arr is not None and arr.dtype.kind in "iubf":
            if arr.ndim == 1:
                arr = arr.reshape(-1, 1)
            inotes, nes = _word_matrix(arr, n, "integer notes")
        else:
            seq_ = list(notes)
            if len(seq_) != n:
                raise ValueError(f"{len(seq_)} notes for {n} values")
            try:
                cnotes = [x if isinstance(x, bytes) else str(x).encode("latin-1") for x in seq_]
            except UnicodeEncodeError as e:
                raise ValueError(f"character notes must be Latin-1: {e}") from None
    path = str(Path(os.fspath(dss_file)))
    kw = dict(
        quality=q, quality_element_size=qes or 1, inotes=inotes, inote_element_size=nes,
        cnotes=cnotes, time_zone=time_zone, precision=_precision(precision),
        allow_undetectable_readers=bool(allow_undetectable_readers),
    )
    if irregular:
        if start is not None:
            raise ValueError("irregular (IR-) series take times= (one per value), not start=")
        if times is None:
            raise ValueError("irregular (IR-) series need times= (one per value)")
        t = np.ascontiguousarray(_times_array(times), dtype=np.int64)
        if t.ndim != 1 or len(t) != n:
            raise ValueError(f"{len(t)} times for {n} values")
        if time_granularity is not None:
            if isinstance(time_granularity, bool) or not isinstance(time_granularity, (int, np.integer)):
                raise ValueError("time_granularity must be an integer number of seconds")
            time_granularity = int(time_granularity)
        report = _native.write_its_variants(
            path, pathname, t, vals, units, data_type, time_granularity=time_granularity, **kw
        )
    else:
        if time_granularity is not None:
            raise ValueError("time_granularity applies to irregular (IR-) series only")
        if (start is None) == (times is None):
            raise ValueError("give exactly one of start= or times=")
        if times is not None:
            t = _times_array(times)
            if len(t) != n:
                raise ValueError(f"{len(t)} times for {n} values")
            expected = _native.rts_value_times(pathname, int(t[0]), n)
            if (expected != t).any():
                raise ValueError(f"times are not regular for {pathname!r}")
            start_s = int(t[0])
        else:
            _refuse_tz(start, "start")
            start_s = to_hec_seconds(start)
        report = _native.write_rts_variants(path, pathname, start_s, vals, units, data_type, **kw)
    _warn_old_scheme(report, dss_file)
    return dict(report)


def _write_pattern(
    dss_file: PathLike,
    pathname: str,
    values: Any,
    *,
    start: Any,
    times: Any,
    units: str,
    data_type: str,
    quality: Any,
    notes: Any,
    time_zone: Optional[str],
    precision: int,
    value_type: str,
    raw: bool,
    version: int,
    allow_undetectable_readers: bool,
    time_granularity: Optional[int],
) -> Dict[str, Any]:
    """One pattern record (packet P38; ``ztsStorePattern``, ``zsrtpa6``, ``zsitsi6``)."""
    if quality is not None or notes is not None:
        raise ValueError(
            "HEC stores no quality or notes in a pattern record (ztsStorePattern); pass neither"
        )
    irregular = _is_irregular(pathname)
    if pathname.split("/")[5].startswith("~"):
        raise ValueError("a pattern E part is a regular or IR- interval, not pseudo-regular")
    if isinstance(values, pd.Series):
        if times is None and start is None and isinstance(values.index, pd.DatetimeIndex):
            times = values.index
        values = values.to_numpy()
    if version == 6:
        vals = _values_array(values, value_type if value_type != "auto" else "float", True)
        if not raw:
            vals[np.isnan(vals)] = vals.dtype.type(HEC_MISSING_DSS6)
    else:
        vals = _values_array(values, value_type, raw)
    n = len(vals)
    if n == 0:
        raise ValueError("no values")
    t = None
    if irregular:
        if start is not None:
            raise ValueError("an irregular pattern takes times= (one per value), not start=")
        if times is None:
            raise ValueError("an irregular pattern needs times= (one per value)")
        t = np.ascontiguousarray(_times_array(times), dtype=np.int64)
        if t.ndim != 1 or len(t) != n:
            raise ValueError(f"{len(t)} times for {n} values")
    else:
        if time_granularity is not None:
            raise ValueError("time_granularity applies to irregular patterns only")
        # P25: the first value of a regular pattern is at 01JAN3000 00:00; nothing else
        # can be stored, so any other time would be lost silently.
        if start is not None and times is not None:
            raise ValueError("give at most one of start= or times=")
        origin = to_hec_seconds("01JAN3000 0000")
        if start is not None:
            _refuse_tz(start, "start")
            if to_hec_seconds(start) != origin:
                raise ValueError(
                    "a regular pattern is positional: its first value is at 01JAN3000 00:00 "
                    "(P25); pass no start="
                )
        if times is not None:
            given = _times_array(times)
            if len(given) != n or int(given[0]) != origin:
                raise ValueError(
                    "a regular pattern is positional: times= must be the reader's, one per "
                    "value from 01JAN3000 00:00 (P25); pass no times="
                )
            if (np.diff(given) <= 0).any():
                raise ValueError("pattern times must be strictly ascending")
    prec = _precision(precision)
    report = _native.write_pattern(
        str(Path(os.fspath(dss_file))), pathname, vals, units, data_type, times=t,
        time_zone=time_zone, precision=prec, time_granularity=time_granularity,
        version=int(version), allow_undetectable_readers=bool(allow_undetectable_readers),
    )
    _warn_old_scheme(report, dss_file)
    return dict(report)


def _quality_array(quality: Any, n: int) -> Optional[np.ndarray]:
    if quality is None:
        return None
    qa = np.asarray(quality)
    i32 = np.iinfo(np.int32)
    if qa.dtype.kind == "u" and qa.size and (qa > i32.max).any():
        raise ValueError("quality flags must fit in 32 bits")
    if qa.dtype.kind not in "iub":
        if qa.dtype.kind != "f" or not np.all(np.isfinite(qa)) or not np.all(qa == np.floor(qa)):
            raise ValueError("quality flags must be integers (no fractions, NaN or inf)")
    q = np.ascontiguousarray(qa, dtype=np.int64)
    if q.ndim != 1 or len(q) != n:
        raise ValueError("quality must be 1-D with one flag per value")
    if (q < i32.min).any() or (q > i32.max).any():
        raise ValueError("quality flags must fit in 32 bits")
    return q.astype(np.int32)


def _write_irregular(
    dss_file: PathLike,
    pathname: str,
    values: Any,
    *,
    start: Any,
    times: Any,
    units: str,
    data_type: str,
    quality: Any,
    time_zone: Optional[str],
    precision: int,
    value_type: str,
    raw: bool,
    allow_undetectable_readers: bool,
    time_granularity: Optional[int],
) -> Dict[str, Any]:
    if start is not None:
        raise ValueError("irregular (IR-) series take times= (one per value), not start=")
    if isinstance(values, pd.Series) and times is None:
        if isinstance(values.index, pd.DatetimeIndex):
            times = values.index
        values = values.to_numpy()
    if times is None:
        raise ValueError("irregular (IR-) series need times= (one per value)")
    vals = _values_array(values, value_type, raw)
    n = len(vals)
    if n == 0:
        raise ValueError("no values")
    t = np.ascontiguousarray(_times_array(times), dtype=np.int64)
    if t.ndim != 1 or len(t) != n:
        raise ValueError(f"{len(t)} times for {n} values")
    q = _quality_array(quality, n)
    if time_granularity is not None:
        if isinstance(time_granularity, bool) or not isinstance(
            time_granularity, (int, np.integer)
        ):
            raise ValueError("time_granularity must be an integer number of seconds")
        time_granularity = int(time_granularity)
    report = _native.write_its(
        str(Path(os.fspath(dss_file))),
        pathname,
        t,
        vals,
        units,
        data_type,
        q,
        time_zone,
        _precision(precision),
        time_granularity,
        bool(allow_undetectable_readers),
    )
    if report.get("readers_undetected"):
        warnings.warn(
            f"{dss_file}: this DSS file uses HEC's old lock scheme, which cannot show "
            "whether HEC software has it open; the write held the HEC write lock but was "
            "not checked against HEC readers (pass allow_undetectable_readers=False to "
            "refuse such files)",
            RuntimeWarning,
            stacklevel=3,
        )
    return dict(report)


# =====================================================================================
# P22: DSS6 time-series writer (``dss_core::dss6::write``).
# =====================================================================================

#: HEC's DSS6 missing value (dss6.md section 7.4): ``NaN`` is stored as this on DSS6
#: files unless ``raw=True``. DSS6 readers (HEC-RAS 4.x-5.x, ``ZRRTSI6``) recognise
#: -901 and -902, not -FLT_MAX.
HEC_MISSING_DSS6 = -901.0


def _create6(path: PathLike) -> Path:
    p = Path(os.fspath(path))
    _native.create_dss6(str(p))
    return p


def _write_timeseries6(
    dss_file: PathLike,
    pathname: str,
    values: Any,
    *,
    start: Optional[TimeLike],
    times: Any,
    units: str,
    data_type: str,
    quality: Any,
    time_zone: Optional[str],
    precision: int,
    value_type: str,
    raw: bool,
    allow_undetectable_readers: bool,
    time_granularity: Optional[int] = None,
) -> Dict[str, Any]:
    """DSS6 branch of :func:`write_timeseries` (packet P22). Uses the shared helpers of
    the DSS7 writers (``_values_array``, ``_times_array`` with its numeric-time and NaT
    refusals, ``_quality_array``, ``_precision``, ``_is_irregular``)."""
    if time_granularity is not None:
        raise ValueError("time_granularity is not written to DSS6 files (DSS6 stores minutes)")
    irregular = _is_irregular(pathname)
    if isinstance(values, pd.Series) and start is None and times is None:
        if isinstance(values.index, pd.DatetimeIndex):
            times = values.index
        values = values.to_numpy()
    vals = _values_array(values, value_type, True)
    if not raw:
        if np.isinf(vals).any():
            i = int(np.nonzero(np.isinf(vals))[0][0])
            raise ValueError(
                f"value {i} is infinite; HEC has no infinity convention (pass raw=True to store "
                "the bits as given)"
            )
        vals[np.isnan(vals)] = vals.dtype.type(HEC_MISSING_DSS6)
    n = len(vals)
    if n == 0:
        raise ValueError("no values")
    if time_zone is not None:
        raise ValueError("time_zone is not written to DSS6 files; pass time_zone=None")
    prec = _precision(precision)
    q = _quality_array(quality, n)
    path = str(Path(os.fspath(dss_file)))
    if irregular:
        if start is not None:
            raise ValueError("irregular (IR-) series take times= (one per value), not start=")
        if times is None:
            raise ValueError("irregular (IR-) series need times= (one per value)")
        t = np.ascontiguousarray(_times_array(times), dtype=np.int64)
        if t.ndim != 1 or len(t) != n:
            raise ValueError(f"{len(t)} times for {n} values")
        report = _native.write_its6(
            path, pathname, t, vals, units, data_type, q, "replace",
            bool(allow_undetectable_readers),
        )
    else:
        if (start is None) == (times is None):
            raise ValueError("give exactly one of start= or times=")
        if times is not None:
            t = _times_array(times)
            if len(t) != n:
                raise ValueError(f"{len(t)} times for {n} values")
            expected = _native.rts6_value_times(pathname, int(t[0]), n)
            bad = np.nonzero(expected != t)[0]
            if len(bad):
                i = int(bad[0])
                raise ValueError(
                    f"times are not regular for {pathname!r}: value {i} is at "
                    f"{_EPOCH + np.timedelta64(int(t[i]), 's')}, expected "
                    f"{_EPOCH + np.timedelta64(int(expected[i]), 's')}"
                )
            start_s = int(t[0])
        else:
            _refuse_tz(start, "start")
            start_s = to_hec_seconds(start)
        report = _native.write_rts6(
            path, pathname, start_s, vals, units, data_type, q, None, prec,
            bool(allow_undetectable_readers),
        )
    # DSS6 files before 6-M (and so every file whose readers cannot be detected, before
    # 6-K) are refused, so report["readers_undetected"] is always False here.
    return dict(report)


__all__ += ["HEC_MISSING_DSS6"]
