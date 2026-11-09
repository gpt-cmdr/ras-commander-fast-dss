//! A DSS file as a [`GridSource`]: one variable per grid record, named by its pathname.
//!
//! Sources, Vortex 1.1.0: `io/DssDataReader.getVariables`, `dssToDto`. A frame reads as Vortex's
//! does: north up, `dy = -cell`, marked with HEC's undefined value.
//!
//! A cell this library's reader takes as missing (`Grid::is_missing`: -901, -902, or a specified
//! grid's null value) is HEC's marker in the frame. Observed behaviour we did not implement: Vortex
//! computes on those as numbers.
//!
//! HRAP and Albers grids other than SHG keep the CRS text Vortex gives them through GDAL 3.2.1
//! (compared with Vortex 1.1.0 output). An HRAP grid's cell numbers count from the CRS's
//! own false origin (HRAP point 401, 1601 is the pole), so it is not [`crate::crs::CrsDef::hrap`],
//! whose origin is the pole, and its records become specified grids as they do in Vortex.
//!
//! Refused rather than guessed: a grid with no CRS, an undefined grid, an Albers grid in another
//! unit or with no datum (see [`albers_wkt`]), and a cell zero away from the CRS's origin.

use crate::crs::wkt::{albers_angle_text, albers_metres_text};
use crate::error::VortexError;
use crate::frame::{
    Crs, DataType, FrameTime, GeoTransform, GridFrame, GridFrameParts, NamedCrs, RowOrder,
};
use crate::import::{GridSource, ImportLimits, SourceGridDef, StepMeta};
use dss_core::dss6::{Dss6File, FileSource};
use dss_core::dss7::Dss7File;
use dss_core::grid::{Grid, GridReadOptions, GridType};
use dss_core::grid_write::{parse_grid_time, GRID_NODATA};
use std::fs::File;
use std::io::Read;
use std::path::Path;
use std::sync::Mutex;

enum Inner {
    V7(Box<Dss7File<File>>),
    V6(Box<Dss6File<FileSource>>),
}

struct Entry {
    pathname: Vec<u8>,
    name: String,
}

/// An opened DSS file.
pub struct DssSource {
    file_name: String,
    inner: Mutex<Inner>,
    entries: Vec<Entry>,
    options: GridReadOptions,
}

fn refused(what: impl Into<String>, reason: impl Into<String>) -> VortexError {
    VortexError::Refused {
        what: what.into(),
        reason: reason.into(),
    }
}

fn decode(message: impl std::fmt::Display) -> VortexError {
    VortexError::Decode {
        format: "DSS",
        message: message.to_string(),
    }
}

/// Whether a file's first bytes are a DSS header, of either version.
pub fn is_dss(path: &Path) -> Result<bool, VortexError> {
    let mut head = vec![0u8; dss_core::header::MIN_HEADER_BYTES];
    let mut file = File::open(path)?;
    let mut n = 0;
    loop {
        let k = file.read(&mut head[n..])?;
        n += k;
        if k == 0 || n == head.len() {
            break;
        }
    }
    head.truncate(n);
    Ok(dss_core::FileSignature::from_bytes(&head).is_ok())
}

