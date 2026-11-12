//! SNODAS, the NOHRSC Snow Data Assimilation System, for HMS snowmelt: the masked CONUS grid of
//! 6,935 by 3,351 cells of 30 arc-seconds, as big-endian 16-bit integers in `.dat` files, each with a
//! NOHRSC text header (`.Hdr`, or `.txt` in NSIDC's archive), delivered daily as
//! `SNODAS_YYYYMMDD.tar` of gzip-compressed members.
//!
//! Read as Vortex 1.1.0 and 0.13.3 read it (`io/SnodasTarDataReader`, `io/SnodasDataReader`):
//! the member's file name gives the product, its name, units, scale and time; the grid is placed by
//! `gdal_translate -a_srs "+proj=longlat +ellps=WGS84 +datum=WGS84 +no_defs" -a_ullr -124.73333333
//! 52.87500000 -66.94166667 24.95000000`, whose geotransform is `apps/gdal_translate_lib.cpp`'s
//! `(lrx - ulx) / nx` and `(lry - uly) / ny`; and each cell is divided by the scale in `float`.
//!
//! **Where this reader differs from Vortex, by default:** a cell of `-9999`, the header's no-data
//! value, is missing. Vortex sets no nodata on these grids (`VortexGrid`'s is NaN), divides `-9999`
//! by the scale like any cell, and so hands on `-9999`, `-999.9` or `-0.09999` as values.
//!
//! The missing cells stay missing through the warp and are stored as DSS missing values, so the
//! record's statistics leave them out. Real negative values, such as sublimation, are kept.
//!
//! The grid's 23,239,185 cells are over the writer's 16,777,216-cell limit, so an import is
//! warped and clipped to the model's envelope, with a buffer of a few destination cells.
//!
//! **Times and data types.** By default ([`SnodasTimes::Header`]) a grid's time comes from its
//! NOHRSC header, checked against the file name, and its data type comes from the product:
//! - a daily total (`1025` liquid and solid precipitation, `1044` melt, `1050` and `1039`
//!   sublimation; time code `T0024`) is PER-CUM over the 24 hours ending at 06:00 UTC of the file's
//!   date;
//! - SWE (`1034`) and snow depth (`1036`), time code `T0001`, are INST-VAL at 06:00 UTC of that date.
//!
//! The file name's timing fields are checked too, by the NOHRSC naming rules: a real date; `TS`, the
//! start of the final integration step, `05`; the step, `D` for a total and `H` for a snapshot; and
//! the offset, `P001` for precipitation, SWE and depth and `P000` for melt and sublimation, as the
//! public 2010 and 2020 tars pair them. A field with another value, a header that disagrees with
//! the name, and a member with no header are refused.
//!
//! These have no qualified interpretation and are refused unless the caller names one: the snowpack
//! average temperature (`1038`, `A0024`); melt with time code `T0001` (historical files, for
//! example 2010-02-15); and sublimation (`1039`, `1050`) whose header stops at 05:00, an hour before
//! the 24 hours its name means (the 2010-02-15 headers; an open item until qualified).
//! [`SnodasTimes::Vortex`] gives Vortex's timing and types for every product;
//! [`SnodasTimes::HeaderAsStated`] gives the header's `Start` and `Stop` as written.
//!
//! Vortex (`SnodasDataReader.getTimeSpan`) stamps the 24-hour totals from 06:00 of the file's date to
//! 06:00 the next day, one day after their headers, and `VortexGrid.inferDataType` types all of them
//! PER-AVER: none of the SNODAS names, `Liquid Precipitation` included, is one it calls
//! precipitation. It stamps the snowpack average temperature as an instant. Sources: NSIDC
//! SNODAS user guide (G02158) sections 1.2 and 1.8; NSIDC Special Report 11, appendix C; the headers
//! of the public 2010-02-15 and 2020-01-01 tars.
//!
//! **Corners, opt-in** ([`SnodasGeoreference::Header`]): the header's corner coordinates. Before 1
//! October 2013 they differ from Vortex's fixed corners by 0.0004167 degrees (a twentieth of a cell).
//!
//! Members are decompressed in memory. Nothing is written beside the input, as Vortex's tar reader
//! does (`<stem>_untar`, `<stem>_unzip` and an ENVI `.hdr` per member).

use super::archive;
use super::bil::infer_data_type;
use super::raster::{alloc_zeroed, names};
use crate::error::VortexError;
use crate::frame::{Crs, FrameTime, GeoTransform, GridFrame, GridFrameParts, RowOrder};

