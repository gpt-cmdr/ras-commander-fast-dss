"""Paired-data and grid results (P10/P11 readers, exposed in P12).

Both classes wrap the native dicts from ``_native`` (NumPy arrays plus plain values);
:class:`~ras_commander_fast_dss.DssFile` builds them.
"""

from __future__ import annotations

import re  # P15

from dataclasses import dataclass, field
from typing import Any, Dict, List, Optional, Tuple

import numpy as np
import pandas as pd

from ._time import hec_seconds_to_datetime64

__all__ = ["PairedData", "Grid", "GRID_UNDEFINED", "grid_missing_mask"]

#: Missing value of decoded grid cells (Java heclib ``UNDEFINED``, -FLT_MAX).
GRID_UNDEFINED = float(-np.finfo(np.float32).max)

#: ``zStructSpatialGrid.h:dataType`` codes.
_GRID_DATA_TYPES = {0: "PER-AVER", 1: "PER-CUM", 2: "INST-VAL", 3: "INST-CUM"}

_PD_DTYPES = ("stored", "float64")
_GRID_MISSING = ("nan", "raw")
_GRID_ORDERS = ("native", "north_up")


# -- paired data ------------------------------------------------------------------------


@dataclass(frozen=True, eq=False)
class PairedData:
    """One paired-data record (types 200 PD and 205 PDD), DSS6 or DSS7.

    ``ordinates`` holds the independent variable (``n_ordinates`` values) and
    ``values`` the curves, shaped ``(n_curves, n_ordinates)`` (row ``i`` is curve
    ``i``). With ``dtype="stored"`` both keep the stored width (float32 for 200,
    float64 for 205) and bits; ``dtype="float64"`` widens them (exact). HEC missing
    markers (-901, -902, -FLT_MAX) are not converted: paired data rarely holds them,
    and the Java path returns them as stored too.

    ``labels`` are the curve labels as HEC's C library returns them: at most one per
    curve, possibly fewer, possibly empty strings, trailing blanks removed and
    **leading blanks kept**. (Monolith's Java DSS6 path also strips leading blanks,
    so a DSS6 label ``" 5%LIMIT"`` reads ``"5%LIMIT"`` there; see P10 report §3.)

    ``units_*``/``type_*`` are HEC's four unit/type strings (``type_independent`` is
    for example ``LINEAR``, ``LOG`` or ``PROB``). ``x_precision``/``y_precision`` are
    -1 when not set. ``time_zone`` is DSS7 only (``None`` for DSS6) and never applied.
    ``parameters`` are the independent and dependent names from the C part
    (``STAGE-FLOW`` gives ``("STAGE", "FLOW")``).
    """

    pathname: str
    record_type: int
    ordinates: np.ndarray
    values: np.ndarray
    units_independent: str
    type_independent: str
    units_dependent: str
    type_dependent: str
    labels: List[str]
    x_precision: int
    y_precision: int
    independent_is_x_axis: int
    time_zone: Optional[str]
    user_header: np.ndarray
    parameters: Tuple[str, str]
    dss_version: int
    stored_dtype: np.dtype
    dss_file: str = ""

    @classmethod
    def _from_native(cls, d: Dict[str, Any], *, dtype: str, dss_file: str) -> "PairedData":
        if dtype not in _PD_DTYPES:
            raise ValueError(f"dtype must be one of {_PD_DTYPES}, not {dtype!r}")
        ordinates = np.asarray(d["ordinates"])
        values = np.asarray(d["values"])
        stored = values.dtype
        if dtype == "float64":
            ordinates = ordinates.astype(np.float64)
            values = values.astype(np.float64)
        return cls(
            pathname=d["pathname"],
            record_type=int(d["record_type"]),
            ordinates=ordinates,
            values=values,
            units_independent=d["units_independent"],
            type_independent=d["type_independent"],
            units_dependent=d["units_dependent"],
            type_dependent=d["type_dependent"],
            labels=list(d["labels"]),
            x_precision=int(d["x_precision"]),
            y_precision=int(d["y_precision"]),
            independent_is_x_axis=int(d["independent_is_x_axis"]),
            time_zone=d["time_zone"],
            user_header=np.asarray(d["user_header"], dtype=np.int32),
            parameters=tuple(d["parameters"]),
            dss_version=int(d["version"]),
            stored_dtype=stored,
            dss_file=dss_file,
        )

    @property
    def n_curves(self) -> int:
        return int(self.values.shape[0])

    @property
    def n_ordinates(self) -> int:
        return int(self.ordinates.shape[0])

    def curve(self, i: int) -> np.ndarray:
        """Values of curve ``i`` (0-based; negative counts from the end)."""
        n = self.n_curves
        if not -n <= i < n:
            raise IndexError(f"curve {i} out of range for {n} curve(s)")
        return self.values[i]

    def column_names(self) -> List[str]:
        """Column names used by :meth:`to_dataframe`: the labels when every curve has
        a distinct non-blank label, otherwise ``curve_1`` .. ``curve_n``."""
        n = self.n_curves
        labels = self.labels
        if len(labels) == n and all(s.strip() for s in labels) and len(set(labels)) == n:
            return list(labels)
        return [f"curve_{i + 1}" for i in range(n)]

    def to_dataframe(self) -> pd.DataFrame:
        """One column per curve, indexed by the ordinates.

        The index is named after the independent parameter of the C part (``ordinate``
        when there is none). ``attrs`` carry ``pathname``, the four unit/type strings,
        ``labels`` and ``dss_file``.
        """
        index = pd.Index(self.ordinates, name=self.parameters[0] or "ordinate")
        df = pd.DataFrame(
            {name: self.values[i] for i, name in enumerate(self.column_names())},
            index=index,
        )
        df.attrs.update(
            pathname=self.pathname,
            units_independent=self.units_independent,
            type_independent=self.type_independent,
            units_dependent=self.units_dependent,
            type_dependent=self.type_dependent,
            labels=list(self.labels),
            dss_file=self.dss_file,
        )
        return df


