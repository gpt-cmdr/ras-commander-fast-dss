//! Allocations whose size comes from a file. A failed infallible allocation ends the process, so
//! what a file sizes is reserved fallibly and reported as an error.

use crate::error::{Error, Result};

/// A vector with room for `capacity` items.
pub(crate) fn try_with_capacity<T>(capacity: usize) -> Result<Vec<T>> {
    let mut out = Vec::new();
    out.try_reserve_exact(capacity)
        .map_err(|_| Error::InvalidData(format!("cannot allocate {capacity} items")))?;
    Ok(out)
}
