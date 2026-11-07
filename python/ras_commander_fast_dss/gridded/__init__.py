"""Gridded import: GRIB2, NetCDF, HDF5, GeoTIFF and ASCII grid files, or arrays, to DSS grids, in the engine's process, with no Java.

    from ras_commander_fast_dss import gridded

    result = gridded.import_gridded(
        ["MRMS_MultiSensor_QPE_01H_Pass2_00.00_20240926-220000.grib2"],
        "storm.dss",
        product="mrms_qpe",
        envelope=(-646000, 1922000, -444000, 2124000),
        parts={"A": "SHG", "B": "MRMS_QPE", "F": "PASS2"},
    )

The result reproduces HEC-Vortex 1.1.0's records on the cases in the P46 evidence report, except
for the observed Vortex behaviours listed in :func:`import_gridded`, which this library refuses by
default. See ``evidence/P46/REPORT.md``.
"""

from ._import import ImportResult, StopHandle, import_arrays, import_gridded, list_variables, netcdf_limits
from ._temporal import (
    TimeToolResult,
    WindowGrid,
    read_min_max,
    read_nearest,
    read_window,
    resample_times,
    series_info,
    shift_times,
    time_steps,
)
from ._tools import NormalizeResult, calculate, calculate_raster, clip, normalize, sanitize
from ._zonal import STATISTICS, GridToPointResult, grid_to_point, zonal_statistics, zone_masks

__all__ = [
    "ImportResult",
    "TimeToolResult",
    "WindowGrid",
    "import_arrays",
    "import_gridded",
    "list_variables",
    "read_min_max",
    "read_nearest",
    "read_window",
    "resample_times",
    "series_info",
    "shift_times",
    "time_steps",
]
__all__ += ["NormalizeResult", "calculate", "calculate_raster", "clip", "normalize", "sanitize"]
__all__ += ["GridToPointResult", "STATISTICS", "grid_to_point", "zonal_statistics", "zone_masks"]
__all__ += ["StopHandle", "netcdf_limits"]

# P49: a local Zarr v2 store, the route for AORC v1.1.
from ._zarr import ZarrStop, ZarrWindow, import_zarr, read_zarr, zarr_limits, zarr_variables  # noqa: E402

__all__ += ["ZarrStop", "ZarrWindow", "import_zarr", "read_zarr", "zarr_limits", "zarr_variables"]