impl DssSource {
    /// Open a file and list its live grid records. A damaged catalog is refused, rather than taken
    /// to hold only what could be read.
    pub fn open(path: &Path) -> Result<Self, VortexError> {
        let file_name = path
            .file_name()
            .map_or(String::new(), |n| n.to_string_lossy().into_owned());
        let mut head = vec![0u8; dss_core::header::MIN_HEADER_BYTES];
        let mut file = File::open(path)?;
        let mut n = 0;
        loop {
            let k = file.read(&mut head[n..])?;
            n += k;
            if k == 0 || n == head.len() {
                break;
            }
        }
        head.truncate(n);
        drop(file);
        let signature = dss_core::FileSignature::from_bytes(&head).map_err(decode)?;
        let options = GridReadOptions {
            max_cells: ImportLimits::default().max_source_cells,
            ..GridReadOptions::default()
        };
        let (inner, entries, anomalies) = match signature.version {
            dss_core::DssVersion::V7 => {
                let mut file = Dss7File::open(path).map_err(decode)?;
                let catalog = file.catalog().map_err(decode)?.catalog;
                let entries = grid_entries(catalog.entries.iter().filter(|e| e.is_live()));
                let anomalies = catalog.anomalies.len();
                (Inner::V7(Box::new(file)), entries, anomalies)
            }
            dss_core::DssVersion::V6 => {
                let file = Dss6File::open(path).map_err(decode)?;
                let catalog = file.catalog().map_err(decode)?;
                let entries = grid_entries(catalog.entries.iter().filter(|e| e.is_live()));
                let anomalies = catalog.anomalies.len();
                (Inner::V6(Box::new(file)), entries, anomalies)
            }
        };
        if anomalies > 0 {
            return Err(refused(
                format!("catalog of {}", path.display()),
                format!("{anomalies} anomaly(ies), so it may not list every record"),
            ));
        }
        Ok(DssSource {
            file_name,
            inner: Mutex::new(inner),
            entries,
            options,
        })
    }

    /// The grid pathnames, in the file's catalog order, which is the order
    /// `DssDataReader.getVariables` collects them in.
    pub fn pathnames(&self) -> Vec<String> {
        self.entries.iter().map(|e| e.name.clone()).collect()
    }

    fn read(&self, variable: &str) -> Result<Grid, VortexError> {
        let entry = self
            .entries
            .iter()
            .find(|e| e.name == variable)
            .ok_or_else(|| {
                refused(
                    format!("variable {variable}"),
                    format!("{} holds no such grid", self.file_name),
                )
            })?;
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| refused("DSS source", "a previous read panicked"))?;
        match &mut *inner {
            Inner::V7(file) => file
                .read_grid_with(&entry.pathname, &self.options)
                .map_err(decode),
            Inner::V6(file) => file
                .read_grid_with(&entry.pathname, &self.options)
                .map_err(decode),
        }
    }
}

fn grid_entries<'a>(live: impl Iterator<Item = &'a dss_core::CatalogEntry>) -> Vec<Entry> {
    live.filter(|e| GridType::from_record_type(e.record_type).is_ok())
        .map(|e| Entry {
            pathname: e.pathname.clone(),
            name: String::from_utf8_lossy(&e.pathname).into_owned(),
        })
        .collect()
}

// `ReferenceUtils.isShg`, from stored values, as there.
fn is_shg(grid: &Grid) -> bool {
    grid.grid_type == GridType::Albers
        && grid.albers.as_ref().is_some_and(|a| {
            a.datum == NAD_83
                && a.first_standard_parallel == 29.5
                && a.second_standard_parallel == 45.5
                && a.latitude_of_projection_origin == 23.0
                && a.central_meridian == -96.0
                && a.false_easting == 0.0
                && a.false_northing == 0.0
        })
}

/// The cell size and origin keep their numbers across a tool, so a CRS may only say metres if the record
/// does. `AlbersInfo.isMeters` (`dss_core`'s `is_meters`): lower-cased, then one of these exactly. Checked
/// before SHG is recognised, as `is_shg` follows `ReferenceUtils.isShg`, which does not read the units.
pub(crate) fn albers_units_stated_in_metres(
    a: &dss_core::grid::AlbersProjection,
    what: &str,
) -> Result<(), VortexError> {
    let units = a.units.to_lowercase();
    if ["meters", "meter", "m", "metres", "metre"].contains(&units.as_str()) {
        return Ok(());
    }
    let reason = if units.contains("foot") || units.contains("feet") || units.contains("us") {
        format!(
            "an Albers grid in {:?} is in feet, and this library's CRSs are in metres",
            a.units
        )
    } else {
        format!(
            "an Albers grid with units {:?} does not state metres, and this library's CRSs are in metres",
            a.units
        )
    };
    Err(refused(what, reason))
}

// `hec.heclib.grid.GridInfo`: NAD_83 and NAD_27.
const NAD_83: i32 = 2;
const NAD_27: i32 = 1;