# -- grids ------------------------------------------------------------------------------


def grid_missing_mask(
    values: np.ndarray, null_value: float, grid_type: Optional[str] = None
) -> np.ndarray:
    """Missing cells of decoded grid values: NaN, -FLT_MAX (``GRID_UNDEFINED``), -901,
    -902 and, for specified grids, the grid's null marker (``Grid::is_missing``).
    Compared in float32, the width cells are stored in.

    ``grid_type`` is :attr:`Grid.grid_type`. P34: as HEC Monolith does, ``null_value``
    is a missing marker only when ``grid_type`` is ``"specified"``; Monolith's
    ``GridStructContainer`` carries ``_nullValue`` only for ``SpecifiedGridInfo``, so a
    DSS7 HRAP or Albers grid it writes stores 0.0 there, and those 0.0 cells are data.
    (HEC's C header ``zStructSpatialGrid.h:74`` documents ``_nullValue`` as the no-data
    value for every grid.) ``grid_type=None`` (unknown) keeps the older rule and
    applies ``null_value`` whatever the grid type."""
    v = np.asarray(values, dtype=np.float32)
    mask = np.isnan(v)
    markers = [GRID_UNDEFINED, -901.0, -902.0]
    if grid_type is None or grid_type == "specified":
        markers.insert(0, null_value)
    for m in markers:
        mask |= v == np.float32(m)
    return mask


