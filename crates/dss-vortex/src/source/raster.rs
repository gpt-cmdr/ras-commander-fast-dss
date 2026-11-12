//! GeoTIFF and ASCII grid source: bytes to one frame, with what GDAL hands Vortex's `AscDataReader`
//! as the reference, compared with what `rasterio` and Vortex 1.1.0 read from the same files.
//!
//! Layers, each with its source named in its file:
//! - `geotiff`: the TIFF container, chunks, predictors and the geotransform.
//! - `geokeys`: GeoTIFF keys to a CRS, and the reverse for the sink.
//! - `asc`: ASCII grid text and its `.prj`.
//! - `names`: names, units and time windows, which Vortex takes from the file name.
//!
//! The TIFF container is parsed here rather than by the `tiff` crate, which is not linked into
//! production code. Its decoder's `read_image` allocates with `vec![0; n]` from header fields and its tag reader
//! builds a `Vec` of `Value` per offset, so a failed allocation aborts the process and
//! `catch_unwind` cannot catch it. Here every allocation sized by the input is fallible and checked
//! against what the file can hold first.
//!
//! BIL is not read here: it is a separate format and is not supported yet.

mod asc;
pub(crate) mod geokeys;
pub(crate) mod geotiff;
pub(crate) mod names;

use crate::error::VortexError;
use crate::{Crs, GeoTransform, GridFrame, GridFrameParts, RowOrder};
use std::path::Path;

pub(crate) use asc::{format_g20, write_g20, NumberText};

/// Bounds on what a file can make this reader allocate. The defaults are the GRIB2 reader's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// Most cells in one grid.
    pub max_cells: usize,
    /// Largest input, held in memory whole.
    pub max_input_bytes: u64,
}

impl Default for Limits {
    fn default() -> Self {
        let grib = super::grib::Limits::default();
        Limits {
            max_cells: grib.max_cells,
            max_input_bytes: grib.max_input_bytes,
        }
    }
}

/// What to do with a CRS the file states and this library cannot represent or understand.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum UnknownCrs {
    /// A typed refusal.
    #[default]
    Refuse,
    /// Read the raster with [`Crs::Undefined`] and say why in [`CrsStatus::Unresolved`].
    Undefined,
}

/// Options for reading. The defaults refuse any CRS this library cannot represent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ReadOptions {
    pub limits: Limits,
    pub unknown_crs: UnknownCrs,
}

/// How the frame's CRS was arrived at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CrsStatus {
    /// The file stated it, and the frame carries it.
    Stated,
    /// The file states none, so the frame's is [`Crs::Undefined`].
    NotInFile,
    /// The file states one this library could not resolve, and the caller chose
    /// [`UnknownCrs::Undefined`]. The frame's is [`Crs::Undefined`]; it is not a guess.
    Unresolved { reason: String },
}

/// The two formats this reader takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RasterFormat {
    GeoTiff,
    Asc,
}

/// One raster file, decoded.
#[derive(Debug, Clone)]
pub struct RasterFile {
    format: RasterFormat,
    frame: GridFrame,
    crs_status: CrsStatus,
    stored_wkt: Option<String>,
}

/// What a format's decoder returns, before the file name's metadata.
pub(crate) struct Decoded {
    pub cells: Vec<f32>,
    pub nx: usize,
    pub ny: usize,
    pub transform: GeoTransform,
    pub nodata: Option<f32>,
    pub crs: CrsResolution,
    /// The CRS as GDAL's `GetProjection` words it, where a DSS record's form depends on that wording
    /// and this differs from what `crs` would store. See `geokeys::stored_text`.
    pub stored_wkt: Option<String>,
}

/// What a file says about its CRS.
pub(crate) enum CrsResolution {
    Stated(Crs),
    NotInFile,
    Unresolved(String),
}