// `HrapInfo.getSpatialReferenceSystem` (`zStructSpatialGrid.h`, HRAP_SRC_DEFINITION) through
// `new SpatialReference(wkt).ExportToWkt()` in `WktFactory.fromGridInfo`: what Vortex 1.1.0 and 0.13.3
// both store. Read from their records (`calc.hrap`), and compared byte for byte there.
const HRAP_AS_VORTEX: &str = "PROJCS[\"Stereographic_CONUS_HRAP\",GEOGCS[\"GCS_Sphere_LFM\",DATUM[\"D_Sphere_LFM\",SPHEROID[\"Shpere_LFM\",6371200,0]],PRIMEM[\"Greenwich\",0],UNIT[\"Degree\",0.0174532925199433]],PROJECTION[\"Polar_Stereographic\"],PARAMETER[\"latitude_of_origin\",60],PARAMETER[\"central_meridian\",-105],PARAMETER[\"false_easting\",1909762.5],PARAMETER[\"false_northing\",7624762.5],UNIT[\"metre\",1,AUTHORITY[\"EPSG\",\"9001\"]],AXIS[\"Easting\",SOUTH],AXIS[\"Northing\",SOUTH]]";

// GDAL 3.2.1's `SetWellKnownGeogCS`, as Vortex's records hold it (`calc.albers_*`).
const NAD83_AS_GDAL: &str = "GEOGCS[\"NAD83\",DATUM[\"North_American_Datum_1983\",SPHEROID[\"GRS 1980\",6378137,298.257222101,AUTHORITY[\"EPSG\",\"7019\"]],AUTHORITY[\"EPSG\",\"6269\"]],PRIMEM[\"Greenwich\",0,AUTHORITY[\"EPSG\",\"8901\"]],UNIT[\"degree\",0.0174532925199433,AUTHORITY[\"EPSG\",\"9122\"]],AUTHORITY[\"EPSG\",\"4269\"]]";
const NAD27_AS_GDAL: &str = "GEOGCS[\"NAD27\",DATUM[\"North_American_Datum_1927\",SPHEROID[\"Clarke 1866\",6378206.4,294.978698213898,AUTHORITY[\"EPSG\",\"7008\"]],AUTHORITY[\"EPSG\",\"6267\"]],PRIMEM[\"Greenwich\",0,AUTHORITY[\"EPSG\",\"8901\"]],UNIT[\"degree\",0.0174532925199433,AUTHORITY[\"EPSG\",\"9122\"]],AUTHORITY[\"EPSG\",\"4267\"]]";

/// `WktFactory.fromGridInfo` on an `AlbersInfo`: `SetWellKnownGeogCS`, `SetACEA`, `SetLinearUnits`,
/// `ExportToWkt`. The parameters are the record's floats, worded by [`albers_angle_text`] and
/// [`albers_metres_text`]. A datum other than NAD83 and NAD27 sets no `GEOGCS`, and GDAL then writes WGS 84,
/// so it is refused. Units are checked first, by [`albers_units_stated_in_metres`].
fn albers_wkt(a: &dss_core::grid::AlbersProjection, what: &str) -> Result<String, VortexError> {
    let geogcs = match a.datum {
        NAD_83 => NAD83_AS_GDAL,
        NAD_27 => NAD27_AS_GDAL,
        other => {
            return Err(refused(
                what,
                format!(
                    "Albers datum code {other} states no datum: a CRS is never guessed. Observed \
                     behaviour we did not implement: Vortex 1.1.0 and 0.13.3 store WGS 84"
                ),
            ))
        }
    };
    Ok(format!(
        "PROJCS[\"unnamed\",{geogcs},PROJECTION[\"Albers_Conic_Equal_Area\"],\
         PARAMETER[\"latitude_of_center\",{}],PARAMETER[\"longitude_of_center\",{}],\
         PARAMETER[\"standard_parallel_1\",{}],PARAMETER[\"standard_parallel_2\",{}],\
         PARAMETER[\"false_easting\",{}],PARAMETER[\"false_northing\",{}],UNIT[\"Meter\",1],\
         AXIS[\"Easting\",EAST],AXIS[\"Northing\",NORTH]]",
        albers_angle_text(a.latitude_of_projection_origin),
        albers_angle_text(a.central_meridian),
        albers_angle_text(a.first_standard_parallel),
        albers_angle_text(a.second_standard_parallel),
        albers_metres_text(a.false_easting),
        albers_metres_text(a.false_northing),
    ))
}

