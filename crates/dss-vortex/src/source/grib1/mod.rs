//! GRIB1 source: NCEP Stage IV, 2002 to mid-2020, with Vortex 1.1.0's own decoded array as the
//! reference.
//!
//! Layers, each with its source named in its file:
//! - [`scan`]: messages and sections.
//! - [`pds`]: section 1, what a record is and when.
//! - [`gds`]: section 2, the polar stereographic HRAP grid.
//! - [`unpack`]: sections 3 and 4, bitmap and simple packing, in `Grib1DataReader`'s arithmetic.
//!
//! Scope is what Stage IV GRIB1 uses: NCEP (centre 7) parameter table 2 parameter 61, total
//! precipitation, at the surface, accumulated (time range indicator 4) over hours, on a spherical
//! polar stereographic grid scanned +i +j, simple packing, with or without an explicit bitmap.
//! Every other variant is a typed refusal (`Refused` or `Unsupported`), not a guess: there is no
//! sample, so no reference for what Vortex reports.
//!
//! Names, descriptions, units and windows are what Vortex 1.1.0 and 0.13.3 report for these files,
//! through NetCDF-Java 5.5.3 `Grib1Iosp.makeVariableNameFromTables` and `makeVariableLongName`
//! (`Total_precipitation_surface_1_Hour_Accumulation`, `kg m^-2`, NaN missing). The naming, time
//! and data type rules downstream are the GRIB2 reader's (`crate::products`).

pub mod gds;
pub mod pds;
pub mod scan;
mod unpack;

use crate::error::VortexError;
use crate::import::{GridSource, SourceGridDef, StepMeta};
use crate::products::window::{self, Accumulation};
use crate::products::{self, Step, TableEntry, Variable, WindowRule};
use crate::source::grib::{frame_time, Georeference, Limits};
use crate::{Crs, GeoTransform, GridFrame, GridFrameParts, RowOrder};
use pds::ProductDefinition;
use scan::Message;
use std::path::Path;

/// NCEP's centre number, and the parameter, level and time range Stage IV carries.
const NCEP: u8 = 7;
/// NCEP code table 2, version 2, parameter 61: `APCP`, "Total precipitation", `kg m^-2`
/// (NetCDF-Java `resources/grib1/ncep/ncepGrib1-2.xml` as Vortex 1.1.0 reports it).
const TOTAL_PRECIPITATION: u8 = 61;
const TABLE_VERSION: u8 = 2;
/// Code table 3, level type 1: "surface", "Ground or water surface".
const SURFACE: u8 = 1;
/// NetCDF-Java's names of the table entry, as Vortex 1.1.0 reports them for Stage IV GRIB1.
const PARAMETER_NAME: &str = "Total precipitation";
const PARAMETER_UNIT: &str = "kg m^-2";
const LEVEL_SHORT: &str = "surface";
const LEVEL_LONG: &str = "Ground or water surface";
/// `GribStatType.Accumulation`, as code table 4.10's 1, so the data type rules give PER-CUM.
const ACCUMULATION: u8 = 1;

/// A GRIB1 file in memory.
#[derive(Debug)]
pub struct Grib1File {
    file_name: String,
    data: Vec<u8>,
    messages: Vec<Message>,
    limits: Limits,
}

fn too_large(size: u64, limits: &Limits) -> VortexError {
    VortexError::Refused {
        what: "GRIB1 file".to_string(),
        reason: format!(
            "{size} bytes is over the limit of {}",
            limits.max_input_bytes
        ),
    }
}

impl Grib1File {
    /// Read a file. Its name matters as it does for GRIB2: Vortex's window rules read it.
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