const FORMAT: &str = "SNODAS";
/// The masked CONUS grid, which Vortex's ENVI header fixes (`SnodasTarDataReader.createHeader`).
pub const NCOLS: usize = 6935;
pub const NROWS: usize = 3351;
/// `-a_ullr` in `SnodasDataReader.getDtos`.
const VORTEX_ULLR: [f64; 4] = [-124.73333333, 52.875, -66.94166667, 24.95];
/// `-a_nodata` there, and the header's `No data value`.
pub const NODATA: f32 = -9999.0;
/// What GDAL 3.2's `GetProjection` gives for `+proj=longlat +ellps=WGS84 +datum=WGS84 +no_defs`, as
/// Vortex 1.1.0 and 0.13.3 report it for these grids.
pub const WGS84_WKT: &str = "GEOGCS[\"unknown\",DATUM[\"WGS_1984\",SPHEROID[\"WGS 84\",6378137,298.257223563,AUTHORITY[\"EPSG\",\"7030\"]],AUTHORITY[\"EPSG\",\"6326\"]],PRIMEM[\"Greenwich\",0,AUTHORITY[\"EPSG\",\"8901\"]],UNIT[\"degree\",0.0174532925199433,AUTHORITY[\"EPSG\",\"9122\"]],AXIS[\"Longitude\",EAST],AXIS[\"Latitude\",NORTH]]";
/// Larger than any NOHRSC header.
const MAX_HEADER: u64 = 1 << 16;

fn refused(what: impl Into<String>, reason: impl Into<String>) -> VortexError {
    VortexError::Refused {
        what: format!("SNODAS {}", what.into()),
        reason: reason.into(),
    }
}

/// Where a grid's times and data type come from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SnodasTimes {
    /// The default: the header's times, checked against the file name and the product, with the
    /// product's data type (PER-CUM daily totals, INST-VAL SWE and depth). A member without a
    /// header, a header that disagrees, and the two ambiguous products (`1038`, melt `T0001`) are
    /// refused.
    #[default]
    Header,
    /// Vortex compatibility: `SnodasDataReader.getTimeSpan`, from the file name, typed by
    /// `VortexGrid.inferDataType`. Every product is read; no header is needed.
    Vortex,
    /// The header's `Start` and `Stop` exactly as written, typed as `VortexGrid.inferDataType`
    /// types a period or an instant. The explicit interpretation for the ambiguous products; the
    /// `"header"` mode of the first development version. A member without a header is refused.
    HeaderAsStated,
}

impl SnodasTimes {
    fn needs_header(self) -> bool {
        self != SnodasTimes::Vortex
    }
}

/// What a product is, for its time and data type under [`SnodasTimes::Header`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProductKind {
    /// A 24-hour total ending at 06:00 UTC: PER-CUM.
    DailyTotal,
    /// A 06:00 UTC state: INST-VAL.
    Snapshot,
    /// No qualified interpretation: `1038`, or a product whose time code is not its usual one.
    Ambiguous,
}

/// Where a grid's corners come from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SnodasGeoreference {
    /// Vortex's fixed corners.
    #[default]
    Vortex,
    /// The header's minimum and maximum coordinates. A member without a header is refused.
    Header,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SnodasOptions {
    pub times: SnodasTimes,
    pub georeference: SnodasGeoreference,
    /// Test-only: reproduce Vortex's cells, every one divided by the scale and no nodata (NaN), so
    /// a comparison with Vortex can show that the rest of an import equals Vortex's. Not a mode to import with.
    #[doc(hidden)]
    pub vortex_nodata: bool,
}

impl SnodasOptions {
    /// The marker the frames carry: -9999, or NaN for the test-only Vortex reproduction.
    pub fn marker(&self) -> f32 {
        if self.vortex_nodata {
            f32::NAN
        } else {
            NODATA
        }
    }
}

/// What a member's file name says, by `SnodasDataReader.parseFile`, `getName`, `getUnits`,
/// `getScaleFactor` and `getTimeSpan`.
#[derive(Debug, Clone, PartialEq)]
pub struct Product {
    /// The product code, e.g. `1034`.
    pub code: String,
    /// `getName`, which is the variable's short name, full name and description in Vortex.
    pub name: &'static str,
    pub units: &'static str,
    pub scale: f32,
    /// `getTimeSpan`.
    pub vortex_time: FrameTime,
    /// The file name's time code (`T0024`, `T0001`, `A0024`), where the name is long enough.
    pub time_code: Option<String>,
    /// The file name's hour (`TS`), step length (`D` or `H`) and offset (`P000` or `P001`), where
    /// the name is long enough and they are of the right characters.
    pub hour: Option<String>,
    pub step: Option<String>,
    pub offset: Option<String>,
    /// Whether the name's date is a real calendar date (Vortex's parser clamps 31 February).
    pub date_exact: bool,
    /// The product's kind under the default timing, from its code and time code.
    pub kind: ProductKind,
    /// 06:00 UTC of the file name's date.
    pub day: (i64, i64),
}