@dataclass(frozen=True, eq=False)
class Grid:
    """One grid record (types 400-431: undefined, HRAP, Albers, specified), DSS6 or DSS7.

    **Cells.** ``data`` is an owned, C-contiguous ``float32`` array shaped
    ``(ny, nx)``. With ``order="native"`` (default) rows are in stored order: x varies
    fastest and row 0 is the **bottom** (southernmost) row, so ``data.ravel()`` is the
    stored cell order (index ``y * nx + x``, Java heclib ``GridData.getValue``) and
    compares directly with P11 oracle dumps. ``order="north_up"`` flips the rows so
    row 0 is the top row, as image and raster tools expect.

    **Missing values.** With ``missing="nan"`` (default) every missing cell is NaN: NaN
    as stored, ``GRID_UNDEFINED`` (-FLT_MAX), -901/-902 and, for specified grids only,
    the grid's ``null_value`` (as HEC Monolith; HRAP/Albers grids Monolith writes to
    DSS7 store 0.0 there, which is data, see :func:`grid_missing_mask`).
    ``missing="raw"`` keeps the decoded values, including those sentinels. Legacy
    grids (DSS6, and DSS7 records with a positive header size) are decoded as Java
    heclib does: legacy ZLIB values at or below -1e38, the specified null marker, and
    HRAP/Albers values outside the stored min/max become ``GRID_UNDEFINED``; read with
    ``preserve_legacy_missing=True`` and ``missing="raw"`` to see the stored bits.
    Modern (-100) grids always keep their stored bits. :meth:`missing_mask` gives the
    mask under the same rule whatever the policy.

    **Geometry.** Cell ``(0, 0)`` of the SRS grid has its lower-left corner at
    ``(x_coord_of_grid_cell_zero, y_coord_of_grid_cell_zero)``; this record starts at
    cell ``(lower_left_cell_x, lower_left_cell_y)``. :attr:`origin` is the lower-left
    corner of the record and :attr:`extent` its outer edges. ``srs_definition`` is the
    WKT (specified grids store it; HRAP and SHG Albers grids get heclib's canonical
    WKT; other legacy Albers grids have their parameters in ``albers`` only).
    Albers grids, legacy and modern, follow HEC Monolith's
    ``AlbersInfo.getSpatialReferenceSystem``: the SHG WKT for NAD83 metre SHG
    parameters with a 0/0 cell-zero origin, otherwise ``""``. Modern (-100) records
    keep the string as stored in ``metadata["stored_srs_definition"]``; for non-Albers
    modern grids it equals ``srs_definition``. Legacy grids have ``None`` there.
    ``metadata["albers"]`` holds the Albers parameters (``None`` for other grid types):
    as stored for legacy grids, and for modern grids parsed from the stored WKT as
    Monolith's ``GridStructContainer.setAlbersSpatialReferenceInfo`` does (P34).

    **Time.** ``start_time``/``end_time`` are naive ``datetime64[s]`` (no time-zone
    conversion) or ``None`` for grids without time (odd record types). DSS7 modern
    grids take them from the D/E parts, legacy grids from the header.

    ``min_value``/``max_value``/``mean_value`` and the range table are as stored.
    ``metadata`` holds every other native field (compression, header sizes, data
    source, Albers parameters, time-zone id/offset, ``start_seconds``/``end_seconds``
    as HEC seconds, ...).
    """

    data: np.ndarray
    pathname: str
    record_type: int
    grid_type: str
    nx: int
    ny: int
    cell_size: float
    lower_left_cell_x: int
    lower_left_cell_y: int
    x_coord_of_grid_cell_zero: float
    y_coord_of_grid_cell_zero: float
    srs_name: str
    srs_definition_type: int
    srs_definition: str
    data_units: str
    data_type: int
    start_time: Optional[np.datetime64]
    end_time: Optional[np.datetime64]
    min_value: Optional[float]
    max_value: Optional[float]
    mean_value: Optional[float]
    range_limit_table: np.ndarray
    range_counts: np.ndarray
    null_value: float
    missing: str
    order: str
    dss_version: int
    metadata: Dict[str, Any] = field(default_factory=dict)
    dss_file: str = ""

    @classmethod
    def _from_native(
        cls, d: Dict[str, Any], *, missing: str, order: str, dss_file: str
    ) -> "Grid":
        _check_grid_args(missing, order)
        d = dict(d)
        data = np.array(d.pop("data"), dtype=np.float32, copy=True)
        null_value = float(d.pop("null_value"))
        if missing == "nan":
            data[grid_missing_mask(data, null_value, d["grid_type"])] = np.nan
        if order == "north_up":
            data = np.ascontiguousarray(data[::-1])

        def when(s):
            return None if s is None else hec_seconds_to_datetime64(np.array([s]))[0]

        # Raw HEC seconds stay available in metadata.
        d["start_seconds"] = d.pop("start_time")
        d["end_seconds"] = d.pop("end_time")

        return cls(
            data=data,
            pathname=d.pop("pathname"),
            record_type=int(d.pop("record_type")),
            grid_type=d.pop("grid_type"),
            nx=int(d.pop("nx")),
            ny=int(d.pop("ny")),
            cell_size=float(d.pop("cell_size")),
            lower_left_cell_x=int(d.pop("lower_left_cell_x")),
            lower_left_cell_y=int(d.pop("lower_left_cell_y")),
            x_coord_of_grid_cell_zero=float(d.pop("x_coord_of_grid_cell_zero")),
            y_coord_of_grid_cell_zero=float(d.pop("y_coord_of_grid_cell_zero")),
            srs_name=d.pop("srs_name"),
            srs_definition_type=int(d.pop("srs_definition_type")),
            srs_definition=d.pop("srs_definition"),
            data_units=d.pop("data_units"),
            data_type=int(d.pop("data_type")),
            start_time=when(d["start_seconds"]),
            end_time=when(d["end_seconds"]),
            min_value=d.pop("min_data_value"),
            max_value=d.pop("max_data_value"),
            mean_value=d.pop("mean_data_value"),
            range_limit_table=np.asarray(d.pop("range_limit_table"), dtype=np.float32),
            range_counts=np.asarray(d.pop("range_counts"), dtype=np.int32),
            null_value=null_value,
            missing=missing,
            order=order,
            dss_version=int(d.pop("version")),
            metadata=d,
            dss_file=dss_file,
        )

    @property
    def shape(self) -> Tuple[int, int]:
        """``(ny, nx)``."""
        return (self.ny, self.nx)

    @property
    def wkt(self) -> str:
        """Alias of ``srs_definition``."""
        return self.srs_definition

    @property
    def data_type_name(self) -> Optional[str]:
        """``PER-AVER``, ``PER-CUM``, ``INST-VAL`` or ``INST-CUM`` (``None`` if unknown)."""
        return _GRID_DATA_TYPES.get(self.data_type)

    @property
    def origin(self) -> Tuple[float, float]:
        """Lower-left corner of the record in SRS units (computed in float64)."""
        xmin, ymin, _, _ = self.extent
        return (xmin, ymin)

    @property
    def extent(self) -> Tuple[float, float, float, float]:
        """``(xmin, ymin, xmax, ymax)``: outer cell edges in SRS units (float64), as
        ``dss_core::Grid::extent``."""
        size = float(np.float32(self.cell_size))
        x = self.x_coord_of_grid_cell_zero + self.lower_left_cell_x * size
        y = self.y_coord_of_grid_cell_zero + self.lower_left_cell_y * size
        return (x, y, x + self.nx * size, y + self.ny * size)

    def missing_mask(self) -> np.ndarray:
        """Boolean ``(ny, nx)`` mask of missing cells (see the class notes)."""
        return grid_missing_mask(self.data, self.null_value, self.grid_type)