    /// Scan bytes. Fails on a malformed message, and refuses a file that also holds GRIB2.
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
                what: "GRIB1 file".to_string(),
                reason: format!(
                    "{} message(s) are not edition 1; a file mixing editions has no sample",
                    found.other_editions
                ),
            });
        }
        if found.messages.is_empty() {
            return Err(VortexError::Decode {
                format: "GRIB1",
                message: "no GRIB1 message found".to_string(),
            });
        }
        Ok(Grib1File {
            file_name: file_name.into(),
            data,
            messages: found.messages,
            limits,
        })
    }

    pub fn file_name(&self) -> &str {
        &self.file_name
    }

    pub fn record_count(&self) -> usize {
        self.messages.len()
    }

    fn message(&self, index: usize) -> Result<&Message, VortexError> {
        self.messages
            .get(index)
            .ok_or_else(|| VortexError::Refused {
                what: "GRIB1 record".to_string(),
                reason: format!("index {index} of {}", self.messages.len()),
            })
    }

    /// Section 1 of one record.
    pub fn product(&self, index: usize) -> Result<ProductDefinition, VortexError> {
        pds::read(&self.data[self.message(index)?.product.clone()])
    }

    fn grid_section(&self, index: usize) -> Result<&[u8], VortexError> {
        let message = self.message(index)?;
        match &message.grid {
            Some(range) => Ok(&self.data[range.clone()]),
            None => Err(VortexError::Refused {
                what: "GRIB1 grid".to_string(),
                reason: format!(
                    "record {index} has no GDS and names predefined grid {}; no Stage IV file \
                     does that",
                    self.data[message.product.start + 6]
                ),
            }),
        }
    }

    /// The georeference of one record.
    pub fn georeference(&self, index: usize) -> Result<Georeference, VortexError> {
        let grid = self.grid_section(index)?;
        let (_, _, points) = gds::points(grid)?;
        if points > self.limits.max_cells {
            return Err(VortexError::Refused {
                what: "GRIB1 grid".to_string(),
                reason: format!(
                    "{points} cells is over the limit of {}",
                    self.limits.max_cells
                ),
            });
        }
        gds::read_georeference(grid)
    }

    /// Decode one record as `Grib1Record.readData` does. Points outside the bitmap are NaN.
    pub fn decode_record(&self, index: usize) -> Result<Vec<f32>, VortexError> {
        let message = self.message(index)?;
        let product = self.product(index)?;
        // Validates the grid (representation, scan mode, earth) and the cell limit first.
        let georef = self.georeference(index)?;
        let points = georef.nx * georef.ny;
        unpack::unpack(
            &unpack::Inputs {
                bitmap_section: message.bitmap.clone().map(|r| &self.data[r]),
                binary_section: &self.data[message.data.clone()],
                decimal_scale: product.decimal_scale,
                points,
            },
            &self.limits,
        )
    }

    /// The record's identity, or why it has none here.
    fn identify(&self, index: usize) -> Result<(ProductDefinition, i64, i64), VortexError> {
        let p = self.product(index)?;
        if p.center != NCEP
            || p.table_version != TABLE_VERSION
            || p.parameter != TOTAL_PRECIPITATION
        {
            return Err(VortexError::Refused {
                what: format!(
                    "GRIB1 parameter {} of table {} from centre {}",
                    p.parameter, p.table_version, p.center
                ),
                reason: "no product table entry; this reader takes NCEP table 2 parameter 61, \
                         the total precipitation of Stage IV"
                    .to_string(),
            });
        }
        if p.level_type != SURFACE {
            return Err(VortexError::Refused {
                what: "GRIB1 level".to_string(),
                reason: format!(
                    "level type {} (code table 3); Stage IV is at the surface, 1",
                    p.level_type
                ),
            });
        }
        let (start, end) = pds::accumulation(&p)?;
        Ok((p, start, end))
    }

    /// Records of the file grouped as Vortex's variables. Records outside the scope are left out;
    /// see [`Self::unlisted`].
    pub fn variables(&self) -> Vec<Variable> {
        self.collect_variables().0
    }

    /// Records [`Self::variables`] left out, with the reason.
    pub fn unlisted(&self) -> Vec<(usize, VortexError)> {
        self.collect_variables().1
    }

    fn collect_variables(&self) -> (Vec<Variable>, Vec<(usize, VortexError)>) {
        let mut unlisted = Vec::new();
        let refuse_all = |reason: &str| {
            (0..self.messages.len())
                .map(|i| {
                    (
                        i,
                        VortexError::Refused {
                            what: "GRIB1 variable".to_string(),
                            reason: reason.to_string(),
                        },
                    )
                })
                .collect::<Vec<_>>()
        };
        // As for GRIB2: GribIospBuilder puts each grid in a group, which changes the names, and no
        // sample has several.
        let grid_of = |m: &Message| m.grid.clone().map(|r| &self.data[r]);
        let first_grid = self.messages.first().map(grid_of);
        if self.messages.iter().any(|m| Some(grid_of(m)) != first_grid) {
            return (Vec::new(), refuse_all("the file holds several grids"));
        }
        let mut found = Vec::new();
        for index in 0..self.messages.len() {
            match self.identify(index) {
                Ok(identity) => found.push((index, identity)),
                Err(error) => unlisted.push((index, error)),
            }
        }
        if found.is_empty() {
            return (Vec::new(), unlisted);
        }
        // CoordinateTimeIntv.getTimeIntervalName: one length names the variable; several are
        // "Mixed_intervals", which no Stage IV file has.
        let length = found[0].1 .2 - found[0].1 .1;
        if found.iter().any(|(_, (_, s, e))| e - s != length) {
            for (index, _) in found {
                unlisted.push((
                    index,
                    VortexError::Refused {
                        what: "GRIB1 variable".to_string(),
                        reason: "accumulations of several lengths in one file (Mixed_intervals); \
                                 no Stage IV file has that"
                            .to_string(),
                    },
                ));
            }
            return (Vec::new(), unlisted);
        }
        if found
            .iter()
            .any(|(_, (p, _, _))| p.level != found[0].1 .0.level)
        {
            for (index, _) in found {
                unlisted.push((
                    index,
                    VortexError::Refused {
                        what: "GRIB1 variable".to_string(),
                        reason: "several levels".to_string(),
                    },
                ));
            }
            return (Vec::new(), unlisted);
        }
        let mut steps: Vec<Step> = found
            .iter()
            .map(|(index, (_, start, end))| Step {
                record: *index,
                start: *start,
                end: *end,
            })
            .collect();
        // TimeCoordIntvValue and CoordinateTime sort by end, then start.
        steps.sort_by_key(|s| (s.end, s.start));
        if steps
            .windows(2)
            .any(|w| (w[0].start, w[0].end) == (w[1].start, w[1].end))
        {
            for (index, _) in found {
                unlisted.push((
                    index,
                    VortexError::Refused {
                        what: "GRIB1 variable".to_string(),
                        reason: "two records at one time".to_string(),
                    },
                ));
            }
            return (Vec::new(), unlisted);
        }
        // unit_seconds accepted only hours, so the interval is whole hours.
        let interval = format!("{}_Hour", length / 3600);
        let name = format!("Total_precipitation_{LEVEL_SHORT}_{interval}_Accumulation");
        let description = format!("{PARAMETER_NAME} ({interval} Accumulation) @ {LEVEL_LONG}");
        let variable = Variable {
            name,
            description,
            entry: TableEntry {
                parameter: PARAMETER_NAME.to_string(),
                unit: PARAMETER_UNIT.to_string(),
                fill: None,
                missing: f32::NAN,
                unknown_parameter: false,
            },
            statistic: Some(ACCUMULATION),
            interval: true,
            steps,
        };
        (vec![variable], unlisted)
    }

    fn find_variable(&self, name: &str) -> Result<Variable, VortexError> {
        self.variables()
            .into_iter()
            .find(|v| v.name == name)
            .ok_or_else(|| VortexError::Refused {
                what: format!("GRIB1 variable {name}"),
                reason: "not in this file, or outside this reader's scope".to_string(),
            })
    }

    /// One time step of one variable, with [`WindowRule::Refuse`].
    pub fn frame(&self, variable: &str, step: usize) -> Result<GridFrame, VortexError> {
        self.frame_with(variable, step, WindowRule::default())
    }

    pub fn frame_with(
        &self,
        variable: &str,
        step: usize,
        rule: WindowRule,
    ) -> Result<GridFrame, VortexError> {
        let variable = self.find_variable(variable)?;
        self.frame_of(&variable, step, rule)
    }

    /// [`Self::frame_with`] for a variable already found.
    pub fn frame_of(
        &self,
        variable: &Variable,
        step: usize,
        rule: WindowRule,
    ) -> Result<GridFrame, VortexError> {
        let time = variable
            .steps
            .get(step)
            .ok_or_else(|| VortexError::Refused {
                what: format!("GRIB1 variable {}", variable.name),
                reason: format!("step {step} of {}", variable.steps.len()),
            })?;
        let georef = self.georeference(time.record)?;
        let cells = self.decode_record(time.record)?;
        let window = products::time_window(&self.file_name, &variable.name, variable, step, rule)?;
        Ok(GridFrame::new(GridFrameParts {
            cells,
            nx: georef.nx,
            ny: georef.ny,
            // Scan mode 64 only, so rows run south to north and dy is positive.
            row_order: RowOrder::SouthToNorth,
            transform: GeoTransform::axis_aligned(
                georef.origin_x,
                georef.dx,
                georef.origin_y,
                georef.dy,
            ),
            crs: Crs::Wkt(georef.wkt),
            nodata: Some(variable.entry.missing),
            time: frame_time(&window),
            data_type: products::data_type(variable, &window),
            units: products::units(variable),
            variable: variable.name.clone(),
            description: Some(variable.description.clone()),
        })?)
    }
}