/// The time and date fields of a name, as `parseFile` cuts them.
fn cut(name: &str, from: usize, to: usize) -> Result<&str, VortexError> {
    name.get(from..to)
        .ok_or_else(|| refused(format!("name {name}"), "too short for SNODAS's fields"))
}

pub fn product(dat_name: &str) -> Result<Product, VortexError> {
    if !dat_name.is_ascii() {
        return Err(refused(format!("name {dat_name}"), "not ASCII"));
    }
    let product = cut(dat_name, 8, 12)?;
    // parseFile: with no `S` at [12, 13), the fields after it start one character earlier.
    let shift = usize::from(cut(dat_name, 12, 13)? != "S");
    let d_type = cut(dat_name, 13 - shift, 17 - shift)?;
    let date = cut(dat_name, 28 - shift, 36 - shift)?;
    let hour = cut(dat_name, 36 - shift, 38 - shift)?;
    let name = match (product, d_type) {
        ("1034", _) => "SWE",
        ("1036", _) => "Snow Depth",
        ("1044", _) => "Snow Melt Runoff at the Base of the Snow Pack",
        ("1050", _) => "Sublimation from the Snow Pack",
        ("1039", _) => "Sublimation of Blowing Snow",
        ("1025", "lL01") => "Solid Precipitation",
        ("1025", "lL00") => "Liquid Precipitation",
        ("1038", _) => "Snow Pack Average Temperature",
        _ => {
            return Err(refused(
                format!("product {product}"),
                "not one Vortex names (\"Error: Invalid product code\")",
            ))
        }
    };
    let units = match product {
        "1034" | "1036" => "mm",
        "1044" | "1050" | "1039" => "m",
        "1025" => "kg/m^2",
        _ => "k",
    };
    let scale = match product {
        "1044" | "1050" | "1039" => 100_000.0,
        "1025" => 10.0,
        _ => 1.0,
    };
    // LocalDate.parse(date, "yyyyMMdd"), SMART.
    if !date.bytes().all(|b| b.is_ascii_digit()) {
        return Err(refused(format!("name {dat_name}"), format!("date {date}")));
    }
    let day = names::time_of(
        dat_name,
        names::number(&date[..4]),
        names::number(&date[4..6]),
        names::number(&date[6..8]),
        6,
        0,
    )?;
    let vortex_time = match product {
        "1044" | "1050" | "1039" | "1025" => FrameTime::Period {
            start: day,
            end: (day.0 + 1, day.1),
        },
        _ => FrameTime::Instant(day),
    };
    // Not cut by Vortex: the time code after the product's type, `T0024` in
    // `us_ssmv11039lL00T0024TTNATS2020010105DP000` (NSIDC Special Report 11, appendix C).
    let time_code = dat_name
        .get(17 - shift..22 - shift)
        .filter(|c| c.bytes().all(|b| b.is_ascii_alphanumeric()))
        .map(str::to_string);
    // Not cut by Vortex either: after the date and the hour (`TS`, the start of the final
    // integration step), the step's length (`D` daily, `H` hourly) and the offset (`P000` data at
    // the start of the step, `P001` at its end). NSIDC SNODAS user guide (G02158), table 4; NSIDC
    // Special Report 11, appendix C.
    let field = |from: usize, to: usize| {
        dat_name
            .get(from - shift..to - shift)
            .filter(|c| c.bytes().all(|b| b.is_ascii_alphanumeric()))
            .map(str::to_string)
    };
    let step = field(38, 39);
    let offset = field(39, 43);
    let hour = Some(hour.to_string()).filter(|h| h.bytes().all(|b| b.is_ascii_digit()));
    // A date `time_of` clamped (31 February) is not the date the name states.
    let date_exact = names::julian(
        names::number(&date[..4]),
        names::number(&date[4..6]),
        names::number(&date[6..8]),
    ) == day.0;
    let kind = match (product, time_code.as_deref()) {
        ("1025" | "1044" | "1050" | "1039", Some("T0024")) => ProductKind::DailyTotal,
        ("1034" | "1036", Some("T0001")) => ProductKind::Snapshot,
        _ => ProductKind::Ambiguous,
    };
    Ok(Product {
        code: product.to_string(),
        name,
        units,
        scale,
        vortex_time,
        time_code,
        hour,
        step,
        offset,
        date_exact,
        kind,
        day,
    })
}

