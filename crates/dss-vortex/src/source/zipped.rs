//! Rasters inside a ZIP archive, as PRISM distributes them: `*_bil.zip` and `*_asc.zip` (Vortex's
//! `io/BilZipDataReader` and `io/AscZipDataReader`), and PRISM's 2025 grid packages
//! (`prism_<var>_<region>_<res>_<date>.zip`), which hold a Cloud-Optimized GeoTIFF or, from the web
//! service's `format=bil`, a BIL.
//!
//! Vortex reads a ZIP through GDAL's `/vsizip/`: `ReadDir` lists the archive's root, and every
//! entry ending `.bil` (or `.asc`) is read as if it were a file of that name, with the archive's other
//! members as its siblings. This module does the same with the members in memory, through
//! [`super::bil::BilFile`] and [`super::raster::RasterFile`], and names each grid from its member's
//! name, as Vortex does.
//!
//! **Which members.** An archive named `*bil.zip` gives its `.bil` members and one named `*asc.zip`
//! its `.asc` members, as Vortex dispatches by name (`DataReader.DataReaderBuilder.build`). Any
//! other ZIP gives the one kind of raster it holds (`.bil`, `.asc`, or `.tif`/`.tiff`), and one with
//! several kinds is refused. Vortex does not read such a ZIP at all; this is how PRISM's current
//! packages, including its 30-year normals, import directly. Members are matched without regard to
//! letter case (Vortex: `.bil` and `.asc` only in lower case).
//!
//! **Sidecars inside the archive.** A `<member>.aux.xml` is read where it states only nodata and
//! statistics, which every PRISM package's does, and refused otherwise; `<member>.aux`,
//! `<stem>.aux` and `<member>.msk` are refused, as beside a file.

use super::archive::{self, ZipEntry};
use super::bil::{BilFile, BilNaming, BilParts, MAX_SIDECAR};
use super::raster::{apply_pam_nodata, RasterFile, ReadOptions};
use crate::error::VortexError;
use crate::frame::GridFrame;

/// The kind of raster a member is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ZippedKind {
    Bil,
    Asc,
    GeoTiff,
}

impl ZippedKind {
    fn of(name: &str) -> Option<Self> {
        let lower = name.to_ascii_lowercase();
        if lower.ends_with(".bil") {
            Some(ZippedKind::Bil)
        } else if lower.ends_with(".asc") {
            Some(ZippedKind::Asc)
        } else if lower.ends_with(".tif") || lower.ends_with(".tiff") {
            Some(ZippedKind::GeoTiff)
        } else {
            None
        }
    }
}

/// One raster read from an archive.
#[derive(Debug, Clone)]
pub struct ZippedRaster {
    pub member: String,
    pub kind: ZippedKind,
    pub frame: GridFrame,
    /// `VortexGrid.shortName`; empty where the member's name states no variable.
    pub short_name: String,
    /// The CRS text a DSS record stores, where it is not the frame's own (GeoTIFF only).
    pub stored_wkt: Option<String>,
    /// How a BIL's name was read; `None` for the other kinds.
    pub naming: Option<BilNaming>,
}

fn refused(what: impl Into<String>, reason: impl Into<String>) -> VortexError {
    VortexError::Refused {
        what: what.into(),
        reason: reason.into(),
    }
}

/// The kind an archive gives, by its name and then by what it holds.
fn chosen_kind(file_name: &str, members: &[&ZipEntry]) -> Result<ZippedKind, VortexError> {
    let lower = file_name.to_ascii_lowercase();
    if lower.len() > 7 && lower.ends_with("bil.zip") {
        return Ok(ZippedKind::Bil);
    }
    if lower.len() > 7 && lower.ends_with("asc.zip") {
        return Ok(ZippedKind::Asc);
    }
    let mut kinds: Vec<ZippedKind> = members
        .iter()
        .filter_map(|e| ZippedKind::of(&e.name))
        .collect();
    kinds.dedup();
    kinds.sort_by_key(|k| *k as u8);
    kinds.dedup();
    match kinds.as_slice() {
        [one] => Ok(*one),
        [] => Err(refused(
            format!("ZIP archive {file_name}"),
            "it holds no .bil, .asc or .tif in its root folder",
        )),
        _ => Err(refused(
            format!("ZIP archive {file_name}"),
            "it holds more than one kind of raster; name it *bil.zip or *asc.zip to choose",
        )),
    }
}