// `WktFactory.fromGridInfo`. Every tool that reads a CRS from a DSS record comes through here.
pub(crate) fn crs_of(grid: &Grid, name: &str) -> Result<Crs, VortexError> {
    if let (GridType::Albers, Some(a)) = (grid.grid_type, &grid.albers) {
        albers_units_stated_in_metres(a, &format!("CRS of {name}"))?;
    }
    if is_shg(grid) {
        return Ok(Crs::Named(NamedCrs::Shg));
    }
    let what = format!("CRS of {name}");
    match grid.grid_type {
        GridType::Specified if !grid.srs_definition.trim().is_empty() => {
            Ok(Crs::Wkt(grid.srs_definition.clone()))
        }
        GridType::Specified => Err(refused(what, "the record states none")),
        GridType::Albers => match &grid.albers {
            Some(a) => Ok(Crs::Wkt(albers_wkt(a, &what)?)),
            None => Err(refused(what, "the record states no Albers parameters")),
        },
        GridType::Hrap => Ok(Crs::Wkt(HRAP_AS_VORTEX.to_string())),
        GridType::Undefined => Err(refused(what, "an undefined grid states no CRS")),
    }
}

// `dssToDto`. The corner is an int times a float, which is float arithmetic in Java, widened after.
// `uly` adds a float product to a double. Both are as Vortex computes them, since a clip depends on them.
pub(crate) fn transform_of(grid: &Grid) -> GeoTransform {
    let cell = grid.cell_size;
    let ulx = f64::from(grid.lower_left_cell_x as f32 * cell);
    let lly = f64::from(grid.lower_left_cell_y as f32 * cell);
    let uly = lly + f64::from(grid.ny as i32 as f32 * cell);
    GeoTransform::axis_aligned(ulx, f64::from(cell), uly, -f64::from(cell))
}

/// The C part of a pathname, which `dssToDto` uses as the short name and description.
pub fn c_part(pathname: &str) -> String {
    pathname.split('/').nth(3).unwrap_or_default().to_string()
}

/// The F part of a pathname.
pub fn f_part(pathname: &str) -> String {
    pathname.split('/').nth(6).unwrap_or_default().to_string()
}

// `dssToDto`. `getEndTime` returns null for a blank or unreadable E part, and the end becomes the
// start; an unreadable D part throws there and is refused here.
fn time_of(pathname: &str) -> Result<FrameTime, VortexError> {
    let parts: Vec<&str> = pathname.split('/').collect();
    let d = parts.get(4).copied().unwrap_or_default().trim();
    let e = parts.get(5).copied().unwrap_or_default().trim();
    if d.is_empty() {
        return Ok(FrameTime::Invariant);
    }
    let start = parse_grid_time(d.as_bytes())
        .ok_or_else(|| refused(format!("D part of {pathname}"), "it is not a time"))?;
    match parse_grid_time(e.as_bytes()) {
        Some(end) if end != start => Ok(FrameTime::Period { start, end }),
        _ => Ok(FrameTime::Instant(start)),
    }
}

fn stored_type(code: i32) -> Option<DataType> {
    [
        DataType::PerAver,
        DataType::PerCum,
        DataType::InstVal,
        DataType::InstCum,
    ]
    .into_iter()
    .find(|t| t.dss_code() == code)
}

// The frame model ties a type to its time; a stored type that disagrees is reported by `step_meta`.
fn frame_type(stored: Option<DataType>, time: &FrameTime) -> DataType {
    match (time, stored) {
        (FrameTime::Period { .. }, Some(t @ (DataType::PerAver | DataType::PerCum))) => t,
        (FrameTime::Period { .. }, _) => DataType::PerAver,
        (_, Some(t @ (DataType::InstVal | DataType::InstCum))) => t,
        (_, _) => DataType::InstVal,
    }
}