/// A NOHRSC GIS/RS raster header, the facts this reader checks or uses.
#[derive(Debug, Clone, PartialEq)]
pub struct Header {
    pub columns: usize,
    pub rows: usize,
    pub min_x: f64,
    pub max_x: f64,
    pub min_y: f64,
    pub max_y: f64,
    pub start: (i64, i64),
    pub stop: (i64, i64),
}

fn header_text(bytes: &[u8]) -> Result<Vec<(String, String)>, VortexError> {
    if bytes.len() as u64 > MAX_HEADER {
        return Err(refused("header", format!("{} bytes", bytes.len())));
    }
    let text = std::str::from_utf8(bytes).map_err(|_| refused("header", "not text"))?;
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim_end_matches('\r');
        if line.trim().is_empty() {
            continue;
        }
        let (key, value) = line
            .split_once(':')
            .ok_or_else(|| refused("header", format!("line {line:?} has no colon")))?;
        out.push((key.trim().to_string(), value.trim().to_string()));
    }
    Ok(out)
}

pub fn parse_header(bytes: &[u8]) -> Result<Header, VortexError> {
    let fields = header_text(bytes)?;
    let get = |key: &str| -> Result<&str, VortexError> {
        let mut found = fields.iter().filter(|(k, _)| k == key);
        let first = found
            .next()
            .ok_or_else(|| refused("header", format!("no {key}")))?;
        if found.next().is_some() {
            return Err(refused("header", format!("{key} is stated twice")));
        }
        Ok(first.1.as_str())
    };
    let real = |key: &str| -> Result<f64, VortexError> {
        let v = get(key)?;
        v.parse::<f64>()
            .ok()
            .filter(|x| x.is_finite())
            .ok_or_else(|| refused("header", format!("{key} {v}")))
    };
    let int = |key: &str| -> Result<i64, VortexError> {
        let v = get(key)?;
        v.parse::<i64>()
            .map_err(|_| refused("header", format!("{key} {v}")))
    };
    if get("Format version")? != "NOHRSC GIS/RS raster file v1.1" {
        return Err(refused("header", "not a NOHRSC GIS/RS raster file v1.1"));
    }
    // What Vortex's ENVI header assumes; a header saying otherwise would be read wrongly by it.
    let expect = |key: &str, ok: bool| -> Result<(), VortexError> {
        if ok {
            Ok(())
        } else {
            Err(refused(
                "header",
                format!("{key} is not what Vortex's fixed reading assumes"),
            ))
        }
    };
    expect("Data type", get("Data type")? == "integer")?;
    expect("Data bytes per pixel", int("Data bytes per pixel")? == 2)?;
    expect("Data slope", real("Data slope")? == 1.0)?;
    expect("Data intercept", real("Data intercept")? == 0.0)?;
    expect("No data value", real("No data value")? == f64::from(NODATA))?;
    // Optional, but at most once: a repeated key is refused, as for every other key,
    // where `get`'s error would otherwise read as an absent key.
    match fields.iter().filter(|(k, _)| k == "Compressed").count() {
        0 => {}
        1 => expect("Compressed", get("Compressed")? == "no")?,
        _ => return Err(refused("header", "Compressed is stated twice")),
    }
    let columns = int("Number of columns")?;
    let rows = int("Number of rows")?;
    if (columns, rows) != (NCOLS as i64, NROWS as i64) {
        return Err(refused(
            "grid",
            format!(
                "{columns} by {rows} cells; Vortex reads the masked {NCOLS} by {NROWS} grid only"
            ),
        ));
    }
    let when = |prefix: &str| -> Result<(i64, i64), VortexError> {
        let f = |part: &str| int(&format!("{prefix} {part}"));
        let (y, mo, d, h, mi, s) = (
            f("year")?,
            f("month")?,
            f("day")?,
            f("hour")?,
            f("minute")?,
            f("second")?,
        );
        // Every field in its calendar range before any arithmetic: a header is untrusted text, and
        // `time_of` also bounds the year, so nothing below can overflow.
        if s != 0
            || !(1..=names::MAX_YEAR).contains(&y)
            || !(1..=12).contains(&mo)
            || !(1..=31).contains(&d)
            || !(0..=23).contains(&h)
            || !(0..=59).contains(&mi)
        {
            return Err(refused("header", format!("{prefix} time")));
        }
        // time_of clamps a day past the month's end, as Java's SMART resolver does; a header must
        // state a real date, so a clamped day is refused.
        let t = names::time_of(prefix, y, mo, d, h, mi)?;
        if names::julian(y, mo, d) != t.0 {
            return Err(refused("header", format!("{prefix} date {y}-{mo}-{d}")));
        }
        Ok(t)
    };
    // Corners in degrees, ordered, so a header-based transform (`SnodasGeoreference::Header`) is
    // finite and north-up. Checked in every mode: a header that fails is malformed, whichever
    // corners the caller asked for.
    let (min_x, max_x) = (
        real("Minimum x-axis coordinate")?,
        real("Maximum x-axis coordinate")?,
    );
    let (min_y, max_y) = (
        real("Minimum y-axis coordinate")?,
        real("Maximum y-axis coordinate")?,
    );
    if !((-180.0..=180.0).contains(&min_x)
        && (-180.0..=180.0).contains(&max_x)
        && (-90.0..=90.0).contains(&min_y)
        && (-90.0..=90.0).contains(&max_y)
        && min_x < max_x
        && min_y < max_y)
    {
        return Err(refused(
            "header",
            format!("corners x {min_x}..{max_x}, y {min_y}..{max_y} are not ordered degrees"),
        ));
    }
    Ok(Header {
        columns: columns as usize,
        rows: rows as usize,
        min_x,
        max_x,
        min_y,
        max_y,
        start: when("Start")?,
        stop: when("Stop")?,
    })
}