/// The sidecar beside `path` that GDAL applies to what it reads, as (what was found, what it does).
/// GDAL matches sibling names without regard to case (`CPLCheckForFile`), so this lists the folder.
///
/// Measured with GDAL 3.12.2: `<name>.aux.xml` and the
/// HFA `<name>.aux` or `<stem>.aux` replace nodata, CRS and georeference (`GDALPamDataset`), and
/// `<name>.msk` replaces the mask (`GDALDefaultOverviews::HaveMaskFile`). A world file or `.tab`
/// has no effect on a raster with an internal georeference, and one without is refused for that.
/// `<name>.ovr` only offers reduced-resolution data, which is not read here, as internal overviews
/// are not; `with_overviews` is for the writer, where it would describe data no longer there.
///
/// `read_pam` leaves `<name>.aux.xml` out of the search, for a reader that applies it: it is read
/// beside a GeoTIFF under the rule a PRISM ZIP's member gets ([`pam_sidecar`], [`apply_pam_nodata`]).
pub(crate) fn applied_sidecar(
    path: &Path,
    with_overviews: bool,
    read_pam: bool,
) -> Result<Option<(String, &'static str)>, VortexError> {
    let Some(name) = path.file_name().map(|n| n.to_string_lossy().into_owned()) else {
        return Ok(None);
    };
    let stem = path
        .file_stem()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    const PAM: &str = "GDAL lets it override nodata, CRS and georeference";
    let mut wanted = Vec::new();
    if !read_pam {
        wanted.push((format!("{name}.aux.xml"), ".aux.xml", PAM));
    }
    wanted.extend([
        (format!("{name}.aux"), ".aux", PAM),
        (format!("{stem}.aux"), ".aux", PAM),
        (
            format!("{name}.msk"),
            ".msk",
            "GDAL takes the mask from it instead of from nodata",
        ),
    ]);
    if with_overviews {
        wanted.push((
            format!("{name}.ovr"),
            ".ovr",
            "GDAL would offer its overviews of the old data with the new file",
        ));
    }
    let parent = match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    };
    for entry in std::fs::read_dir(parent)? {
        let entry = entry?.file_name();
        let found = entry.to_string_lossy();
        if let Some((_, kind, why)) = wanted.iter().find(|w| w.0.eq_ignore_ascii_case(&found)) {
            return Ok(Some((format!("{kind} sidecar, {found}"), why)));
        }
    }
    Ok(None)
}

/// The `<name>.aux.xml` beside `path`, matched without regard to case as GDAL matches it. Two files
/// whose names differ only in case are refused: which one GDAL takes depends on the file system.
pub(crate) fn pam_sidecar(path: &Path) -> Result<Option<std::path::PathBuf>, VortexError> {
    let Some(name) = path.file_name().map(|n| n.to_string_lossy().into_owned()) else {
        return Ok(None);
    };
    let wanted = format!("{name}.aux.xml");
    let parent = match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    };
    let mut found = None;
    for entry in std::fs::read_dir(parent)? {
        let entry = entry?;
        if entry
            .file_name()
            .to_string_lossy()
            .eq_ignore_ascii_case(&wanted)
        {
            if found.is_some() {
                return Err(VortexError::Refused {
                    what: format!("raster with a .aux.xml sidecar, {wanted}"),
                    reason: "more than one file has that name in some letter case".to_string(),
                });
            }
            found = Some(entry.path());
        }
    }
    Ok(found)
}

/// Apply a PAM `.aux.xml` to a raster's frame where it states only nodata and `STATISTICS_*`
/// items, and refuse it otherwise ([`super::bil::pam_nodata`]). This is the rule for a GeoTIFF or
/// ASCII grid inside a PRISM ZIP and, since 0.1.2, for a loose GeoTIFF:
/// - the PAM nodata is taken where the raster states none, and must be a float32 value;
/// - where the raster states one, the two must be equal (GDAL's GeoTIFF driver prefers its own);
/// - statistics change nothing that is read.
///
/// `what` names the sidecar in a refusal.
pub(crate) fn apply_pam_nodata(
    parts: &mut GridFrameParts,
    xml: &[u8],
    what: &str,
) -> Result<(), VortexError> {
    let refused = |reason: String| VortexError::Refused {
        what: what.to_string(),
        reason,
    };
    match (super::bil::pam_nodata(xml)?, parts.nodata) {
        (None, _) => {}
        (Some(v), None) => {
            let narrow = v as f32;
            if f64::from(narrow) != v {
                return Err(refused(format!("nodata {v} is not a float32 value")));
            }
            parts.nodata = Some(narrow);
        }
        (Some(v), Some(own)) if f64::from(own) == v => {}
        (Some(v), Some(own)) => {
            return Err(refused(format!(
                "nodata {v} disagrees with the raster's own {own}"
            )))
        }
    }
    Ok(())
}