def _check_grid_args(missing: str, order: str) -> None:
    if missing not in _GRID_MISSING:
        raise ValueError(f"missing must be one of {_GRID_MISSING}, not {missing!r}")
    if order not in _GRID_ORDERS:
        raise ValueError(f"order must be one of {_GRID_ORDERS}, not {order!r}")


# -- locations (P15; decision D37) ------------------------------------------------------

#: ``zStructLocation.h`` code tables. Codes outside a table, and code 0 (unset), have
#: no name: the name field (``vertical_datum``, ...) is ``None`` and ``*_code`` keeps it.
_COORDINATE_SYSTEMS = {
    1: "LAT_LONG",
    2: "STATE_PLANE_FIPS",
    3: "STATE_PLANE_ADS",
    4: "UTM",
    5: "LOCAL",
}
_HORIZONTAL_UNITS = {1: "FEET", 2: "METERS", 3: "DECIMAL_DEGREES", 4: "DEGREES_MINUTES_SECONDS"}
_HORIZONTAL_DATUMS = {1: "NAD83", 2: "NAD27", 3: "WGS84", 4: "WGS72", 5: "LOCAL"}
_VERTICAL_UNITS = {1: "FEET", 2: "METERS"}
#: ``verticalDatum.h`` ``CVERTICAL_DATUM_*`` names.
_VERTICAL_DATUMS = {1: "NAVD-88", 2: "NGVD-29", 3: "OTHER"}
#: Codes that name a datum (3 is "other/local": typed, but unnamed).
_NAMED_VERTICAL_DATUMS = (1, 2)


def find_supplemental_keyword(text: Optional[str], keyword: str) -> Optional[str]:
    """Item of the first ``keyword:`` anywhere in ``text``, or ``None``.

    Like HEC's ``extractFromDelimitedString()`` (``strstr``/``dss_strcasestr``), the
    keyword is found anywhere, not only at segment starts, so it also works in
    newline-separated text; matching is ASCII case-insensitive. The item runs to the next
    ``;`` (HEC's delimiter) and, unlike HEC, also stops at a line break.
    """
    if not text:
        return None
    m = re.search(re.escape(keyword) + ":", text, flags=re.IGNORECASE | re.ASCII)
    if m is None:
        return None
    return re.split(r"[;\n\r]", text[m.end():], maxsplit=1)[0]