/// One SNODAS grid: its product, and its header where one came with it.
#[derive(Debug, Clone)]
pub struct Grid {
    pub dat_name: String,
    pub product: Product,
    pub header: Option<Header>,
}

/// How a refusal names the options that read every product.
const EXPLICIT: &str = "to import it, name an interpretation: snodas_times=\"vortex\" \
     (SnodasTimes::Vortex: Vortex's file-name times and types) or \
     snodas_times=\"header_as_stated\" (SnodasTimes::HeaderAsStated: the header's Start and Stop \
     as written)";

/// A (day since 1899-12-31, second) time as `YYYY-MM-DD HH:MM UTC`, for refusals.
fn show(t: (i64, i64)) -> String {
    // civil_from_days (H. Hinnant), from 1970-01-01, which is day 25,568 here.
    let z = t.0 - 25_568 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02} UTC",
        t.1 / 3600,
        t.1 % 3600 / 60
    )
}

impl Grid {
    fn header_for(&self, what: &str) -> Result<&Header, VortexError> {
        self.header.as_ref().ok_or_else(|| {
            refused(
                &self.dat_name,
                format!("{what} were asked for and it has no NOHRSC header"),
            )
        })
    }

    /// The grid's time and data type under `options`.
    pub fn time_and_type(
        &self,
        options: SnodasOptions,
    ) -> Result<(FrameTime, crate::frame::DataType), VortexError> {
        use crate::frame::DataType;
        let p = &self.product;
        match options.times {
            SnodasTimes::Vortex => Ok((p.vortex_time, infer_data_type(&p.vortex_time, p.name))),
            SnodasTimes::HeaderAsStated => {
                let h = self.header_for("header times")?;
                let time = if h.start == h.stop {
                    FrameTime::Instant(h.start)
                } else {
                    FrameTime::Period {
                        start: h.start,
                        end: h.stop,
                    }
                };
                Ok((time, infer_data_type(&time, p.name)))
            }
            SnodasTimes::Header => {
                let code = p.time_code.as_deref().unwrap_or("none");
                if p.kind == ProductKind::Ambiguous {
                    return Err(refused(
                        &self.dat_name,
                        format!(
                            "product {} ({}) with time code {code} has no qualified time and \
                             data-type interpretation (snodas_times=\"header\", the default); {EXPLICIT}",
                            p.code, p.name
                        ),
                    ));
                }
                self.check_name_fields()?;
                let h = self.header.as_ref().ok_or_else(|| {
                    refused(
                        &self.dat_name,
                        "it has no NOHRSC header, and the default timing (snodas_times=\"header\") takes its times \
                         from one. snodas_times=\"vortex\" (SnodasTimes::Vortex) needs no header",
                    )
                })?;
                // The name's final integration step starts at 05:00 (`check_name_fields`) and
                // ends at 06:00 UTC of its date.
                let six = p.day;
                let (time, data_type) = match p.kind {
                    ProductKind::Snapshot => (FrameTime::Instant(six), DataType::InstVal),
                    _ => (
                        FrameTime::Period {
                            start: (six.0 - 1, six.1),
                            end: six,
                        },
                        DataType::PerCum,
                    ),
                };
                let (want_start, want_stop) = match time {
                    FrameTime::Period { start, end } => (start, end),
                    _ => (six, six),
                };
                if (h.start, h.stop) == (want_start, want_stop) {
                    return Ok((time, data_type));
                }
                // The 2010-02-15 sublimation headers (`1039`, `1050`, `P000`) state 05:00, the
                // start of the last hourly step, as the stop; the 2020-01-01 ones state 06:00.
                // The sources do not settle what the historical stop means, so it is an
                // ambiguous product until qualified.
                if matches!(p.code.as_str(), "1039" | "1050")
                    && h.start == want_start
                    && h.stop == (six.0, six.1 - 3600)
                {
                    return Err(refused(
                        &self.dat_name,
                        format!(
                            "product {} ({}) has a header stopping at {}, an hour before the \
                             24 hours its file name means ({} to {}). This historical \
                             sublimation header convention is not qualified (snodas_times=\"header\", the default; an open item); \
                             {EXPLICIT}",
                            p.code,
                            p.name,
                            show(h.stop),
                            show(want_start),
                            show(want_stop)
                        ),
                    ));
                }
                Err(refused(
                    &self.dat_name,
                    format!(
                        "its header states {} to {}, where its file name ({} {code}) means {} to \
                         {} (snodas_times=\"header\", the default); {EXPLICIT}",
                        show(h.start),
                        show(h.stop),
                        p.code,
                        show(want_start),
                        show(want_stop)
                    ),
                ))
            }
        }
    }

