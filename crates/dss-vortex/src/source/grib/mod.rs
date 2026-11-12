//! GRIB2 source: bytes to frames, with Vortex 1.1.0's own decoded array as the reference.
//!
//! Layers, each with its source named in its file:
//! - [`scan`]: messages and sections.
//! - [`unpack`]: section 7 to `f32`, in `Grib2DataReader`'s arithmetic.
//! - [`gds`]: section 3 to the georeference.
//! - [`pds`]: sections 1 and 4.
//! - [`crate::products`]: names, units, missing values and time windows.
//!
//! The value formula is NetCDF-Java's. The `grib` crate computes `(R + X * 2^E) * 10^-D`, which
//! rounds differently from `/ 10^D`, so it is not used (removed from the dependency graph).

pub mod gds;
mod georef_maths;
mod jpeg2000_budget;
mod numbers;
pub mod pds;
pub mod scan;
mod unpack;

use crate::error::VortexError;
use crate::products::{self, RecordIdentity, WindowRule};
use crate::{Crs, FrameTime, GeoTransform, GridFrame, GridFrameParts, RowOrder};
pub use gds::Georeference;
#[doc(hidden)]
pub use jpeg2000_budget::required_bytes as jpeg2000_required_bytes;
pub(crate) use numbers::{nearly_equals_f64, wkt_number};
pub use pds::Product;
pub use scan::RecordSpan;
use std::path::Path;

/// Bounds on what a file can make this reader allocate.
///
/// Working memory for one decoded record is at most about 13 bytes per cell of `max_cells`: up
/// to three `f32` grids (values, unpacked, bitmap expansion) and a one-byte map, plus group tables
/// bounded by the bits actually present in the file. That is on top of the input itself. A JPEG 2000
/// record is bounded separately by `max_jpeg2000_bytes`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// Most cells in one grid. A small hostile file can declare a huge constant field.
    pub max_cells: usize,
    /// Most records in one file.
    pub max_records: usize,
    /// Largest input, held in memory whole.
    pub max_input_bytes: u64,
    /// Most working memory for one JPEG 2000 record, worked out from its headers before the decoder
    /// is entered. That decoder's allocations abort the process when they fail, and `max_cells`
    /// alone would admit over 4 GiB from a few hundred bytes. About 17 bytes per cell.
    pub max_jpeg2000_bytes: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            // 1 GiB of f32: over ten times the largest sampled product (MRMS, 24.5 million).
            max_cells: 1 << 28,
            max_records: 1 << 20,
            max_input_bytes: 1 << 34,
            // About 63 million cells, which clears every product the importer was checked on.
            max_jpeg2000_bytes: 1 << 30,
        }
    }
}

/// A GRIB2 file in memory.
#[derive(Debug)]
pub struct Grib2File {
    file_name: String,
    data: Vec<u8>,
    records: Vec<RecordSpan>,
    limits: Limits,
}

impl Grib2File {
    /// Read a file. The file's name matters: Vortex sets some time windows from it.
    pub fn open(path: &Path) -> Result<Self, VortexError> {
        Self::open_with_limits(path, Limits::default())
    }

    pub fn open_with_limits(path: &Path, limits: Limits) -> Result<Self, VortexError> {
        let file_name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let size = std::fs::metadata(path)?.len();
        if size > limits.max_input_bytes {
            return Err(too_large(size, &limits));
        }
        Self::from_bytes(file_name, std::fs::read(path)?, limits)
    }