/// The record's own definition of where cell numbers count from (`zStructSpatialGrid.h`).
pub(crate) fn cell_zero_is_the_crs_origin(grid: &Grid, name: &str) -> Result<(), VortexError> {
    if grid.x_coord_of_grid_cell_zero != 0.0 || grid.y_coord_of_grid_cell_zero != 0.0 {
        return Err(refused(
            format!("grid {name}"),
            format!(
                "its cell zero is at ({}, {}), not at the CRS's origin. Observed behaviour we did not \
                 implement: `DssDataReader.dssToDto` ignores these coordinates and writes 0, moving \
                 the grid with no message",
                grid.x_coord_of_grid_cell_zero, grid.y_coord_of_grid_cell_zero
            ),
        ));
    }
    Ok(())
}

fn def_of(grid: &Grid, name: &str) -> Result<SourceGridDef, VortexError> {
    cell_zero_is_the_crs_origin(grid, name)?;
    Ok(SourceGridDef {
        nx: grid.nx,
        ny: grid.ny,
        transform: transform_of(grid),
        crs: crs_of(grid, name)?,
        nodata: Some(GRID_NODATA),
        stored_wkt: None,
    })
}

impl GridSource for DssSource {
    fn file_name(&self) -> &str {
        &self.file_name
    }

    fn variables(&self) -> Vec<String> {
        self.pathnames()
    }

    fn step_count(&self, variable: &str) -> Result<usize, VortexError> {
        if self.entries.iter().any(|e| e.name == variable) {
            Ok(1)
        } else {
            Err(refused(
                format!("variable {variable}"),
                format!("{} holds no such grid", self.file_name),
            ))
        }
    }

    fn step_meta(&self, variable: &str, _step: usize) -> Result<StepMeta, VortexError> {
        let grid = self.read(variable)?;
        Ok(StepMeta {
            time: time_of(variable)?,
            units: grid.data_units.clone(),
            description: Some(c_part(variable)),
            source_type: stored_type(grid.data_type).ok_or_else(|| {
                refused(
                    format!("data type of {variable}"),
                    format!("{} is not one of HEC's four", grid.data_type),
                )
            })?,
        })
    }

    fn grid(&self, variable: &str, _step: usize) -> Result<SourceGridDef, VortexError> {
        def_of(&self.read(variable)?, variable)
    }

    fn frame(&self, variable: &str, _step: usize) -> Result<GridFrame, VortexError> {
        let grid = self.read(variable)?;
        let def = def_of(&grid, variable)?;
        let time = time_of(variable)?;
        let (nx, ny) = (grid.nx, grid.ny);
        if nx.checked_mul(ny) != Some(grid.data.len()) {
            return Err(refused(
                format!("cells of {variable}"),
                format!("{} cells for {nx} x {ny}", grid.data.len()),
            ));
        }
        // Decided on the stored values, by the reader's own rule, before the cells move out of it.
        let holes: Vec<usize> = (0..grid.data.len())
            .filter(|i| grid.is_missing(grid.data[*i]))
            .collect();
        let units = grid.data_units.clone();
        let data_type = frame_type(stored_type(grid.data_type), &time);
        let mut cells = grid.data;
        for i in holes {
            cells[i] = GRID_NODATA;
        }
        // `RasterUtils.flipVertically`: DSS stores rows south to north.
        for row in 0..ny / 2 {
            let (low, high) = cells.split_at_mut((ny - 1 - row) * nx);
            low[row * nx..(row + 1) * nx].swap_with_slice(&mut high[..nx]);
        }
        Ok(GridFrame::new(GridFrameParts {
            cells,
            nx,
            ny,
            row_order: RowOrder::NorthToSouth,
            transform: def.transform,
            crs: def.crs,
            nodata: Some(GRID_NODATA),
            data_type,
            time,
            units,
            variable: variable.to_string(),
            description: Some(c_part(variable)),
        })?)
    }
}