    /// The file name's timing fields under the default timing, as the NOHRSC naming rules
    /// state them (NSIDC SNODAS user guide G02158, table 4; Special Report 11, appendix C): the
    /// date, a real one; `TS`, the start of the final integration step, 05 (so the product's time
    /// is 06:00 UTC); the step, `D` for a daily total and `H` for a snapshot; and the offset,
    /// `P001` for precipitation and the snapshots and `P000` for melt and sublimation, the
    /// pairings of the public 2010 and 2020 tars. Any other value is refused as unqualified.
    fn check_name_fields(&self) -> Result<(), VortexError> {
        let p = &self.product;
        let (step, offset) = match (p.kind, p.code.as_str()) {
            (ProductKind::Snapshot, _) => ("H", "P001"),
            (_, "1025") => ("D", "P001"),
            _ => ("D", "P000"),
        };
        let fields = [
            ("date", p.date_exact.then_some("real"), "real"),
            ("hour (TS)", p.hour.as_deref(), "05"),
            ("step", p.step.as_deref(), step),
            ("offset", p.offset.as_deref(), offset),
        ];
        for (what, got, want) in fields {
            if got != Some(want) {
                return Err(refused(
                    &self.dat_name,
                    format!(
                        "its file name's {what} is {}, where a qualified {} product ({}) has \
                         {want} (snodas_times=\"header\", the default); {EXPLICIT}",
                        got.unwrap_or("missing or malformed"),
                        p.code,
                        p.name
                    ),
                ));
            }
        }
        Ok(())
    }

    pub fn time(&self, options: SnodasOptions) -> Result<FrameTime, VortexError> {
        Ok(self.time_and_type(options)?.0)
    }

    pub fn transform(&self, options: SnodasOptions) -> Result<GeoTransform, VortexError> {
        let [ulx, uly, lrx, lry] = match options.georeference {
            SnodasGeoreference::Vortex => VORTEX_ULLR,
            SnodasGeoreference::Header => {
                let h = self.header.as_ref().ok_or_else(|| {
                    refused(
                        &self.dat_name,
                        "header corners were asked for and it has no header",
                    )
                })?;
                [h.min_x, h.max_y, h.max_x, h.min_y]
            }
        };
        Ok(GeoTransform::axis_aligned(
            ulx,
            (lrx - ulx) / NCOLS as f64,
            uly,
            (lry - uly) / NROWS as f64,
        ))
    }

    pub fn data_type(&self, options: SnodasOptions) -> Result<crate::frame::DataType, VortexError> {
        Ok(self.time_and_type(options)?.1)
    }

    /// Decode the `.dat` bytes, already decompressed.
    pub fn decode(&self, dat: &[u8], options: SnodasOptions) -> Result<GridFrame, VortexError> {
        let cells = NCOLS * NROWS;
        if dat.len() != cells * 2 {
            return Err(refused(
                &self.dat_name,
                format!("{} bytes, where the grid is {}", dat.len(), cells * 2),
            ));
        }
        let mut out: Vec<f32> = alloc_zeroed(cells, FORMAT)?;
        let scale = self.product.scale;
        for (o, c) in out.iter_mut().zip(dat.as_chunks::<2>().0) {
            let raw = f32::from(i16::from_be_bytes(*c));
            *o = if raw == NODATA && !options.vortex_nodata {
                NODATA
            } else {
                raw / scale
            };
        }
        let (time, data_type) = self.time_and_type(options)?;
        Ok(GridFrame::new(GridFrameParts {
            cells: out,
            nx: NCOLS,
            ny: NROWS,
            row_order: RowOrder::NorthToSouth,
            transform: self.transform(options)?,
            crs: Crs::Wkt(WGS84_WKT.to_string()),
            nodata: Some(options.marker()),
            data_type,
            time,
            units: self.product.units.to_string(),
            variable: self.product.name.to_string(),
            description: Some(self.product.name.to_string()),
        })?)
    }
}