    /// Scan bytes. Fails on a malformed message, and refuses a file holding GRIB1 (V10).
    pub fn from_bytes(
        file_name: impl Into<String>,
        data: Vec<u8>,
        limits: Limits,
    ) -> Result<Self, VortexError> {
        if data.len() as u64 > limits.max_input_bytes {
            return Err(too_large(data.len() as u64, &limits));
        }
        let found = scan::scan(&data, limits.max_records)?;
        if found.other_editions > 0 {
            return Err(VortexError::Refused {
                what: "GRIB1".to_string(),
                reason: format!(
                    "{} message(s) are not edition 2; GRIB1 is read by source::grib1::Grib1File",
                    found.other_editions
                ),
            });
        }
        if found.records.is_empty() {
            return Err(VortexError::Decode {
                format: "GRIB2",
                message: "no GRIB2 message found".to_string(),
            });
        }
        Ok(Grib2File {
            file_name: file_name.into(),
            data,
            records: found.records,
            limits,
        })
    }

    pub fn file_name(&self) -> &str {
        &self.file_name
    }

    pub fn record_count(&self) -> usize {
        self.records.len()
    }

    fn span(&self, index: usize) -> Result<&RecordSpan, VortexError> {
        self.records.get(index).ok_or_else(|| VortexError::Refused {
            what: "GRIB2 record".to_string(),
            reason: format!("index {index} of {}", self.records.len()),
        })
    }

    /// Decode one record as `Grib2Record.readData` does: no variable's missing values applied.
    /// Missing cells from a bitmap or packing are NaN.
    pub fn decode_record(&self, index: usize) -> Result<Vec<f32>, VortexError> {
        let span = self.span(index)?;
        let grid = &self.data[span.grid.clone()];
        let total_points = gds::total_points(grid)?;
        // Section 7 octets 1-5 are its header (Grib2DataReader.getData: startPos + 5).
        let payload = &self.data[span.data.start + 5..span.data.end];
        unpack::unpack(
            &unpack::Inputs {
                representation: &self.data[span.representation.clone()],
                bitmap_section: &self.data[span.bitmap.clone()],
                payload,
                total_points,
            },
            &self.limits,
        )
    }

    /// The georeference of one record.
    pub fn georeference(&self, index: usize) -> Result<Georeference, VortexError> {
        let grid = &self.data[self.span(index)?.grid.clone()];
        // The axis check walks every point, so the limit comes first.
        let points = gds::total_points(grid)?;
        if points > self.limits.max_cells {
            return Err(VortexError::Refused {
                what: "GRIB2 grid".to_string(),
                reason: format!(
                    "{points} cells is over the limit of {}",
                    self.limits.max_cells
                ),
            });
        }
        gds::read_georeference(grid)
    }

    /// Discipline, parameter, level and time of one record.
    pub fn product(&self, index: usize) -> Result<Product, VortexError> {
        let span = self.span(index)?;
        pds::read_product(
            span.discipline,
            &self.data[span.identification.clone()],
            &self.data[span.product.clone()],
        )
    }

    /// Records of the file grouped as Vortex's variables, in name order. Records with no
    /// product table entry or an unsupported product are left out; see [`Self::unlisted`].
    pub fn variables(&self) -> Vec<products::Variable> {
        self.collect_variables().0
    }

    /// Records [`Self::variables`] left out, with the reason.
    pub fn unlisted(&self) -> Vec<(usize, VortexError)> {
        self.collect_variables().1
    }

    fn collect_variables(&self) -> (Vec<products::Variable>, Vec<(usize, VortexError)>) {
        let mut identities = Vec::new();
        let mut unlisted = Vec::new();
        // GribIospBuilder puts each grid in a group, which changes the variables' names. No
        // sampled product has several, so there is no reference for those names.
        let first_grid = self.records.first().map(|r| &self.data[r.grid.clone()]);
        if self
            .records
            .iter()
            .any(|r| Some(&self.data[r.grid.clone()]) != first_grid)
        {
            for index in 0..self.records.len() {
                unlisted.push((
                    index,
                    VortexError::Refused {
                        what: "GRIB2 variable".to_string(),
                        reason: "the file holds several grids".to_string(),
                    },
                ));
            }
            return (Vec::new(), unlisted);
        }
        for index in 0..self.records.len() {
            let identity = self.product(index).and_then(|product| {
                products::identify(&product).map(|entry| RecordIdentity {
                    index,
                    product,
                    entry,
                })
            });
            match identity {
                Ok(identity) => identities.push(identity),
                Err(error) => unlisted.push((index, error)),
            }
        }
        let (variables, grouping_errors) = products::group(identities);
        unlisted.extend(grouping_errors);
        (variables, unlisted)
    }

