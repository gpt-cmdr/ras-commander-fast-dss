//! The importer's side of the PRISM and SNODAS readers: BIL beside its `.hdr`,
//! rasters in a ZIP archive, and SNODAS tars and members, each as a [`GridSource`].
//!
//! `open_source` picks these by content first (ZIP and tar signatures) and by name only where the
//! content does not say what the grid is (`.bil`, a SNODAS `.dat` or `.dat.gz`), as it does for
//! ASCII grids. A SNODAS header beside a member is gzip or text by its own first bytes. Vortex
//! dispatches all of them by name (`DataReader.DataReaderBuilder.build`).

use crate::error::VortexError;
use crate::frame::{GridFrame, GridFrameParts};
use crate::import::{GridSource, OpenOptions, SourceGridDef, StepMeta};
use crate::source::bil::{self, BilFile};
use crate::source::raster::{self, ReadOptions};
use crate::source::snodas::{self, SnodasFile};
use crate::source::zipped::{self, ZippedRaster};
use std::path::Path;

/// As `import::RASTER_DEFAULT_MARKER`: a raster with no stated nodata is imported under -9999,
/// Vortex's `AscDataReader` default. Vortex's `BilDataReader` has no default and fails on such a
/// file (`Double[0]` is null); this importer takes the raster rule.
const DEFAULT_MARKER: f32 = -9999.0;

struct Item {
    variable: String,
    short_name: String,
    frame: GridFrame,
    stored_wkt: Option<String>,
}

/// Rasters decoded at opening: one BIL, or the rasters of one ZIP archive. One input is in memory at
/// a time, as for [`crate::import::RasterSource`].
pub struct FramesSource {
    file_name: String,
    items: Vec<Item>,
}

impl FramesSource {
    fn from_bil(file_name: String, file: BilFile) -> Self {
        let short_name = file.short_name().to_string();
        let frame = file.into_frame();
        FramesSource {
            file_name,
            items: vec![Item {
                variable: frame.parts().variable.clone(),
                short_name,
                frame,
                stored_wkt: None,
            }],
        }
    }

    fn from_zip(file_name: String, rasters: Vec<ZippedRaster>) -> Self {
        let items = rasters
            .into_iter()
            .map(|r| Item {
                variable: r.frame.parts().variable.clone(),
                short_name: r.short_name,
                frame: r.frame,
                stored_wkt: r.stored_wkt,
            })
            .collect();
        FramesSource { file_name, items }
    }

    fn item(&self, variable: &str, step: usize) -> Result<&Item, VortexError> {
        self.items
            .iter()
            .filter(|i| i.variable == variable)
            .nth(step)
            .ok_or_else(|| VortexError::Refused {
                what: format!("variable {variable}, step {step}"),
                reason: format!("not in {}", self.file_name),
            })
    }
}

impl GridSource for FramesSource {
    fn file_name(&self) -> &str {
        &self.file_name
    }

    fn variables(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for i in &self.items {
            if !out.contains(&i.variable) {
                out.push(i.variable.clone());
            }
        }
        out
    }

    fn step_count(&self, variable: &str) -> Result<usize, VortexError> {
        let n = self.items.iter().filter(|i| i.variable == variable).count();
        if n == 0 {
            return Err(VortexError::Refused {
                what: format!("variable {variable}"),
                reason: format!("not in {}", self.file_name),
            });
        }
        Ok(n)
    }

    fn step_meta(&self, variable: &str, step: usize) -> Result<StepMeta, VortexError> {
        let p = self.item(variable, step)?.frame.parts();
        Ok(StepMeta {
            time: p.time,
            units: p.units.clone(),
            description: p.description.clone(),
            source_type: p.data_type,
        })
    }

    fn grid(&self, variable: &str, step: usize) -> Result<SourceGridDef, VortexError> {
        let item = self.item(variable, step)?;
        let p = item.frame.parts();
        Ok(SourceGridDef {
            nx: p.nx,
            ny: p.ny,
            transform: p.transform,
            crs: p.crs.clone(),
            nodata: Some(p.nodata.unwrap_or(DEFAULT_MARKER)),
            stored_wkt: item.stored_wkt.clone(),
        })
    }

    fn frame(&self, variable: &str, step: usize) -> Result<GridFrame, VortexError> {
        let p = self.item(variable, step)?.frame.parts();
        let mut cells: Vec<f32> = Vec::new();
        cells
            .try_reserve_exact(p.cells.len())
            .map_err(|_| VortexError::Refused {
                what: "raster".into(),
                reason: format!("cannot hold a frame of {} cells", p.cells.len()),
            })?;
        cells.extend_from_slice(&p.cells);
        Ok(GridFrame::new(GridFrameParts {
            cells,
            nx: p.nx,
            ny: p.ny,
            row_order: p.row_order,
            transform: p.transform,
            crs: p.crs.clone(),
            nodata: Some(p.nodata.unwrap_or(DEFAULT_MARKER)),
            time: p.time,
            data_type: p.data_type,
            units: p.units.clone(),
            variable: p.variable.clone(),
            description: p.description.clone(),
        })?)
    }

    fn short_name(&self, variable: &str) -> Result<String, VortexError> {
        Ok(self.item(variable, 0)?.short_name.clone())
    }
}

/// A SNODAS tar or member: each product is a variable of one step, decoded when it is written.
pub struct SnodasSource {
    file: SnodasFile,
}

impl SnodasSource {
    fn check(&self, step: usize) -> Result<(), VortexError> {
        if step == 0 {
            Ok(())
        } else {
            Err(VortexError::Refused {
                what: format!("step {step}"),
                reason: "each SNODAS product is one grid".into(),
            })
        }
    }
}

