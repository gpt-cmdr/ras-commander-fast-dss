//! Typed errors for the Tier V crate.

use crate::{DataType, RowOrder};
use std::fmt;

/// Why a [`GridFrame`](crate::GridFrame) could not be built.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum FrameError {
    /// `nx` or `ny` is zero.
    EmptyGrid { nx: usize, ny: usize },
    /// `nx`, `ny` or their product does not fit the `i32` a DSS grid record stores.
    GridTooLarge { nx: usize, ny: usize },
    /// The cell buffer length is not `nx * ny`.
    CellCountMismatch { expected: usize, actual: usize },
    /// A transform coefficient is NaN or infinite.
    NonFiniteTransform,
    /// The transform maps the grid to a line or a point.
    DegenerateTransform,
    /// An unrotated transform's row direction disagrees with the stated row order.
    RowOrderMismatch { order: RowOrder, dy: f64 },
    /// A rotated frame cannot be flipped by reversing its rows.
    RotatedRowFlip,
    /// A period does not end after it starts, or a time is outside its day.
    InvalidTime { reason: &'static str },
    /// PER-AVER and PER-CUM need a period; INST-VAL and INST-CUM cannot have one.
    /// Source: `dss_core::grid_write::GridInput`.
    DataTypeTimeMismatch { data_type: DataType },
}

impl fmt::Display for FrameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FrameError::EmptyGrid { nx, ny } => write!(f, "grid is empty ({nx} x {ny})"),
            FrameError::GridTooLarge { nx, ny } => {
                write!(f, "grid {nx} x {ny} does not fit a DSS grid record")
            }
            FrameError::CellCountMismatch { expected, actual } => {
                write!(f, "expected {expected} cells, got {actual}")
            }
            FrameError::NonFiniteTransform => write!(f, "transform has a non-finite coefficient"),
            FrameError::DegenerateTransform => write!(f, "transform is degenerate"),
            FrameError::RowOrderMismatch { order, dy } => {
                write!(f, "row order {order:?} disagrees with row spacing {dy}")
            }
            FrameError::RotatedRowFlip => write!(f, "a rotated frame cannot flip its rows"),
            FrameError::InvalidTime { reason } => write!(f, "invalid frame time: {reason}"),
            FrameError::DataTypeTimeMismatch { data_type } => {
                write!(f, "data type {data_type:?} does not fit the frame's time")
            }
        }
    }
}

impl std::error::Error for FrameError {}

/// Any Tier V failure. Refusals are values, never panics, on any input.
#[derive(Debug)]
#[non_exhaustive]
pub enum VortexError {
    /// The frame model's invariants were not met.
    Frame(FrameError),
    /// The input is a format, template or product this library does not read.
    Unsupported { what: String },
    /// A deliberate refusal, for example a datum shift with no supplied grid.
    Refused { what: String, reason: String },
    /// A decoder rejected the input bytes.
    Decode {
        format: &'static str,
        message: String,
    },
    /// Reading or writing a file failed.
    Io(std::io::Error),
}

impl fmt::Display for VortexError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            VortexError::Frame(e) => write!(f, "{e}"),
            VortexError::Unsupported { what } => write!(f, "unsupported: {what}"),
            VortexError::Refused { what, reason } => write!(f, "refused {what}: {reason}"),
            VortexError::Decode { format, message } => write!(f, "{format} decode: {message}"),
            VortexError::Io(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for VortexError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            VortexError::Frame(e) => Some(e),
            VortexError::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<FrameError> for VortexError {
    fn from(e: FrameError) -> Self {
        VortexError::Frame(e)
    }
}

impl From<std::io::Error> for VortexError {
    fn from(e: std::io::Error) -> Self {
        VortexError::Io(e)
    }
}