/// GRIB1 through [`Grib1File`], for the importer.
pub struct Grib1Source {
    file: Grib1File,
    variables: Vec<Variable>,
    rule: WindowRule,
}

impl Grib1Source {
    pub fn new(file: Grib1File, rule: WindowRule) -> Self {
        let variables = file.variables();
        Grib1Source {
            file,
            variables,
            rule,
        }
    }

    fn variable(&self, name: &str) -> Result<&Variable, VortexError> {
        self.variables
            .iter()
            .find(|v| v.name == name)
            .ok_or_else(|| VortexError::Refused {
                what: format!("GRIB1 variable {name}"),
                reason: "not in this file, or outside this reader's scope".into(),
            })
    }

    fn record(&self, name: &str, step: usize) -> Result<usize, VortexError> {
        let variable = self.variable(name)?;
        variable
            .steps
            .get(step)
            .map(|s| s.record)
            .ok_or_else(|| VortexError::Refused {
                what: format!("GRIB1 variable {name}"),
                reason: format!("step {step} of {}", variable.steps.len()),
            })
    }
}

impl GridSource for Grib1Source {
    fn file_name(&self) -> &str {
        self.file.file_name()
    }

    fn variables(&self) -> Vec<String> {
        self.variables.iter().map(|v| v.name.clone()).collect()
    }