impl GridSource for SnodasSource {
    fn file_name(&self) -> &str {
        self.file.file_name()
    }

    fn variables(&self) -> Vec<String> {
        self.file.variables()
    }

    fn step_count(&self, variable: &str) -> Result<usize, VortexError> {
        self.file.grid(variable)?;
        Ok(1)
    }

    fn step_meta(&self, variable: &str, step: usize) -> Result<StepMeta, VortexError> {
        self.check(step)?;
        let g = self.file.grid(variable)?;
        let options = self.file.options();
        Ok(StepMeta {
            time: g.time(options)?,
            units: g.product.units.to_string(),
            description: Some(g.product.name.to_string()),
            source_type: g.data_type(options)?,
        })
    }

    fn grid(&self, variable: &str, step: usize) -> Result<SourceGridDef, VortexError> {
        self.check(step)?;
        let g = self.file.grid(variable)?;
        Ok(SourceGridDef {
            nx: snodas::NCOLS,
            ny: snodas::NROWS,
            transform: g.transform(self.file.options())?,
            crs: crate::frame::Crs::Wkt(snodas::WGS84_WKT.to_string()),
            nodata: Some(self.file.options().marker()),
            stored_wkt: None,
        })
    }

    fn frame(&self, variable: &str, step: usize) -> Result<GridFrame, VortexError> {
        self.check(step)?;
        self.file.frame(variable)
    }

    /// Under the default timing the product states its type (PER-CUM daily totals, INST-VAL
    /// SWE and depth). Under Vortex's timing, and the header as stated, the importer infers it as
    /// Vortex does.
    fn stated_data_type(
        &self,
        variable: &str,
        step: usize,
    ) -> Result<Option<crate::frame::DataType>, VortexError> {
        self.check(step)?;
        let options = self.file.options();
        if options.times != snodas::SnodasTimes::Header {
            return Ok(None);
        }
        Ok(Some(self.file.grid(variable)?.data_type(options)?))
    }
}

fn raster_options(options: &OpenOptions) -> ReadOptions {
    ReadOptions {
        limits: raster::Limits {
            max_cells: options.limits.max_cells,
            max_input_bytes: options.limits.max_input_bytes,
        },
        ..ReadOptions::default()
    }
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

fn read_whole(path: &Path, limit: u64) -> Result<Vec<u8>, VortexError> {
    let size = std::fs::metadata(path)?.len();
    if size > limit {
        return Err(VortexError::Refused {
            what: format!("input {}", path.display()),
            reason: format!("{size} bytes is over the limit of {limit}"),
        });
    }
    // The stated size is checked first for a clear message; the read itself is bounded, because a
    // pipe or device can state 0 bytes and stream for ever.
    crate::source::archive::read_file_bounded(path, limit, &format!("input {}", path.display()))
}

pub(crate) fn open_zip(
    path: &Path,
    options: &OpenOptions,
    stop: Option<&crate::import::StopFlag>,
) -> Result<Box<dyn GridSource>, VortexError> {
    let data = read_whole(path, options.limits.max_input_bytes)?;
    let name = file_name(path);
    let rasters = zipped::read_zip_with(
        &name,
        &data,
        raster_options(options),
        options.limits.archive,
        stop,
    )?;
    Ok(Box::new(FramesSource::from_zip(name, rasters)))
}

pub(crate) fn open_bil(
    path: &Path,
    options: &OpenOptions,
) -> Result<Box<dyn GridSource>, VortexError> {
    let file = BilFile::open_with(path, raster_options(options))?;
    Ok(Box::new(FramesSource::from_bil(file_name(path), file)))
}

pub(crate) fn open_snodas_tar(
    path: &Path,
    options: &OpenOptions,
) -> Result<Box<dyn GridSource>, VortexError> {
    let data = read_whole(path, options.limits.max_input_bytes)?;
    let file = SnodasFile::from_tar(
        file_name(path),
        data,
        options.snodas,
        options.limits.max_input_bytes,
    )?;
    Ok(Box::new(SnodasSource { file }))
}

/// A `.dat` or `.dat.gz` with its NOHRSC header beside it, if there is one: `<stem>.Hdr` or
/// `<stem>.txt`, either gzipped, matched without regard to letter case.
pub(crate) fn open_snodas_member(
    path: &Path,
    options: &OpenOptions,
) -> Result<Box<dyn GridSource>, VortexError> {
    let name = file_name(path);
    let lower = name.to_ascii_lowercase();
    let stem_len = if lower.ends_with(".dat.gz") {
        name.len() - 7
    } else {
        name.len() - 4
    };
    let stem = &name[..stem_len];
    let parent = match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    };
    let wanted =
        [".hdr", ".txt", ".hdr.gz", ".txt.gz"].map(|s| format!("{stem}{s}").to_ascii_lowercase());
    let mut header = None;
    for entry in std::fs::read_dir(parent)? {
        let entry = entry?;
        let n = entry.file_name().to_string_lossy().to_ascii_lowercase();
        if wanted.contains(&n) {
            if header.is_some() {
                return Err(VortexError::Refused {
                    what: format!("SNODAS {name}"),
                    reason: "more than one header beside it".into(),
                });
            }
            header = Some(read_whole(&entry.path(), bil::MAX_SIDECAR)?);
        }
    }
    let dat = read_whole(path, options.limits.max_input_bytes)?;
    let file = SnodasFile::from_member(
        name,
        dat,
        header.as_deref(),
        options.snodas,
        options.limits.max_input_bytes,
    )?;
    Ok(Box::new(SnodasSource { file }))
}
