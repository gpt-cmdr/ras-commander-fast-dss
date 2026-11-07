//! Pure-Rust HEC-DSS engine.
//!
//! Format knowledge is derived from HEC's MIT-licensed `hec-dss` source
//! (<https://github.com/HydrologicEngineeringCenter/hec-dss>). Every layout rule
//! cites the `heclib` file it comes from. See `docs/format/`.
//!
//! The engine never links to HEC's C or Fortran libraries.

#![forbid(unsafe_code)]

pub mod catalog;
pub mod grid;
pub mod header;
pub mod paireddata;
pub mod time;
pub mod timeseries;
// Version readers; each owned by one packet: dss6 (P07), dss7 (P06).
pub mod dss6;
pub mod dss7;

pub use catalog::{AnomalyKind, Catalog, CatalogAnomaly, CatalogEntry, RecordStatus};
pub use grid::{AlbersProjection, Grid, GridError, GridReadOptions, GridType, GRID_UNDEFINED};
pub use header::{ByteOrder, DssVersion, FileSignature, HeaderError};
pub use paireddata::{PairedData, PdValues, ValueWidth};
pub use timeseries::{is_missing, IrregularBlock, TimeSeries, TsKind, TsValues};
// Location metadata (P15).
pub mod location;
pub use location::{
    CoordinateSystem, HorizontalDatum, HorizontalUnits, Location, LocationSource, VerticalDatum,
    VerticalUnits,
};
// P19a: HEC lock-word detection for writers (D10). Not built for wasm32 (no writer there).
#[cfg(not(target_arch = "wasm32"))]
pub mod lock;
// Record user-header text (P26).
pub mod user_header;
pub use user_header::{parse_user_header_items, user_header_text};
// P28.
pub use user_header::{decode_user_header_text, user_header_text_encoding, UserHeaderEncoding};

// P31: UTF-8 with U+FFFD per malformed sequence, as Java's decoder does.
pub use user_header::decode_utf8_replacing;
// DSS6 location supplemental text from the user header (P27).
pub use location::SupplementalSource;
// P30: HEC's `zisMissingFloat` / `zisMissingDouble` test (NaN is a value), for `ztsTrim`.
pub use timeseries::is_hec_missing;
// P24: grid writer, version-neutral part (input, statistics, compression). Not built for
// wasm32 (no writer there).
#[cfg(not(target_arch = "wasm32"))]
pub mod grid_write;