/// What one archive may cost, over all its rasters. Each raster also has the per-grid bounds of
/// [`ReadOptions::limits`]; these bound what the archive holds together, because every raster read
/// from it is kept until the import is planned. The importer's are in
/// [`crate::import::ImportLimits`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ArchiveBudget {
    /// Most bytes of decoded cells (4 per cell) kept from one archive. Checked before each raster
    /// is decoded: a raster may hold only the cells the rest of the budget leaves.
    pub max_decoded_bytes: u64,
    /// Most rasters read from one archive, checked from the central directory before any is read.
    pub max_rasters: usize,
}

impl Default for ArchiveBudget {
    /// 1 GiB of cells (2^28 cells, four of the importer's largest source grids) and 256 rasters.
    /// A PRISM package holds one raster, of at most 7 million cells at 800 m.
    fn default() -> Self {
        ArchiveBudget {
            max_decoded_bytes: 1 << 30,
            max_rasters: 256,
        }
    }
}

/// Read every raster of the chosen kind in the archive's root, in archive order, with the default
/// [`ArchiveBudget`] and no stop.
pub fn read_zip(
    file_name: &str,
    data: &[u8],
    options: ReadOptions,
) -> Result<Vec<ZippedRaster>, VortexError> {
    read_zip_with(file_name, data, options, ArchiveBudget::default(), None)
}