def parse_supplemental(text: Optional[str]) -> List[Tuple[str, str]]:
    """``Keyword:Item;`` pairs of location supplemental text, in order.

    HEC's convention (``zlocationRetrieve.c`` header comment): the keyword ends at the
    first ``:`` and the item at ``;``. Nothing is trimmed; segments without ``:`` (for
    example newline-separated notes) are skipped. Duplicate keywords are all returned.
    """
    if not text:
        return []
    out = []
    for seg in text.split(";"):
        if ":" in seg:
            k, v = seg.split(":", 1)
            out.append((k, v))
    return out


@dataclass(frozen=True, eq=False)
class Location:
    """Location metadata for a DSS record (decision D37 extends D29).

    DSS7 stores it in a location record (type 20) at ``/A/B/Location Info////``, shared
    by every dataset with the same A and B parts (``source="dss7_location_record"``).
    DSS6 has no location records; regular and irregular time-series headers carry the
    coordinates and six descriptor codes (``source="dss6_ts_header"``, ``time_zone`` is
    ``None``).

    **DSS6 supplemental text (P27).** For a DSS6 location, ``supplemental`` is the
    user-header text of the same time-series record (docs/format/dss6.md §14): up to the
    first NUL, trailing blanks removed, then decoded as standard UTF-8, else Java
    modified UTF-8, else UTF-8 with U+FFFD per malformed sequence (P28, P31; the
    decode is the engine's, the same as ``user_header_text``); ``None`` when that text
    is empty. This is Monolith's
    behavior (its ``supplementalInfo`` for a DSS6 time series is the user header); HEC's C library leaves the location's supplemental text empty
    for DSS6. The text is informational only (decision D37): it may be stale or wrong,
    and a datum named in it (for example ``VerticalDatum: NAVD88``) is never applied to
    values and never fills the typed fields; at most it shows up in
    ``supplemental_vertical_datum``. The presence rule does not change: a DSS6 record
    without stored coordinates has no location, even when its user header has text.
    ``supplemental_source`` says where the text came from: ``"dss7_location_record"``
    (DSS7 values2), ``"dss6_user_header"``, or ``None`` exactly when ``supplemental``
    is ``None``. ``supplemental_items`` uses HEC's location convention
    (:func:`parse_supplemental`) for both sources; for Monolith's item rules on DSS6
    text use :func:`parse_user_header_items`.

    Every descriptor has a ``*_code`` (the stored int, always kept) and a name (``None``
    for code 0 and for codes outside HEC's table). The vertical datum is never guessed:
    ``vertical_datum_unknown`` is true for code 0 (unset) and for unrecognized codes,
    and it is **not** NAVD88. ``vertical_datum_named`` is true only for codes naming a
    datum (1 ``NAVD-88``, 2 ``NGVD-29``). Code 3 (``OTHER``, HEC's "other/local") gives
    ``vertical_datum_unknown=False`` *and* ``vertical_datum_named=False``: the record is
    typed as some other or local datum that is not named, so it is not a usable named
    datum. A datum named in the supplemental text is reported separately in
    ``supplemental_vertical_datum`` (the item of the first ``verticalDatum:`` keyword
    anywhere in the text, see :func:`find_supplemental_keyword`) and never replaces the
    typed fields.

    ``x``/``y``/``z`` are the stored doubles (longitude/easting, latitude/northing,
    elevation). ``descriptors_stored`` is false when a DSS7 record holds only the
    coordinates (the codes are then 0, as HEC leaves them). ``time_zone`` is the
    location's time-zone name (``None`` when not stored; not necessarily the data's
    time zone). ``supplemental`` is the decoded text (``supplemental_bytes`` raw, both
    ``None`` when not stored, ``""`` when stored empty) and ``supplemental_items`` its
    ``Keyword:Item;`` pairs (first occurrence wins). For ``source="dss7_location_record"``
    the text is Latin-1 decoded; for ``source="dss6_ts_header"`` it is the user-header
    text decoded as above. ``supplemental_encoding`` (P28) names the decoding:
    ``"utf-8"``, ``"modified-utf-8"`` or ``"utf-8-replaced"`` (always ``"latin-1"`` for DSS7),
    ``None`` exactly when ``supplemental`` is ``None``.
    """

    pathname: str
    source: str
    record_type: int
    dss_version: int
    x: float
    y: float
    z: float
    descriptors_stored: bool
    coordinate_system_code: int
    coordinate_system: Optional[str]
    coordinate_id: int
    horizontal_units_code: int
    horizontal_units: Optional[str]
    horizontal_datum_code: int
    horizontal_datum: Optional[str]
    vertical_units_code: int
    vertical_units: Optional[str]
    vertical_datum_code: int
    vertical_datum: Optional[str]
    vertical_datum_unknown: bool
    vertical_datum_named: bool
    time_zone: Optional[str]
    supplemental: Optional[str]
    supplemental_bytes: Optional[bytes]
    supplemental_items: Dict[str, str]
    supplemental_vertical_datum: Optional[str]
    requested_pathname: str = ""
    program: Optional[str] = None
    last_write: Optional[pd.Timestamp] = None
    values1: Optional[Tuple[int, ...]] = None
    dss_file: str = ""
    #: P27: ``"dss7_location_record"``, ``"dss6_user_header"``, or ``None`` when
    #: ``supplemental`` is ``None``.
    supplemental_source: Optional[str] = None
    #: P28, P31: ``"utf-8"``, ``"modified-utf-8"`` or ``"utf-8-replaced"``, the decoding of
    #: ``supplemental`` (always ``"latin-1"`` for DSS7 location records); ``None`` when
    #: ``supplemental`` is ``None``.
    supplemental_encoding: Optional[str] = None

    @classmethod
    def _from_native(cls, d: Dict[str, Any], *, requested: str, dss_file: str) -> "Location":
        sup_b = d["supplemental"]
        # P28: the text is decoded once, in dss-core (DSS6 user-header text: UTF-8, else
        # modified UTF-8, else UTF-8 with U+FFFD (P31); DSS7: Latin-1). No Python fallback.
        sup = d["supplemental_text"]
        pairs = parse_supplemental(sup)
        items: Dict[str, str] = {}
        for k, v in pairs:
            items.setdefault(k, v)
        svd = find_supplemental_keyword(sup, "verticalDatum")
        vd = int(d["vertical_datum"])
        ms = d.get("last_write_ms")
        return cls(
            pathname=d["pathname"],
            source=d["source"],
            record_type=int(d["record_type"]),
            dss_version=int(d["version"]),
            x=float(d["x"]),
            y=float(d["y"]),
            z=float(d["z"]),
            descriptors_stored=bool(d["descriptors_stored"]),
            coordinate_system_code=int(d["coordinate_system"]),
            coordinate_system=_COORDINATE_SYSTEMS.get(int(d["coordinate_system"])),
            coordinate_id=int(d["coordinate_id"]),
            horizontal_units_code=int(d["horizontal_units"]),
            horizontal_units=_HORIZONTAL_UNITS.get(int(d["horizontal_units"])),
            horizontal_datum_code=int(d["horizontal_datum"]),
            horizontal_datum=_HORIZONTAL_DATUMS.get(int(d["horizontal_datum"])),
            vertical_units_code=int(d["vertical_units"]),
            vertical_units=_VERTICAL_UNITS.get(int(d["vertical_units"])),
            vertical_datum_code=vd,
            vertical_datum=_VERTICAL_DATUMS.get(vd),
            vertical_datum_unknown=vd not in _VERTICAL_DATUMS,
            vertical_datum_named=vd in _NAMED_VERTICAL_DATUMS,
            time_zone=d["time_zone"],
            supplemental=sup,
            supplemental_bytes=None if sup_b is None else bytes(sup_b),
            supplemental_items=items,
            supplemental_vertical_datum=svd,
            requested_pathname=requested,
            program=d.get("program"),
            last_write=None if ms is None else pd.Timestamp(int(ms), unit="ms"),
            values1=None if d.get("values1") is None else tuple(int(v) for v in d["values1"]),
            dss_file=dss_file,
            supplemental_source=None if sup_b is None else d.get("supplemental_source"),
            supplemental_encoding=None if sup_b is None else d.get("supplemental_encoding"),
        )

    @property
    def coordinates(self) -> Tuple[float, float, float]:
        """``(x, y, z)``."""
        return (self.x, self.y, self.z)

    def to_dict(self) -> Dict[str, Any]:
        """Plain dict of every field (for JSON or DataFrame rows)."""
        return {f: getattr(self, f) for f in self.__dataclass_fields__}


