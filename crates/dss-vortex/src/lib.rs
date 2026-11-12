//! Tier V: pure-Rust gridded import for HEC-DSS, replacing HEC-Vortex.
//!
//! This crate holds the frame model every source and tool shares. Third-party decoders
//! live here and not in `dss-core`, and `dss-wasm` must not depend on it. Decoders
//! are pinned exactly, with no C in the build graph; `tests/build_graph.rs` enforces
//! that.
#![forbid(unsafe_code)]

pub mod crs;
pub mod error;
pub mod file_identity;
pub mod frame;
pub mod grid_tools;
pub mod import;
pub mod import_archive;
pub mod import_netcdf;
pub mod maths;
pub mod products;
pub mod sink_dss;
pub mod sink_raster;
pub mod source;
pub mod temporal;
pub mod units;
pub mod warp;
pub mod zonal;

pub use error::{FrameError, VortexError};
pub use frame::{
    Crs, DataType, FrameTime, GeoTransform, GridFrame, GridFrameParts, NamedCrs, RowOrder,
};
