"""Type stubs for the Rust extension module (crates/dss-py)."""

from typing import Any, Dict, List, Optional, Tuple

def file_signature(path: str) -> Dict[str, Any]: ...
def open(path: str, recover: bool = False) -> NativeFile: ...

class NativeFile:
    @property
    def path(self) -> str: ...
    @property
    def version(self) -> int: ...
    @property
    def version_string(self) -> str: ...
    @property
    def recover(self) -> bool: ...
    @property
    def closed(self) -> bool: ...
    def signature(self) -> Dict[str, Any]: ...
    def close(self) -> None: ...
    # P62: live_only exports only live/alias rows (no stored_pathname/raw_status keys).
    def catalog(self, recover: Optional[bool] = None, live_only: bool = False) -> Dict[str, Any]: ...
    def pathnames(self, recover: Optional[bool] = None) -> List[str]: ...
    # P29: refuse_no_data raises DssNoDataError for HEC DSS6 "no data" (status 4).
    def read_record(self, pathname: str, refuse_no_data: bool = False) -> Dict[str, Any]: ...
    def read_dataset(
        self,
        pathname: str,
        start: Optional[int] = None,
        end: Optional[int] = None,
        trim_missing: bool = True,
        refuse_no_data: bool = False,
    ) -> Dict[str, Any]: ...
    def read_paired_data(self, pathname: str) -> Dict[str, Any]: ...
    def read_paired_data_at(self, index: int) -> Dict[str, Any]: ...
    def read_grid(
        self,
        pathname: str,
        max_cells: Optional[int] = None,
        preserve_legacy_missing: bool = False,
    ) -> Dict[str, Any]: ...
    def read_grid_at(
        self,
        index: int,
        max_cells: Optional[int] = None,
        preserve_legacy_missing: bool = False,
    ) -> Dict[str, Any]: ...
    # P14: DSS7 regular profiles (102/107).
    def read_profile(
        self,
        pathname: str,
        start: Optional[int] = None,
        end: Optional[int] = None,
        trim_missing: bool = True,
    ) -> Dict[str, Any]: ...
    def read_profile_record(self, pathname: str) -> Dict[str, Any]: ...


# Location records: module-level functions taking a NativeFile.
from typing import List, TypedDict

class LocationDict(TypedDict):
    version: int
    pathname: str
    record_type: int
    source: str  # "dss7_location_record" | "dss6_ts_header"
    x: float
    y: float
    z: float
    descriptors_stored: bool
    coordinate_system: int
    coordinate_id: int
    horizontal_units: int
    horizontal_datum: int
    vertical_units: int
    vertical_datum: int
    time_zone: Optional[str]
    supplemental: Optional[bytes]
    values1: Optional[List[int]]
    program: Optional[str]
    last_write_ms: Optional[int]
    # P27: "dss7_location_record" | "dss6_user_header" | None (no supplemental text);
    # DSS6 block date (julian) from the D part, None for DSS7.
    supplemental_source: Optional[str]
    block_julian: Optional[int]
    # P28: decoded supplemental text and its decoding ("utf-8" | "modified-utf-8" |
    # "utf-8-replaced"; DSS7: "latin-1"); None when supplemental is None.
    supplemental_text: Optional[str]
    supplemental_encoding: Optional[str]

def location_read(file: NativeFile, pathname: str) -> LocationDict: ...
def location_read_associated(file: NativeFile, pathname: str) -> Optional[LocationDict]: ...

# P16: catalog-index time-series route.
def ts_entry_provenance(file: NativeFile, index: int) -> Dict[str, Any]: ...
def read_record_at(file: NativeFile, index: int) -> Tuple[Dict[str, Any], Dict[str, Any]]: ...

# P19a: lock detection.
def lock_status(path: str) -> Dict[str, Any]: ...

# P25: the time-series dicts of read_record / read_dataset also carry the pattern
# time-origin keys, all None for non-pattern records:
#   pattern_time_convention: Optional[str]  (first-value-at-3000-01-01T00:00)
#   pattern_stored_offset_seconds: Optional[int]  (raw, not applied)
#   pattern_stored_offset_source: Optional[str]
#   ("dss6_internal_header_word0_minutes" | "dss7_int_head_time_offset")
#   pattern_heclib_first_seconds: Optional[int]  (heclib ztsRetrievePattern start)