__all__ += ["Location", "parse_supplemental", "find_supplemental_keyword"]


# -- profiles (P14) ---------------------------------------------------------------------

#: HEC missing markers for profile values (``zdssVals.h``: -901 missing, -902 not
#: computed, -FLT_MAX UNDEFINED). Same rule as ``ras_commander_fast_dss.missing_mask``.
_PROFILE_MISSING = (-901.0, -902.0, float(-np.finfo(np.float32).max))


def profile_missing_mask(values: np.ndarray) -> np.ndarray:
    """Boolean mask of HEC missing markers (and NaN) in profile ``values``, compared
    in the array's own width (a float32 -FLT_MAX matches)."""
    v = np.asarray(values)
    mask = np.isnan(v)
    for m in _PROFILE_MISSING:
        mask |= v == v.dtype.type(m)
    return mask


@dataclass(frozen=True, eq=False)
class Profile:
    """One regular-interval profile dataset (DSS7 record types 102 RTS-PROFILE and 107
    RTD-PROFILE): at every time, one value per depth.

    **Shape.** ``depths`` is 1-D (``n_depths``, the profile ordinates) and ``values``
    is 2-D shaped ``(n_times, n_depths)``: row ``i`` is the profile at ``times[i]``,
    column ``j`` the series at ``depths[j]``. A profile is never flattened into a
    scalar series.

    **Precision.** ``depths`` and ``values`` keep the stored width: float32 for 102,
    float64 for 107 (``stored_dtype``). With ``raw=False`` (default) HEC missing
    markers (-901, -902, -FLT_MAX) become NaN in that width; ``raw=True`` keeps the
    stored bits. :meth:`missing_mask` applies the marker rule either way.

    **Time.** ``times`` is naive ``datetime64[s]`` (end-of-period stamps; 24:00 is the
    next day's 00:00); ``hec_seconds`` holds the same instants as int64 seconds since
    1899-12-31 00:00 (HEC julian day 0). ``time_zone`` is the stored zone name, never
    applied. ``interval_seconds`` is the E-part interval.

    **Units and types.** ``depth_units`` and ``value_units`` are the two unit strings
    of the DSS7 internal header (``ztsInternalHeaderUnpack``: depth units first, then
    value units). ``value_type`` is the HEC data type (``INST-VAL``, ``PER-AVER``, ...).
    DSS7 stores no type for the depth axis, so ``depth_type`` is ``None`` (unknown),
    never a guessed value.

    ``precision`` is the stored precision word (-1 when not set). ``quality`` is one
    int32 per time (the first quality word) when the record stores quality, else
    ``None``. ``metadata`` keeps the raw native fields: ``internal_header`` words,
    ``user_header``, ``time_granularity_seconds``, ``julian_base_date``,
    ``time_offset_seconds``, ``interval_name``, ``quality_element_size``,
    ``quality_words``, ``blocks_found``, ``blocks_missing``, ``block_pathnames``,
    ``units_padded`` and ``stored_pathname``.
    """

    pathname: str
    record_type: int
    times: np.ndarray
    hec_seconds: np.ndarray
    depths: np.ndarray
    values: np.ndarray
    depth_units: str
    depth_type: Optional[str]
    value_units: str
    value_type: str
    precision: int
    stored_dtype: np.dtype
    time_zone: Optional[str]
    interval_seconds: int
    quality: Optional[np.ndarray]
    raw: bool
    dss_version: int
    metadata: Dict[str, Any] = field(default_factory=dict)
    dss_file: str = ""

    @classmethod
    def _from_native(
        cls, d: Dict[str, Any], *, pathname: str, raw: bool, dss_file: str
    ) -> "Profile":
        d = dict(d)
        secs = np.asarray(d.pop("times"), dtype=np.int64)
        depths = np.array(d.pop("depths"), copy=True)
        values = np.array(d.pop("values"), copy=True)
        if values.ndim != 2 or values.shape != (len(secs), len(depths)):
            raise ValueError(
                f"profile values shaped {values.shape}, expected ({len(secs)}, {len(depths)})"
            )
        stored = values.dtype
        if not raw:
            values[profile_missing_mask(values)] = np.nan
        quality = d.pop("quality")
        if quality is not None:
            quality = np.asarray(quality, dtype=np.int32)
        d["stored_pathname"] = d.pop("pathname")
        return cls(
            pathname=pathname,
            record_type=int(d.pop("record_type")),
            times=hec_seconds_to_datetime64(secs),
            hec_seconds=secs,
            depths=depths,
            values=values,
            depth_units=(d.pop("depth_units") or "").strip(" \0"),
            depth_type=None,
            value_units=(d.pop("value_units") or "").strip(" \0"),
            value_type=(d.pop("data_type") or "").strip(" \0"),
            precision=int(d.pop("precision")),
            stored_dtype=stored,
            time_zone=d.pop("time_zone"),
            interval_seconds=int(d.pop("interval_seconds")),
            quality=quality,
            raw=raw,
            dss_version=int(d.pop("version")),
            metadata=d,
            dss_file=dss_file,
        )

    @property
    def n_times(self) -> int:
        return int(self.values.shape[0])

    @property
    def n_depths(self) -> int:
        return int(self.values.shape[1])

    @property
    def shape(self) -> Tuple[int, int]:
        """``(n_times, n_depths)``."""
        return (self.n_times, self.n_depths)

    def missing_mask(self) -> np.ndarray:
        """Boolean ``(n_times, n_depths)`` mask of missing values (NaN or a marker)."""
        return profile_missing_mask(self.values)

    def at_depth(self, j: int) -> np.ndarray:
        """Values at depth index ``j`` (0-based; negative counts from the end)."""
        n = self.n_depths
        if not -n <= j < n:
            raise IndexError(f"depth index {j} out of range for {n} depth(s)")
        return self.values[:, j]

    def to_dataframe(self) -> pd.DataFrame:
        """Wide frame: one row per time, one column per depth.

        The index is a ``DatetimeIndex`` named ``datetime`` (``datetime64[s]``); the
        columns are the depths (a float ``Index`` named ``depth``, stored width). Values
        keep their stored dtype. ``attrs`` carry ``pathname``, ``depth_units``,
        ``value_units``, ``type`` (the value type), ``time_zone``, ``interval``
        (seconds), ``record_type`` and ``dss_file``.
        """
        index = pd.DatetimeIndex(self.times, name="datetime")
        columns = pd.Index(self.depths, name="depth")
        df = pd.DataFrame(self.values, index=index, columns=columns, copy=True)
        df.attrs.update(
            pathname=self.pathname,
            depth_units=self.depth_units,
            value_units=self.value_units,
            type=self.value_type,
            time_zone=self.time_zone,
            interval=self.interval_seconds,
            record_type=self.record_type,
            dss_file=self.dss_file,
        )
        return df