impl RasterFile {
    /// Read a file with the default options. The file's name matters: Vortex takes the variable,
    /// units and time window from it.
    pub fn open(path: &Path) -> Result<Self, VortexError> {
        Self::open_with(path, ReadOptions::default())
    }

    /// Read a file.
    ///
    /// A sidecar GDAL would apply and this reader does not read is refused, so a mask, CRS or
    /// georeference cannot differ from GDAL's without anyone saying so; see [`applied_sidecar`].
    /// [`RasterFile::from_bytes`] has no folder to look in and cannot see them.
    ///
    /// A GeoTIFF's `<name>.aux.xml` is read under the rule a PRISM ZIP's member gets
    /// ([`apply_pam_nodata`]). One that states only nodata and statistics is applied. One that
    /// states anything else (an SRS, a geotransform, a mask, scale or offset) is refused. An ASCII
    /// grid's is refused as before.
    pub fn open_with(path: &Path, options: ReadOptions) -> Result<Self, VortexError> {
        let file_name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let size = std::fs::metadata(path)?.len();
        if size > options.limits.max_input_bytes {
            return Err(too_large(size, &options.limits));
        }
        // Not named .asc: a GeoTIFF, or refused by `from_bytes` whatever its sidecars say.
        let read_pam = !is_asc_name(&file_name);
        if let Some((found, why)) = applied_sidecar(path, false, read_pam)? {
            return Err(VortexError::Refused {
                what: format!("raster with a {found}"),
                reason: format!("{why}, and this reader does not read it"),
            });
        }
        let pam = match read_pam.then(|| pam_sidecar(path)).transpose()?.flatten() {
            Some(side) => {
                let side_name = side
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                let side_size = std::fs::metadata(&side)?.len();
                if side_size > super::bil::MAX_SIDECAR {
                    return Err(VortexError::Refused {
                        what: format!("raster with a .aux.xml sidecar, {side_name}"),
                        reason: format!(
                            "{side_size} bytes is over the sidecar limit of {}",
                            super::bil::MAX_SIDECAR
                        ),
                    });
                }
                let what = format!("raster with a .aux.xml sidecar, {side_name}");
                let xml = super::archive::read_file_bounded(&side, super::bil::MAX_SIDECAR, &what)?;
                Some((side_name, xml))
            }
            None => None,
        };
        // Bounded by the bytes read, not the size the file system states.
        let data =
            super::archive::read_file_bounded(path, options.limits.max_input_bytes, "raster file")?;
        let prj = if is_asc_name(&file_name) {
            asc::read_prj(path)?
        } else {
            None
        };
        let file = Self::from_bytes(file_name, &data, prj.as_deref(), options)?;
        match pam {
            Some((side_name, xml)) => file.with_pam(&xml, &side_name),
            None => Ok(file),
        }
    }

    /// This raster with a PAM `.aux.xml` applied by [`apply_pam_nodata`]; `what` names the sidecar
    /// in a refusal.
    pub(crate) fn with_pam(self, xml: &[u8], what: &str) -> Result<Self, VortexError> {
        let RasterFile {
            format,
            frame,
            crs_status,
            stored_wkt,
        } = self;
        let mut parts = frame.into_parts();
        apply_pam_nodata(&mut parts, xml, what)?;
        Ok(RasterFile {
            format,
            frame: GridFrame::new(parts)?,
            crs_status,
            stored_wkt,
        })
    }