# P20: DSS7 regular time-series writer.
def create_dss7(path: str) -> None: ...
def repair_dss7(src: str, dest: str) -> dict[str, Any]: ...
def write_rts(
    path: str,
    pathname: str,
    start: int,
    values: Any,
    units: str,
    data_type: str,
    quality: Optional[Any] = None,
    time_zone: Optional[str] = None,
    precision: int = -1,
    allow_undetectable_readers: bool = True,
    sync: bool = True,
) -> Dict[str, Any]: ...
def rts_value_times(pathname: str, start: int, n: int) -> Any: ...

# P21: DSS7 irregular time-series writer.
def write_its(
    path: str,
    pathname: str,
    times: Any,
    values: Any,
    units: str,
    data_type: str,
    quality: Optional[Any] = None,
    time_zone: Optional[str] = None,
    precision: int = -1,
    time_granularity: Optional[int] = None,
    allow_undetectable_readers: bool = True,
    sync: bool = True,
) -> Dict[str, Any]: ...

# P22: DSS6 time-series writer.
def create_dss6(path: str) -> None: ...
def write_rts6(
    path: str,
    pathname: str,
    start: int,
    values: Any,
    units: str,
    data_type: str,
    quality: Optional[Any] = None,
    time_zone: Optional[str] = None,
    precision: int = -1,
    allow_undetectable_readers: bool = True,
    sync: bool = True,
) -> Dict[str, Any]: ...
def rts6_value_times(pathname: str, start: int, n: int) -> Any: ...
def write_rts_variants(
    path: str, pathname: str, start: int, values: Any, units: str, data_type: str,
    quality: Any = ..., quality_element_size: int = ..., inotes: Any = ...,
    inote_element_size: int = ..., cnotes: Any = ..., time_zone: Any = ..., precision: int = ...,
    allow_undetectable_readers: bool = ..., sync: bool = ...,
) -> Dict[str, Any]: ...
def write_its_variants(
    path: str, pathname: str, times: Any, values: Any, units: str, data_type: str,
    quality: Any = ..., quality_element_size: int = ..., inotes: Any = ...,
    inote_element_size: int = ..., cnotes: Any = ..., time_zone: Any = ..., precision: int = ...,
    time_granularity: Any = ..., allow_undetectable_readers: bool = ..., sync: bool = ...,
) -> Dict[str, Any]: ...
def write_pattern(
    path: str, pathname: str, values: Any, units: str, data_type: str, times: Any = ...,
    time_zone: Any = ..., precision: int = ..., time_granularity: Any = ..., version: int = ...,
    allow_undetectable_readers: bool = ..., sync: bool = ...,
) -> Dict[str, Any]: ...
def write_its6(
    path: str,
    pathname: str,
    times: Any,
    values: Any,
    units: str,
    data_type: str,
    quality: Optional[Any] = None,
    mode: str = "replace",
    allow_undetectable_readers: bool = True,
    sync: bool = True,
) -> Dict[str, Any]: ...

# P23: paired-data writer (DSS7 and DSS6).
def write_pd(
    path: str,
    pathname: str,
    ordinates: Any,
    values: Any,
    number_curves: int,
    units_independent: str,
    type_independent: str,
    units_dependent: str,
    type_dependent: str,
    labels: Optional[List[str]] = None,
    independent_is_x_axis: int = 1,
    x_precision: int = -1,
    y_precision: int = -1,
    time_zone: Optional[str] = None,
    version: int = 7,
    allow_undetectable_readers: bool = True,
    sync: bool = True,
) -> Dict[str, Any]: ...

# P24: grid writer (DSS7 and DSS6).
def write_grid(
    path: str,
    pathname: str,
    data: Any,
    nx: int,
    ny: int,
    grid_type: str,
    time_stamped: bool,
    lower_left_cell_x: int,
    lower_left_cell_y: int,
    cell_size: float,
    x0: float,
    y0: float,
    units: str,
    data_type: int,
    compression: str,
    scale_factor: Optional[float] = None,
    base: Optional[float] = None,
    data_source: str = "",
    albers: Optional[Tuple[int, str, float, float, float, float, float, float]] = None,
    srs_name: str = "",
    srs_definition: str = "",
    srs_definition_type: int = 0,
    null_value: float = ...,
    time_zone_id: str = "UTC",
    time_zone_raw_offset: int = 0,
    is_interval: bool = True,
    version: int = 7,
    allow_undetectable_readers: bool = True,
    sync: bool = True,
    validate_only: bool = False,
) -> Dict[str, Any]: ...