__all__ += ["Profile", "profile_missing_mask"]


# -- P26: record user-header text -------------------------------------------------------


def parse_user_header_items(text: Optional[str]) -> Dict[str, str]:
    """``{keyword: value}`` items of record user-header text, parsed as Monolith parses
    a container's ``supplementalInfo`` (``hec.io.DataContainer.getSupplementalInfo()``,
    Monolith 3.3.27):

    - the text is split at ``;`` and empty segments are skipped;
    - a segment is an item only when it holds exactly one ``:``; segments without a
      ``:`` (for example ``StreamSample Creek``, as one USGS import wrote it) or with
      several are not items, and HEC's keyed lookups never find them either;
    - trailing blanks are removed from keyword and value, leading blanks are kept
      (``"State: X"`` gives ``{"State": " X"}``);
    - a repeated keyword keeps its last value;
    - an empty keyword or value is kept as ``""`` (``":v"`` gives ``{"": "v"}``,
      ``"K:"`` gives ``{"K": ""}``). Monolith raises ``StringIndexOutOfBoundsException``
      on such text instead; the engine keeps the item.

    The text is free-form and unvalidated; treat every item as informational. A datum
    named here is never a typed location datum (decision D37). Matches the native
    ``user_header_items`` of :meth:`DssFile.read_record_arrays`.
    """
    out: Dict[str, str] = {}
    if not text:
        return out
    for seg in text.split(";"):
        if not seg:
            continue
        parts = seg.split(":")
        if len(parts) != 2:
            continue
        out[parts[0].rstrip(" ")] = parts[1].rstrip(" ")
    return out
