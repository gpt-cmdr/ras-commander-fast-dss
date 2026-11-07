"""Typed exceptions raised by ras_commander_fast_dss (decision D32).

Every class derives from :class:`DssError`, which derives from :class:`RuntimeError`, so
existing ``except RuntimeError`` and ``except Exception`` handlers written for the Java
path (``ras_commander.RasDss``) keep working. Where the Java path raised a more specific
builtin for the same condition, the typed class also derives from that builtin:

=============================  ============================  ===============================
Exception                      Extra base class              Raised when
=============================  ============================  ===============================
``DssError``                   ``RuntimeError``              Any other DSS failure (read
                                                             limits such as the grid cell
                                                             limit, password-protected
                                                             DSS6 records).
``DssFormatError``             (``RuntimeError``)            Not a DSS file, or damaged
                                                             structures (bad header, bad
                                                             info block, corrupt record).
``DssUnsupportedRecordError``  (``RuntimeError``)            The record exists but is not of
                                                             the kind the call reads (a
                                                             time series, paired data or a
                                                             grid), or uses an encoding
                                                             the reader does not decode
                                                             (D29).
``DssPathnameNotFoundError``   ``ValueError``                No record / no data for the
                                                             pathname. ``RasDss`` raised
                                                             ``ValueError("No data found
                                                             ...")`` for this.
``DssNoDataError``             ``DssPathnameNotFoundError``  Only with ``no_data="refuse"``
                                                             (P29): a DSS6 regular record or
                                                             window exists but holds no
                                                             value other than -901/-902
                                                             (HEC status 4, Monolith -2).
``DssLockedError``             (``RuntimeError``)            Reserved for the writer: the
                                                             file is locked by HEC
                                                             software. Never raised by
                                                             reads.
=============================  ============================  ===============================

A missing file raises the builtin :class:`FileNotFoundError` (other OS failures raise the
matching :class:`OSError` subclass). Invalid arguments raise :class:`ValueError`.
"""

from __future__ import annotations

__all__ = [
    "DssError",
    "DssFormatError",
    "DssUnsupportedRecordError",
    "DssPathnameNotFoundError",
    "DssLockedError",
]


class DssError(RuntimeError):
    """Base class for every DSS error raised by ras_commander_fast_dss."""


class DssFormatError(DssError):
    """The file is not a DSS6/DSS7 file, or a structure in it is damaged."""


class DssUnsupportedRecordError(DssError):
    """The record exists but its type is not decoded by this API (decision D29)."""


class DssPathnameNotFoundError(DssError, ValueError):
    """No record, or no data, for the requested pathname."""


class DssLockedError(DssError):
    """Reserved for writers: the file is locked by HEC software (decision D10).

    Never raised by reads. Attributes (P19a; all optional, so ``DssLockedError("msg")``
    keeps working):

    - ``reasons``: tuple of lock reasons from :func:`ras_commander_fast_dss.lock_status`
      (``"primary_write"``, ``"write_array"``, ``"exclusive"``, ``"request"``,
      ``"read_array"``, ``"stale_write_marker"``, ``"sharing_violation"``);
    - ``holder_pids``: tuple of holder process IDs, when the OS reports them (POSIX
      only; empty on Windows);
    - ``path``: the DSS file, or ``None``.
    """

    def __init__(self, *args, reasons=(), holder_pids=(), path=None):
        super().__init__(*args)
        self.reasons = tuple(reasons)
        self.holder_pids = tuple(int(p) for p in holder_pids)
        self.path = None if path is None else str(path)

    @classmethod
    def from_lock_status(cls, status, path):
        """Build the writer error from a :func:`ras_commander_fast_dss.lock_status` dict.

        Message: ``"DSS file is in use by HEC software (<reasons>): <path>"``.
        """
        reasons = tuple(status.get("reasons", ()))
        msg = f"DSS file is in use by HEC software ({', '.join(reasons)}): {path}"
        return cls(msg, reasons=reasons, holder_pids=status.get("holder_pids", ()), path=path)


# -- P15 -------------------------------------------------------------------------------


class DssLocationConflictError(DssError):
    """DSS6 blocks of one series store different location coordinates, so no single
    associated location exists for the dataset (P15). Read a block by its full pathname."""


__all__ += ["DssLocationConflictError"]


# -- P16 (DSS_TASK DT6): catalog-index recovery route --------------------------------


class DssRecordUnavailableError(DssFormatError):
    """A catalog entry exists but its record cannot be read: its info block is zeroed
    or fails verification (interrupted write). ``problem`` is ``"zeroed"``,
    ``"bad_flag"``, ``"out_of_bounds"`` or ``"unreadable"`` (both versions),
    ``"hash_mismatch"`` (DSS7: the info hash differs from the bin hash, the space was
    reused), or ``"pathname_mismatch"`` (DSS6: the info pathname differs from the bin
    pathname; the DSS6 counterpart of ``hash_mismatch``). ``"status"`` is reserved for
    a DSS7 info status that is not live. Raised by
    :meth:`DssFile.read_record_at`; no data is ever substituted. ``index`` is the
    catalog row and ``info_address`` the info block's byte offset."""

    def __init__(self, message, problem=None, index=None, info_address=None):
        super().__init__(message)
        self.problem = problem
        self.index = index
        self.info_address = info_address


__all__.append("DssRecordUnavailableError")


# -- P29: opt-in HEC DSS6 "no data" refusal -------------------------------------

#: HEC's ``ISTAT`` for "no data found, although a pathname was read"
#: (``heclib_f/src/zrrtsi.f:ZRRTSI6`` line 56, hec-dss fork tag ``7-IU-15``).
NO_DATA_STATUS = 4


class DssNoDataError(DssPathnameNotFoundError):
    """HEC DSS6 "no data": raised only when a read passes ``no_data="refuse"``.

    The record (or the blocks of a dataset window) exists, but no value read is
    other than -901 or -902, so HEC DSS6 reports the read as not found
    (``zrrtsi.f`` line 651 ``ISTAT = 4``; ``ztsRetrieveReg6.c`` returns -1; Monolith
    ``HecTimeSeries.read`` returns -2). A subclass of
    :class:`DssPathnameNotFoundError`, so existing "not found" handlers catch it.
    DSS7, irregular and pattern records never raise it (docs/format/dss6.md §7.4).

    Attributes: ``pathname`` (as requested; ``None`` when unknown) and ``status``
    (HEC's ``ISTAT``: always 4 when raised by ras_commander_fast_dss; the constructor accepts
    another value only so the class can be built by other code).
    """

    def __init__(self, message, pathname=None, status=NO_DATA_STATUS):
        super().__init__(message)
        self.pathname = None if pathname is None else str(pathname)
        self.status = int(status)


__all__ += ["DssNoDataError", "NO_DATA_STATUS"]