/// [`read_zip`] with an aggregate budget and the importer's stop flag.
///
/// Before anything is decompressed: the number of rasters is checked against
/// `budget.max_rasters`, and the members' stated sizes (which `archive::zip_read` holds each member
/// to) together against `options.limits.max_input_bytes`. Before each raster is decoded, its
/// decoder is given at most the cells the rest of `budget.max_decoded_bytes` leaves, so it refuses
/// a grid over the budget before allocating it. The stop is looked at before each member; a stopped
/// read is a refusal, which the importer reports as stopped.
pub fn read_zip_with(
    file_name: &str,
    data: &[u8],
    options: ReadOptions,
    budget: ArchiveBudget,
    stop: Option<&crate::import::StopFlag>,
) -> Result<Vec<ZippedRaster>, VortexError> {
    if data.len() as u64 > options.limits.max_input_bytes {
        return Err(refused(
            format!("ZIP archive {file_name}"),
            format!(
                "{} bytes is over the limit of {}",
                data.len(),
                options.limits.max_input_bytes
            ),
        ));
    }
    let entries = archive::zip_entries(data)?;
    let root: Vec<&ZipEntry> = entries.iter().filter(|e| e.in_root()).collect();
    let kind = chosen_kind(file_name, &root)?;
    let find = |name: &str| -> Option<&ZipEntry> {
        root.iter()
            .copied()
            .find(|e| e.name.eq_ignore_ascii_case(name))
    };
    let small = |e: &ZipEntry| archive::zip_read(data, e, MAX_SIDECAR);
    let members: Vec<&ZipEntry> = root
        .iter()
        .copied()
        .filter(|e| ZippedKind::of(&e.name) == Some(kind))
        .collect();
    let archive_refused = |reason: String| refused(format!("ZIP archive {file_name}"), reason);
    if members.len() > budget.max_rasters {
        return Err(archive_refused(format!(
            "{} rasters is over the limit of {} in one archive",
            members.len(),
            budget.max_rasters
        )));
    }
    let stated: u64 = members.iter().fold(0u64, |n, e| n.saturating_add(e.size));
    if stated > options.limits.max_input_bytes {
        return Err(archive_refused(format!(
            "its rasters hold {stated} bytes together, over the limit of {}",
            options.limits.max_input_bytes
        )));
    }
    let mut kept_bytes: u64 = 0;
    let mut out = Vec::new();
    for entry in members {
        if stop.is_some_and(|s| s.load(std::sync::atomic::Ordering::SeqCst)) {
            return Err(archive_refused("the read was stopped".to_string()));
        }
        let left_cells = (budget.max_decoded_bytes.saturating_sub(kept_bytes) / 4)
            .try_into()
            .unwrap_or(usize::MAX);
        if left_cells == 0 {
            return Err(archive_refused(format!(
                "its rasters decode to more than the limit of {} bytes in one archive",
                budget.max_decoded_bytes
            )));
        }
        let capped = left_cells < options.limits.max_cells;
        let mut options = options;
        options.limits.max_cells = options.limits.max_cells.min(left_cells);
        // A refusal under a cap the budget set says so, as the decoder's own text names only the cap.
        let over_budget = |e: VortexError| {
            match e {
            VortexError::Refused { what, reason } if capped => archive_refused(format!(
                "{what}: {reason}; the archive's limit of {} decoded bytes leaves {left_cells} cells \
                 for {}",
                budget.max_decoded_bytes, entry.name
            )),
            other => other,
        }
        };
        let name = entry.name.as_str();
        let stem = &name[..name.rfind('.').unwrap_or(name.len())];
        for (sidecar, why) in [
            (
                format!("{name}.aux"),
                "GDAL lets it override nodata, CRS and georeference",
            ),
            (
                format!("{stem}.aux"),
                "GDAL lets it override nodata, CRS and georeference",
            ),
            (
                format!("{name}.msk"),
                "GDAL takes the mask from it instead of from nodata",
            ),
        ] {
            if find(&sidecar).is_some() {
                return Err(refused(
                    format!("{name} in {file_name}"),
                    format!("{sidecar}: {why}, and this reader does not read it"),
                ));
            }
        }
        let aux = find(&format!("{name}.aux.xml")).map(small).transpose()?;
        let prj = find(&format!("{stem}.prj")).map(small).transpose()?;
        let bytes = archive::zip_read(data, entry, options.limits.max_input_bytes)?;
        let raster = match kind {
            ZippedKind::Bil => {
                let hdr = find(&format!("{stem}.hdr")).ok_or_else(|| {
                    refused(
                        format!("{name} in {file_name}"),
                        "no .hdr beside it in the archive",
                    )
                })?;
                let hdr = small(hdr)?;
                let bil = BilFile::from_parts(
                    name,
                    BilParts {
                        data: &bytes,
                        hdr: &hdr,
                        prj: prj.as_deref(),
                        aux_xml: aux.as_deref(),
                    },
                    options,
                )
                .map_err(over_budget)?;
                ZippedRaster {
                    member: name.to_string(),
                    kind,
                    short_name: bil.short_name().to_string(),
                    naming: Some(bil.naming()),
                    frame: bil.into_frame(),
                    stored_wkt: None,
                }
            }
            ZippedKind::Asc | ZippedKind::GeoTiff => {
                // A GeoTIFF ignores the .prj, as `RasterFile::from_bytes` does.
                let file = RasterFile::from_bytes(name, &bytes, prj.as_deref(), options)
                    .map_err(over_budget)?;
                let stored_wkt = file.stored_wkt().map(str::to_string);
                let mut parts = file.into_frame().into_parts();
                if let Some(xml) = &aux {
                    apply_pam_nodata(&mut parts, xml, &format!("{name}.aux.xml in {file_name}"))?;
                }
                let short_name = parts.variable.clone();
                parts.variable = super::bil::selection_name(&short_name, name);
                ZippedRaster {
                    member: name.to_string(),
                    kind,
                    frame: GridFrame::new(parts)?,
                    short_name,
                    stored_wkt,
                    naming: None,
                }
            }
        };
        kept_bytes = kept_bytes.saturating_add(4 * raster.frame.parts().cells.len() as u64);
        out.push(raster);
    }
    if out.is_empty() {
        return Err(refused(
            format!("ZIP archive {file_name}"),
            "no member of the kind its name gives",
        ));
    }
    Ok(out)
}
