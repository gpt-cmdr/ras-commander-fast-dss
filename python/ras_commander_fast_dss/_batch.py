"""P76: many time-series and paired-data records in one write call.

:func:`write_batch` stores a list of records with one open of the file (or one write
through a ``DssFile``'s held handle), one HEC lock test and lock, and one flush at the end
in the default ``sync="end"``. Each record is checked and converted by the same code as
:func:`ras_commander_fast_dss.write_timeseries` or
:func:`ras_commander_fast_dss.write_paired_data`, and written by the same native
transaction as that call, so the file holds what the separate calls would have written.
"""

from __future__ import annotations

import os
import warnings
from pathlib import Path
from typing import Any, Dict, Iterable, List, Mapping, Tuple, Union

from . import _native
from ._write import _COLLECT, _held, _sync_mode

PathLike = Union[str, "os.PathLike[str]"]

#: Record kinds :func:`write_batch` takes, and the writer whose arguments each record holds.
BATCH_KINDS = ("timeseries", "paired_data")

#: Arguments set once for the whole batch, never per record.
_BATCH_LEVEL = ("version", "sync", "allow_undetectable_readers")


def _item(name: str, args: tuple, kwargs: dict) -> Tuple[Any, ...]:
    """The native batch item of one captured single-record call (``_native.<name>``)."""
    if name == "write_rts":
        # (path, pathname, start, values, units, data_type, quality, time_zone, precision,
        #  allow_undetectable_readers)
        return ("rts",) + tuple(args[1:9])
    if name == "write_its":
        # (..., precision, time_granularity, allow_undetectable_readers)
        return ("its",) + tuple(args[1:10])
    if name == "write_rts6":
        return ("rts6",) + tuple(args[1:9])
    if name == "write_its6":
        # (path, pathname, times, values, units, data_type, quality, mode, allow)
        return ("its6",) + tuple(args[1:8])
    if name == "write_pd":
        # (path, pathname, ordinates, values, number_curves, units_independent,
        #  type_independent, units_dependent, type_dependent, labels, axis, x_precision,
        #  y_precision, time_zone, version, allow)
        return ("pd",) + tuple(args[1:14])
    raise ValueError(
        "write_batch stores regular and irregular series (with at most one quality word "
        "per value) and paired data; patterns, notes and multi-word quality are written "
        "with write_timeseries, one record per call"
    )


def write_batch(
    dss_file: PathLike,
    records: Iterable[Mapping[str, Any]],
    *,
    version: int = 7,
    sync: Union[bool, str] = "end",
    allow_undetectable_readers: bool = True,
) -> List[Dict[str, Any]]:
    """Store many time-series and paired-data records in an existing DSS file in one call.

    Parameters
    ----------
    dss_file:
        Existing DSS7 or DSS6 file, as for :func:`write_timeseries`.
    records:
        One mapping per record, in write order. ``"kind"`` is ``"timeseries"`` or
        ``"paired_data"``; the other keys are the arguments of
        :func:`write_timeseries` (``pathname``, ``values``, ``start`` or ``times``,
        ``units``, ``data_type``, ...) or :func:`write_paired_data` (``pathname``, ``x``,
        ``y``, ``x_units``, ...) by name. Regular and irregular series with at most one
        quality word per value are accepted; patterns, ``notes`` and multi-word quality
        are refused (write them with :func:`write_timeseries`).
    version, sync, allow_undetectable_readers:
        As for :func:`write_timeseries`, for the whole batch. Records must not set them.

    Returns
    -------
    list of dict
        One report per record: what its own :func:`write_timeseries` or
        :func:`write_paired_data` call would have returned.

    What a batch changes, compared with one call per record:

    - The file is opened once, and the HEC lock is tested once, taken before the first
      record and released after the last. HEC software that wants the file waits for the
      whole batch, as it waits for one long series.
    - With ``sync="end"`` (the default) the batch is flushed to storage once, after the
      last record, instead of once per record. ``"step"`` still flushes at every flush
      point of every record, and ``"none"`` never flushes.

    What it does not change: each record is written by the same ordered transaction, with
    the same in-progress marker, as its own call. A process killed during a batch leaves
    the records before the interrupted one complete and the interrupted one reconciled by
    the next write, exactly as for separate calls (``docs/format/dss7.md``, ``dss6.md``).

    Errors: every record is checked before anything is written; an invalid record raises
    its error and the file is not touched. A record refused or failed while the batch is
    written (for example an existing record of another type) stops it: the records before
    it stay written and, in the ``"end"`` and ``"step"`` modes, flushed. Its exception is
    raised with ``batch_index`` (its position in ``records``) and ``batch_reports`` (the
    reports of the records written) set. An empty ``records`` does not touch the file.
    """
    sync = _sync_mode(sync)
    if version not in (6, 7):
        raise ValueError(f"version must be 6 or 7, not {version!r}")
    from ._write import write_timeseries
    from ._write_pd import write_paired_data

    items: List[Tuple[Any, ...]] = []
    for i, rec in enumerate(records):
        try:
            if not isinstance(rec, Mapping):
                raise TypeError(f"each record must be a mapping, not {type(rec).__name__}")
            kwargs = dict(rec)
            kind = kwargs.pop("kind", None)
            if kind not in BATCH_KINDS:
                raise ValueError(f'record "kind" must be one of {BATCH_KINDS}, not {kind!r}')
            fixed = [k for k in _BATCH_LEVEL if k in kwargs]
            if fixed:
                raise TypeError(
                    f"{', '.join(fixed)} apply to the whole batch; pass them to write_batch"
                )
            kwargs.update(
                version=version, sync=sync, allow_undetectable_readers=allow_undetectable_readers
            )
            captured: List[Tuple[str, tuple, dict]] = []
            token = _COLLECT.set(lambda name, a, kw: captured.append((name, a, kw)))
            try:
                if kind == "timeseries":
                    write_timeseries(dss_file, **kwargs)
                else:
                    write_paired_data(dss_file, **kwargs)
            finally:
                _COLLECT.reset(token)
            if len(captured) != 1:
                raise RuntimeError(f"internal error: {len(captured)} native writes for one record")
            items.append(_item(*captured[0]))
        except Exception as e:  # annotate, nothing has been written
            _annotate(e, i, [])
            raise
    if not items:
        return []
    reports, failed = _native.write_batch(
        str(Path(os.fspath(dss_file))),
        int(version),
        items,
        bool(allow_undetectable_readers),
        sync=sync,
        **_held(),
    )
    reports = [dict(r) for r in reports]
    if any(r.get("readers_undetected") for r in reports):
        warnings.warn(
            f"{dss_file}: this DSS file uses HEC's old lock scheme, which cannot show "
            "whether HEC software has it open; the write held the HEC write lock but was "
            "not checked against HEC readers (pass allow_undetectable_readers=False to "
            "refuse such files)",
            RuntimeWarning,
            stacklevel=2,
        )
    if failed is not None:
        index, exc = failed
        _annotate(exc, int(index), reports)
        raise exc
    return reports


def _annotate(e: BaseException, index: int, reports: List[Dict[str, Any]]) -> None:
    try:
        e.batch_index = index  # type: ignore[attr-defined]
        e.batch_reports = reports  # type: ignore[attr-defined]
    except AttributeError:  # pragma: no cover - exception types without a __dict__
        pass


__all__ = ["write_batch", "BATCH_KINDS"]