/// A member's bytes: borrowed from a tar, or owned.
enum Bytes {
    Tar(archive::TarEntry),
    Owned(Vec<u8>),
}

struct Member {
    grid: Grid,
    dat: Bytes,
    gzipped: bool,
}

/// A daily tar, or one `.dat` (or `.dat.gz`) with its header beside it.
pub struct SnodasFile {
    file_name: String,
    data: Vec<u8>,
    members: Vec<Member>,
    options: SnodasOptions,
    max_bytes: u64,
}

/// The header that goes with a member: `<stem>.Hdr` or `<stem>.txt`, gzipped or not.
fn header_names(stem: &str) -> [String; 4] {
    [
        format!("{stem}.Hdr"),
        format!("{stem}.txt"),
        format!("{stem}.Hdr.gz"),
        format!("{stem}.txt.gz"),
    ]
}

fn dat_stem(name: &str) -> Option<(&str, bool)> {
    let lower = name.to_ascii_lowercase();
    if lower.ends_with(".dat.gz") {
        Some((&name[..name.len() - 7], true))
    } else if lower.ends_with(".dat") {
        Some((&name[..name.len() - 4], false))
    } else {
        None
    }
}

/// Whether a file name is one SNODAS member's, `<region>_ssmv...`.
pub fn is_snodas_name(name: &str) -> bool {
    name.get(2..7)
        .is_some_and(|m| m.eq_ignore_ascii_case("_ssmv"))
        && dat_stem(name).is_some()
}

impl SnodasFile {
    /// Open a daily tar from its bytes.
    pub fn from_tar(
        file_name: impl Into<String>,
        data: Vec<u8>,
        options: SnodasOptions,
        max_bytes: u64,
    ) -> Result<Self, VortexError> {
        let file_name = file_name.into();
        let entries = archive::tar_entries(&data)?;
        let mut members = Vec::new();
        for entry in &entries {
            if entry.name.contains('/') {
                return Err(refused(
                    format!("tar {file_name}"),
                    format!("member {} is in a folder", entry.name),
                ));
            }
            let Some((stem, gzipped)) = dat_stem(&entry.name) else {
                continue;
            };
            let dat_name = format!("{stem}.dat");
            let product = product(&dat_name)?;
            let mut header = None;
            for wanted in header_names(stem) {
                if let Some(h) = entries.iter().find(|e| e.name == wanted) {
                    if header.is_some() {
                        return Err(refused(&dat_name, "more than one header in the tar"));
                    }
                    let raw = archive::tar_read(&data, h);
                    // Not copied: `parse_header` refuses one over MAX_HEADER before anything is
                    // allocated for it.
                    header = Some(if wanted.ends_with(".gz") {
                        parse_header(&archive::gunzip(raw, MAX_HEADER)?)?
                    } else {
                        parse_header(raw)?
                    });
                }
            }
            members.push(Member {
                grid: Grid {
                    dat_name,
                    product,
                    header,
                },
                dat: Bytes::Tar(entry.clone()),
                gzipped,
            });
        }
        Self::finish(file_name, data, members, options, max_bytes)
    }

    /// One member from its own bytes, with its header text if there is one.
    pub fn from_member(
        file_name: impl Into<String>,
        dat: Vec<u8>,
        header: Option<&[u8]>,
        options: SnodasOptions,
        max_bytes: u64,
    ) -> Result<Self, VortexError> {
        let file_name = file_name.into();
        let (stem, gzipped) = dat_stem(&file_name)
            .ok_or_else(|| refused(format!("file {file_name}"), "not named .dat or .dat.gz"))?;
        let dat_name = format!("{stem}.dat");
        let header = match header {
            Some(bytes) if archive::is_gzip(bytes) => {
                Some(parse_header(&archive::gunzip(bytes, MAX_HEADER)?)?)
            }
            Some(bytes) => Some(parse_header(bytes)?),
            None => None,
        };
        let member = Member {
            grid: Grid {
                product: product(&dat_name)?,
                dat_name,
                header,
            },
            dat: Bytes::Owned(dat),
            gzipped,
        };
        Self::finish(file_name, Vec::new(), vec![member], options, max_bytes)
    }