# P39: profile and location writers.
def write_profile(
    path: str,
    pathname: str,
    start: int,
    depths: Any,
    values: Any,
    depth_units: str,
    value_units: str,
    data_type: str,
    quality: Any = None,
    time_zone: Optional[str] = None,
    precision: int = -1,
    allow_undetectable_readers: bool = True,
    sync: bool = True,
) -> Dict[str, Any]: ...

def write_location(
    path: str,
    pathname: str,
    x: Optional[float] = None,
    y: Optional[float] = None,
    z: Optional[float] = None,
    coordinate_system: Optional[int] = None,
    coordinate_id: Optional[int] = None,
    horizontal_units: Optional[int] = None,
    horizontal_datum: Optional[int] = None,
    vertical_units: Optional[int] = None,
    vertical_datum: Optional[int] = None,
    time_zone: Optional[bytes] = None,
    supplemental: Optional[bytes] = None,
    keep_existing: bool = True,
    version: int = 7,
    allow_undetectable_readers: bool = True,
    sync: bool = True,
) -> Dict[str, Any]: ...

# P52: time tools, behind the `vortex` feature. Public API: `ras_commander_fast_dss.gridded`.
def shift_grid_times(input: str, output: str, shift_start: int, shift_end: int, **kwargs: Any) -> Dict[str, Any]: ...
def resample_grid_times(input: str, output: str, step_minutes: int, rule: str = "strict", **kwargs: Any) -> Dict[str, Any]: ...
def read_grid_window(input: str, pathname: str, start: str, end: str, rule: str = "strict") -> Dict[str, Any]: ...
def read_grid_nearest(input: str, pathname: str, time: str) -> Dict[str, Any]: ...
def read_grid_min_max(input: str, pathname: str, start: str, end: str) -> Any: ...
def grid_series_info(input: str, pathname: str) -> Dict[str, Any]: ...
def grid_time_steps() -> List[int]: ...
# P53: grid tools, behind the `vortex` feature. Public API: `ras_commander_fast_dss.gridded`.
def grid_calculate(input: str, output: str, operation: str, value: float, **kwargs: Any) -> List[Dict[str, Any]]: ...
def grid_calculate_raster(input: str, raster: str, output: str, operation: str, **kwargs: Any) -> List[Dict[str, Any]]: ...
def grid_sanitize(input: str, output: str, **kwargs: Any) -> List[Dict[str, Any]]: ...
def grid_clip(input: str, output: str, **kwargs: Any) -> List[Dict[str, Any]]: ...
def grid_normalize(input: str, normals: str, output: str, start: int, end: int, interval_seconds: int, **kwargs: Any) -> Dict[str, Any]: ...

# P46b: NetCDF through the importer, behind the `vortex` feature. Public API: `ras_commander_fast_dss.gridded`.
class GriddedStop:
    def __init__(self) -> None: ...
    def request(self) -> None: ...
    def clear(self) -> None: ...
    @property
    def requested(self) -> bool: ...
# P49: a local Zarr v2 store, behind the `vortex` feature. Public API: `ras_commander_fast_dss.gridded`.
class ZarrStop:
    def __init__(self) -> None: ...
    def set(self) -> None: ...
    def clear(self) -> None: ...
    def is_set(self) -> bool: ...

def zarr_limits() -> Dict[str, Any]: ...
def zarr_variables(path: str) -> List[Dict[str, Any]]: ...
def read_zarr(path: str, variable: str, first_step: int, last_step: int, **kwargs: Any) -> Dict[str, Any]: ...
def import_zarr(inputs: List[str], output: str, variable: str, **kwargs: Any) -> List[Dict[str, Any]]: ...
def gridded_cell_size(target_crs: str, metres: float, align: str = ...) -> tuple[float, float]: ...