    /// Time steps in a variable.
    pub fn frame_count(&self, variable: &str) -> Result<usize, VortexError> {
        Ok(self.find_variable(variable)?.steps.len())
    }

    fn find_variable(&self, name: &str) -> Result<products::Variable, VortexError> {
        self.variables()
            .into_iter()
            .find(|v| v.name == name)
            .ok_or_else(|| VortexError::Refused {
                what: format!("GRIB2 variable {name}"),
                reason: "not in this file, or no product table entry".to_string(),
            })
    }

    /// One time step of one variable, with [`WindowRule::Refuse`]. See `crate::products` for the
    /// observed behaviour this refuses.
    pub fn frame(&self, variable: &str, step: usize) -> Result<GridFrame, VortexError> {
        self.frame_with(variable, step, WindowRule::default())
    }

    /// One time step of one variable: the frame `DataReader.getDto(step)` describes. With
    /// [`WindowRule::Vortex`] every window is Vortex's.
    pub fn frame_with(
        &self,
        variable: &str,
        step: usize,
        rule: WindowRule,
    ) -> Result<GridFrame, VortexError> {
        let variable = self.find_variable(variable)?;
        self.frame_of(&variable, step, rule)
    }

    /// [`Self::frame_with`] for a variable already found, so a caller reading many frames does not
    /// group every record again for each one.
    pub fn frame_of(
        &self,
        variable: &products::Variable,
        step: usize,
        rule: WindowRule,
    ) -> Result<GridFrame, VortexError> {
        let time = variable
            .steps
            .get(step)
            .ok_or_else(|| VortexError::Refused {
                what: format!("GRIB2 variable {}", variable.name),
                reason: format!("step {step} of {}", variable.steps.len()),
            })?;
        let georef = self.georeference(time.record)?;
        let mut cells = self.decode_record(time.record)?;
        products::apply_missing(&variable.entry, &mut cells);

        let window = products::time_window(&self.file_name, &variable.name, variable, step, rule)?;
        let units = products::units(variable);
        let data_type = products::data_type(variable, &window);
        let frame_time = frame_time(&window);
        let row_order = if georef.dy < 0.0 {
            RowOrder::NorthToSouth
        } else {
            RowOrder::SouthToNorth
        };
        let nodata = variable.entry.missing;
        Ok(GridFrame::new(GridFrameParts {
            cells,
            nx: georef.nx,
            ny: georef.ny,
            row_order,
            transform: GeoTransform::axis_aligned(
                georef.origin_x,
                georef.dx,
                georef.origin_y,
                georef.dy,
            ),
            crs: Crs::Wkt(georef.wkt),
            nodata: Some(nodata),
            time: frame_time,
            data_type,
            units,
            variable: variable.name.clone(),
            description: Some(variable.description.clone()),
        })?)
    }
}

fn too_large(size: u64, limits: &Limits) -> VortexError {
    VortexError::Refused {
        what: "GRIB2 file".to_string(),
        reason: format!(
            "{size} bytes is over the limit of {}",
            limits.max_input_bytes
        ),
    }
}

/// (julian day, seconds past midnight). An end at midnight stays 0000 of the next day; writing
/// it as 2400 is the DSS sink's concern.
pub(crate) fn frame_time(window: &products::TimeWindow) -> FrameTime {
    let split = |t: i64| (t.div_euclid(86_400), t.rem_euclid(86_400));
    if window.start == window.end {
        FrameTime::Instant(split(window.end))
    } else {
        FrameTime::Period {
            start: split(window.start),
            end: split(window.end),
        }
    }
}