    fn finish(
        file_name: String,
        data: Vec<u8>,
        members: Vec<Member>,
        options: SnodasOptions,
        max_bytes: u64,
    ) -> Result<Self, VortexError> {
        if members.is_empty() {
            return Err(refused(format!("file {file_name}"), "no .dat member"));
        }
        for (i, m) in members.iter().enumerate() {
            if members[..i]
                .iter()
                .any(|o| o.grid.product.name == m.grid.product.name)
            {
                return Err(refused(
                    format!("file {file_name}"),
                    format!("two members give {}", m.grid.product.name),
                ));
            }
            // Fail at opening, not at writing, if an option needs a header this member lacks. A
            // product the default timing refuses is refused when it is selected, so the
            // other products of its tar stay readable.
            if options.times.needs_header() && m.grid.header.is_none() {
                m.grid.time(options)?;
            }
            m.grid.transform(options)?;
        }
        Ok(SnodasFile {
            file_name,
            data,
            members,
            options,
            max_bytes,
        })
    }

    pub fn file_name(&self) -> &str {
        &self.file_name
    }

    /// Variables in archive order: `getName` of each member.
    pub fn variables(&self) -> Vec<String> {
        self.members
            .iter()
            .map(|m| m.grid.product.name.to_string())
            .collect()
    }

    pub fn grid(&self, variable: &str) -> Result<&Grid, VortexError> {
        self.members
            .iter()
            .find(|m| m.grid.product.name == variable)
            .map(|m| &m.grid)
            .ok_or_else(|| {
                refused(
                    format!("variable {variable}"),
                    format!("not in {}", self.file_name),
                )
            })
    }

    pub fn options(&self) -> SnodasOptions {
        self.options
    }

    pub fn frame(&self, variable: &str) -> Result<GridFrame, VortexError> {
        let member = self
            .members
            .iter()
            .find(|m| m.grid.product.name == variable)
            .ok_or_else(|| {
                refused(
                    format!("variable {variable}"),
                    format!("not in {}", self.file_name),
                )
            })?;
        let raw: &[u8] = match &member.dat {
            Bytes::Tar(e) => archive::tar_read(&self.data, e),
            Bytes::Owned(v) => v,
        };
        let limit = self.max_bytes.min((NCOLS * NROWS * 2) as u64);
        let unzipped;
        let dat = if member.gzipped {
            unzipped = archive::gunzip(raw, limit)?;
            &unzipped[..]
        } else {
            raw
        };
        member.grid.decode(dat, self.options)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use names::julian;

    #[test]
    fn names_as_parse_file_cuts_them() {
        let swe = product("us_ssmv11034tS__T0001TTNATS2020010105HP001.dat").unwrap();
        assert_eq!((swe.name, swe.units, swe.scale), ("SWE", "mm", 1.0));
        assert_eq!(
            swe.vortex_time,
            FrameTime::Instant((julian(2020, 1, 1), 21_600))
        );
        let liquid = product("us_ssmv01025SlL00T0024TTNATS2020010105DP001.dat").unwrap();
        assert_eq!(
            (liquid.name, liquid.units, liquid.scale),
            ("Liquid Precipitation", "kg/m^2", 10.0)
        );
        assert_eq!(
            liquid.vortex_time,
            FrameTime::Period {
                start: (julian(2020, 1, 1), 21_600),
                end: (julian(2020, 1, 2), 21_600)
            }
        );
        assert_eq!(
            product("us_ssmv01025SlL01T0024TTNATS2020010105DP001.dat")
                .unwrap()
                .name,
            "Solid Precipitation"
        );
        let melt = product("us_ssmv11044bS__T0024TTNATS2020010105DP000.dat").unwrap();
        assert_eq!((melt.units, melt.scale), ("m", 100_000.0));
        assert_eq!(
            product("us_ssmv11038wS__A0024TTNATS2020010105DP001.dat")
                .unwrap()
                .units,
            "k"
        );
        assert!(product("us_ssmv11099tS__T0001TTNATS2020010105HP001.dat").is_err());
        assert!(product("us_ssmv1103").is_err());
        assert!(product("us_ssmv11034tS__T0001TTNATS2020130105HP001.dat").is_err());
    }

    #[test]
    fn vortex_georeference_is_gdal_translates() {
        let g = Grid {
            dat_name: "x".into(),
            product: product("us_ssmv11034tS__T0001TTNATS2020010105HP001.dat").unwrap(),
            header: None,
        };
        let t = g.transform(SnodasOptions::default()).unwrap();
        assert_eq!(t.origin_x, -124.73333333);
        assert_eq!(t.dx, (-66.94166667 - -124.73333333) / 6935.0);
        assert_eq!(t.dy, (24.95 - 52.875) / 3351.0);
        assert!(g
            .time(SnodasOptions {
                times: SnodasTimes::Header,
                ..Default::default()
            })
            .is_err());
    }
}