    fn left_out(&self) -> Vec<String> {
        self.file
            .unlisted()
            .into_iter()
            .map(|(i, e)| format!("record {i}: {e}"))
            .collect()
    }

    fn step_count(&self, variable: &str) -> Result<usize, VortexError> {
        Ok(self.variable(variable)?.steps.len())
    }

    fn step_meta(&self, name: &str, step: usize) -> Result<StepMeta, VortexError> {
        let variable = self.variable(name)?;
        let window = products::time_window(self.file.file_name(), name, variable, step, self.rule)?;
        Ok(StepMeta {
            time: frame_time(&window),
            units: products::units(variable),
            description: Some(variable.description.clone()),
            source_type: products::data_type(variable, &window),
        })
    }

    fn grid(&self, name: &str, step: usize) -> Result<SourceGridDef, VortexError> {
        let record = self.record(name, step)?;
        let g = self.file.georeference(record)?;
        Ok(SourceGridDef {
            nx: g.nx,
            ny: g.ny,
            transform: GeoTransform::axis_aligned(g.origin_x, g.dx, g.origin_y, g.dy),
            crs: Crs::Wkt(g.wkt),
            nodata: Some(self.variable(name)?.entry.missing),
            stored_wkt: None,
        })
    }

    fn frame(&self, name: &str, step: usize) -> Result<GridFrame, VortexError> {
        self.file.frame_of(self.variable(name)?, step, self.rule)
    }

    fn accumulation_stated_by(
        &self,
        variable: &str,
        wanted: Accumulation,
    ) -> Option<Result<&'static str, VortexError>> {
        Some(window::stated_by(self.file.file_name(), variable, wanted))
    }
}
