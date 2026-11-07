//! Allocations whose size comes from a file.
//!
//! A failed infallible allocation ends the process, and no caller can catch it. So what a file sizes
//! is reserved fallibly here and reported as [`Error::AllocationFailed`], which allocates nothing:
//! these reservations can be small, and a small one failing means the process has no memory left
//! for a message either. Allocations smaller than one parsed item stay infallible: the caller
//! charges them through [`Storage::charge`], which can refuse before they fail.
//!
//! [`Storage::charge`]: crate::storage::Storage::charge

use std::collections::HashSet;

use crate::error::{Error, Result};

fn failed(what: &'static str, count: usize, unit: &'static str) -> Error {
    Error::AllocationFailed { what, count, unit }
}

/// A vector with room for `capacity` items.
pub(crate) fn try_with_capacity<T>(capacity: usize) -> Result<Vec<T>> {
    let mut out = Vec::new();
    out.try_reserve_exact(capacity)
        .map_err(|_| failed("", capacity, "items"))?;
    Ok(out)
}

/// `Vec::push`, reporting a failure to grow.
pub(crate) fn try_push<T>(out: &mut Vec<T>, value: T) -> Result<()> {
    out.try_reserve(1)
        .map_err(|_| failed("room for ", out.len().saturating_add(1), "items"))?;
    out.push(value);
    Ok(())
}

/// A copy of `bytes`.
pub(crate) fn try_copy(bytes: &[u8]) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    out.try_reserve_exact(bytes.len())
        .map_err(|_| failed("", bytes.len(), "bytes"))?;
    out.extend_from_slice(bytes);
    Ok(out)
}

/// A copy of `strings`, each of which a file can make large.
pub(crate) fn try_copy_strings(strings: &[String]) -> Result<Vec<String>> {
    let mut out = try_with_capacity(strings.len())?;
    for string in strings {
        let mut copy = String::new();
        copy.try_reserve_exact(string.len())
            .map_err(|_| failed("", string.len(), "bytes"))?;
        copy.push_str(string);
        out.push(copy);
    }
    Ok(out)
}

/// `HashSet::insert`, reporting a failure to grow.
pub(crate) fn try_insert(set: &mut HashSet<u64>, value: u64) -> Result<bool> {
    set.try_reserve(1)
        .map_err(|_| failed("room for ", set.len(), "visited nodes"))?;
    Ok(set.insert(value))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_impossible_size_is_an_error_and_not_an_abort() {
        assert!(matches!(
            try_with_capacity::<u64>(usize::MAX / 8),
            Err(Error::AllocationFailed { .. })
        ));
        assert!(matches!(
            try_with_capacity::<u8>(isize::MAX as usize),
            Err(Error::AllocationFailed { .. })
        ));
    }

    #[test]
    fn a_failure_reads_as_before_and_is_a_resource_limit() {
        let error = try_with_capacity::<u64>(usize::MAX / 8).unwrap_err();
        assert_eq!(
            error.to_string(),
            format!("resource limit: cannot allocate {} items", usize::MAX / 8)
        );
        assert!(error.is_resource_limit());
        assert!(error.with_context("/x").is_resource_limit());
    }

    #[test]
    fn the_helpers_do_what_their_infallible_forms_do() {
        let mut out: Vec<u8> = try_with_capacity(2).unwrap();
        try_push(&mut out, 7).unwrap();
        assert_eq!(out, [7]);
        assert_eq!(try_copy(&[1, 2, 3]).unwrap(), [1, 2, 3]);
        let mut set = HashSet::new();
        assert!(try_insert(&mut set, 5).unwrap());
        assert!(!try_insert(&mut set, 5).unwrap());
    }
}
