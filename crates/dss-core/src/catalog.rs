//! Version-neutral catalog types shared by the DSS6 and DSS7 readers.
//!
//! The DSS7 reader (`dss7`, packet P06) and the DSS6 reader (`dss6`, packet P07)
//! each walk their own on-disk structures and map what they find into these types.
//! Layout rules live in the version modules; this file holds no format knowledge.
//! Changes here affect both readers, so keep them additive.

/// Lifecycle state of a catalog entry, normalized across versions.
///
/// DSS7 codes come from `zdssKeys.h` / `zcheckKeys` (0 undeleted, 1 primary, 2 alias,
/// 11 deleted, 12 renamed, 13 alias deleted, 15 reused); DSS6 codes from `zcatfi6`
/// (1/11 live, 2/12 deleted, 3/13 renamed, 4/14 rewritten-old). The raw code is kept
/// in [`CatalogEntry::raw_status`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RecordStatus {
    /// Readable record (DSS7 0 or 1, DSS6 1 or 11).
    Live,
    /// DSS7 alias entry (status 2). Absent from the P02 corpus.
    Alias,
    /// Deleted record whose space has not been reused.
    Deleted,
    /// Old pathname of a renamed record.
    Renamed,
    /// DSS6 superseded copy of a rewritten record (status 4/14).
    RewrittenOld,
    /// Any other code; see `raw_status`.
    Other,
}

/// One pathname found by a catalog walk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogEntry {
    /// Pathname bytes exactly as stored (DSS6 may contain Latin-1 bytes).
    pub pathname: Vec<u8>,
    /// Record type code (100 RTS, 105 RTD, 200 PD, 420 Albers grid, ...).
    pub record_type: i32,
    pub status: RecordStatus,
    /// Status code as stored in the bin.
    pub raw_status: i32,
    /// Stored pathname hash from the bin entry.
    pub stored_hash: i64,
    /// Byte offset of the record's info block (DSS7 record header).
    pub info_address: u64,
    /// Byte offset of the bin entry that listed this record.
    pub bin_address: u64,
    /// Last write time in milliseconds since 1970-01-01 UTC when stored (DSS7 bins);
    /// `None` when the version does not keep it in the bin.
    pub last_write_ms: Option<i64>,
}

impl CatalogEntry {
    /// Pathname decoded for display. Non-UTF-8 bytes are mapped as Latin-1.
    pub fn pathname_lossy(&self) -> String {
        match std::str::from_utf8(&self.pathname) {
            Ok(s) => s.to_owned(),
            Err(_) => self.pathname.iter().map(|&b| b as char).collect(),
        }
    }

    pub fn is_live(&self) -> bool {
        self.status == RecordStatus::Live
    }
}

/// Structural problem found while walking a catalog. Walks report anomalies and keep
/// going; they only fail for damage that makes the rest of the walk meaningless.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogAnomaly {
    pub kind: AnomalyKind,
    /// Byte offset the anomaly refers to, when there is one.
    pub address: Option<u64>,
    pub detail: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AnomalyKind {
    /// Recomputed pathname hash differs from the stored hash.
    HashMismatch,
    /// Live bin entry not reachable from any hash-table slot (DSS7 orphan bin).
    OrphanBin,
    /// Live bin entry whose info block is not valid (zeroed or wrong flag).
    BadInfoBlock,
    /// Same pathname listed live more than once.
    DuplicatePathname,
    /// Header record count differs from the number of live entries walked.
    RecordCountMismatch,
    /// A pointer or length outside the file; the affected structure was skipped.
    OutOfBounds,
    /// A loop guard stopped a cyclic chain.
    CycleDetected,
    Other,
}

/// Result of a full catalog walk.
#[derive(Debug, Clone, Default)]
pub struct Catalog {
    /// Entries in on-disk bin order (HEC's unsorted catalog order).
    pub entries: Vec<CatalogEntry>,
    pub anomalies: Vec<CatalogAnomaly>,
}

impl Catalog {
    pub fn live(&self) -> impl Iterator<Item = &CatalogEntry> {
        self.entries.iter().filter(|e| e.is_live())
    }
}