    /// Decode bytes. `prj` is the text of an ASCII grid's `.prj`; a GeoTIFF ignores it.
    pub fn from_bytes(
        file_name: impl Into<String>,
        data: &[u8],
        prj: Option<&[u8]>,
        options: ReadOptions,
    ) -> Result<Self, VortexError> {
        let file_name = file_name.into();
        if data.len() as u64 > options.limits.max_input_bytes {
            return Err(too_large(data.len() as u64, &options.limits));
        }
        let (format, decoded) = if geotiff::has_magic(data) {
            if is_asc_name(&file_name) {
                return Err(VortexError::Refused {
                    what: "raster format".to_string(),
                    reason: format!("{file_name} is named as an ASCII grid and holds a TIFF"),
                });
            }
            (
                RasterFormat::GeoTiff,
                geotiff::decode(data, &options.limits)?,
            )
        } else if is_asc_name(&file_name) {
            (RasterFormat::Asc, asc::decode(data, prj, &options.limits)?)
        } else {
            return Err(VortexError::Unsupported {
                what: format!("{file_name}: neither a TIFF nor named .asc"),
            });
        };
        let meta = names::describe(&file_name)?;
        let (crs, crs_status) = match decoded.crs {
            CrsResolution::Stated(crs) => (crs, CrsStatus::Stated),
            CrsResolution::NotInFile => (Crs::Undefined, CrsStatus::NotInFile),
            CrsResolution::Unresolved(reason) => match options.unknown_crs {
                UnknownCrs::Refuse => {
                    return Err(VortexError::Refused {
                        what: "raster CRS".to_string(),
                        reason,
                    })
                }
                UnknownCrs::Undefined => (Crs::Undefined, CrsStatus::Unresolved { reason }),
            },
        };
        let t = decoded.transform;
        let frame = GridFrame::new(GridFrameParts {
            cells: decoded.cells,
            nx: decoded.nx,
            ny: decoded.ny,
            // A rotated transform has no row order of its own; the frame does not check this one.
            row_order: if t.dy < 0.0 {
                RowOrder::NorthToSouth
            } else {
                RowOrder::SouthToNorth
            },
            transform: t,
            crs,
            nodata: decoded.nodata,
            time: meta.time,
            data_type: meta.data_type,
            units: meta.units,
            variable: meta.short_name,
            description: meta.description,
        })?;
        Ok(RasterFile {
            format,
            frame,
            crs_status,
            stored_wkt: decoded.stored_wkt,
        })
    }

    pub fn format(&self) -> RasterFormat {
        self.format
    }

    pub fn frame(&self) -> &GridFrame {
        &self.frame
    }

    pub fn into_frame(self) -> GridFrame {
        self.frame
    }

    pub fn crs_status(&self) -> &CrsStatus {
        &self.crs_status
    }

    /// The CRS text a DSS record stores for this file, where it is not the frame's own.
    pub fn stored_wkt(&self) -> Option<&str> {
        self.stored_wkt.as_deref()
    }
}

fn is_asc_name(name: &str) -> bool {
    name.to_ascii_lowercase().ends_with(".asc")
}

fn too_large(size: u64, limits: &Limits) -> VortexError {
    VortexError::Refused {
        what: "raster file".to_string(),
        reason: format!(
            "{size} bytes is over the limit of {}",
            limits.max_input_bytes
        ),
    }
}

/// Allocate a zeroed buffer, fallibly.
pub(crate) fn alloc_zeroed<T: Copy + Default>(
    count: usize,
    format: &'static str,
) -> Result<Vec<T>, VortexError> {
    let mut out = Vec::new();
    out.try_reserve_exact(count)
        .map_err(|_| VortexError::Refused {
            what: format!("{format} grid"),
            reason: format!("could not allocate {count} values"),
        })?;
    out.resize(count, T::default());
    Ok(out)
}

/// A complete WKT1 `PROJCS` or `GEOGCS`, with what GDAL needs to build a CRS from it. It says
/// nothing about whether this library can transform it; `CrsDef::from_frame` decides that.
pub(crate) fn is_wkt1_crs(text: &str) -> bool {
    let trimmed = text.trim();
    (trimmed.starts_with("PROJCS[") || trimmed.starts_with("GEOGCS["))
        && crate::crs::wkt::wkt_facts(trimmed).is_ok()
}
