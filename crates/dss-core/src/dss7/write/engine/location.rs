//! DSS7 location store (packet P39): `Public/zlocationStore.c:zlocationStore()` (storage
//! flag 1) with `Internal/zlocationUpdatePerm.c:zlocationUpdatePerm()`, committed with
//! P20's transaction order. Tag `7-IU-15`.
//!
//! Record: the pathname from `zlocationPath()`; values1 x, y, z (doubles, low word
//! first), the six codes, then the time-zone name when it is not empty and fits (lines
//! 203-226; at most 30 ints; see [`areas`] for the terminator); values2 the supplemental text
//! and its NUL in `numberIntsInBytes(len + 1)` ints, absent when there is none (lines
//! 228-236). `numberValues` = values1 ints (line 240); no headers; first and last dates
//! untouched (0 on a fresh handle).

use super::{Areas, Engine, Session};
use crate::dss7::write::location::{check_common, LocationFields, LocationInput};
use crate::dss7::write::{validate_pd_pathname, WriteError, WriteOptions, WriteReport};
use crate::dss7::RecordRef;
use crate::location::{location_pathname, LOCATION};
use std::fs::File;

/// `zlocationStore()`: `int values1[30]`, 12 of them coordinates and codes, and the
/// terminator below: 71 bytes and a NUL fill the 18 ints.
pub(crate) const MAX_TIME_ZONE_BYTES: usize = 18 * 4 - 1;
/// `zdssFileKeys.klocBoundLR` .. `klocBoundElev` (`Internal/zinit.c` lines 698-702;
/// dss7.md §2.2 words 84-88).
const LOC_BOUND_LR: usize = 84;
const LOC_BOUND_LL: usize = 85;
const LOC_BOUND_UR: usize = 86;
const LOC_BOUND_UL: usize = 87;
const LOC_BOUND_ELEV: usize = 88;

fn pack(b: &[u8], ints: usize) -> Vec<i32> {
    let mut buf = b.to_vec();
    buf.resize(ints * 4, 0);
    buf.chunks_exact(4)
        .map(|c| i32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}

/// `zlocationStructValid()`: heclib stores nothing without one of these (and returns
/// `STATUS_OKAY`); the other four codes do not count.
pub(crate) fn has_data(f: &LocationFields) -> bool {
    f.xyz.iter().any(|&v| v != 0.0)
        || f.codes[3] != 0
        || f.codes[5] != 0
        || f.supplemental.is_some()
        || f.time_zone.as_ref().is_some_and(|t| !t.is_empty())
}

fn areas(f: &LocationFields) -> Areas {
    let mut v1 = Vec::with_capacity(30);
    for v in f.xyz {
        let b = v.to_bits();
        v1.push(b as u32 as i32);
        v1.push((b >> 32) as u32 as i32);
    }
    v1.extend_from_slice(&f.codes);
    if let Some(tz) = f.time_zone.as_deref().filter(|t| !t.is_empty()) {
        // Deviation (P39 REPORT §5): heclib uses `numberIntsInBytes(len)`, so a name of 4, 8,
        // ... bytes has no NUL, and zlocationRetrieve() reads past values1 into whatever
        // memory follows (observed with heclib and Monolith on heclib's own files). One
        // more int keeps the name terminated; every other length is word-identical.
        v1.extend(pack(tz, (tz.len() + 1).div_ceil(4)));
    }
    let v2 = f
        .supplemental
        .as_deref()
        .map_or_else(Vec::new, |s| pack(s, (s.len() + 1).div_ceil(4)));
    let n = v1.len() as i64;
    Areas {
        data_type: LOCATION,
        ih: Vec::new(),
        h2: Vec::new(),
        uh: Vec::new(),
        v1,
        v2,
        v3: Vec::new(),
        number_data: n,
        logical: n,
        first: (0, 0),
        last: (0, 0),
        total_expanded: 0,
    }
}

/// `Utilities/f4toi8.c`.
fn f4toi8(a: f32, b: f32) -> i64 {
    (u64::from(a.to_bits()) | (u64::from(b.to_bits()) << 32)) as i64
}

fn i8tof4(v: i64) -> (f32, f32) {
    let u = v as u64;
    (f32::from_bits(u as u32), f32::from_bits((u >> 32) as u32))
}

/// `zlocationUpdatePerm()`: the header's bounds grow only for latitude/longitude in
/// decimal degrees (coordinate system 1, horizontal units 3).
fn update_bounds(h: &mut [i64; 100], f: &LocationFields) {
    if f.codes[0] != 1 || f.codes[2] != 3 {
        return;
    }
    let (x, y, z) = (f.xyz[0] as f32, f.xyz[1] as f32, f.xyz[2] as f32);
    if f.xyz[1] != 0.0 {
        if h[LOC_BOUND_LR] == 0 {
            let ll = f4toi8(y, x);
            h[LOC_BOUND_LR] = ll;
            h[LOC_BOUND_LL] = ll;
            h[LOC_BOUND_UR] = ll;
            h[LOC_BOUND_UL] = ll;
        } else {
            // Per corner (east, north): LR x>, y<; LL x<, y<; UR x>, y>; UL x<, y>.
            for (w, east, north) in [
                (LOC_BOUND_LR, true, false),
                (LOC_BOUND_LL, false, false),
                (LOC_BOUND_UR, true, true),
                (LOC_BOUND_UL, false, true),
            ] {
                let (mut lat, mut lon) = i8tof4(h[w]);
                if (east && x > lon) || (!east && x < lon) {
                    lon = x;
                    h[w] = f4toi8(lat, lon);
                }
                if (north && y > lat) || (!north && y < lat) {
                    lat = y;
                    h[w] = f4toi8(lat, lon);
                }
            }
        }
    }
    if f.xyz[2] != 0.0 {
        if h[LOC_BOUND_ELEV] == 0 {
            h[LOC_BOUND_ELEV] = f4toi8(z, z);
        } else {
            let (mut lo, mut hi) = i8tof4(h[LOC_BOUND_ELEV]);
            if z < lo {
                lo = z;
                h[LOC_BOUND_ELEV] = f4toi8(lo, hi);
            }
            if z > hi {
                hi = z;
                h[LOC_BOUND_ELEV] = f4toi8(lo, hi);
            }
        }
    }
}

impl Engine<'_> {
    fn stored_location(&self, r: &RecordRef) -> Result<LocationFields, WriteError> {
        let mut rdr = self.reader()?;
        let l = rdr
            .read_location_record(r)
            .map_err(|e| WriteError::Corrupt(format!("cannot read the location record: {e}")))?
            .location;
        Ok(LocationFields {
            xyz: [l.x, l.y, l.z],
            codes: [
                l.coordinate_system_code,
                l.coordinate_id,
                l.horizontal_units_code,
                l.horizontal_datum_code,
                l.vertical_units_code,
                l.vertical_datum_code,
            ],
            // The reader maps each byte to one char (Latin-1); this restores the bytes.
            time_zone: l
                .time_zone_name
                .map(|s| s.chars().map(|c| c as u32 as u8).collect()),
            supplemental: l.supplemental,
        })
    }
}

/// Everything checkable without the file. Returns the location pathname.
pub(crate) fn plan_location(input: &LocationInput) -> Result<Vec<u8>, WriteError> {
    check_common(input)?;
    let lp = location_pathname(input.pathname).ok_or_else(|| {
        WriteError::BadPathname("the pathname has no A and B parts (fewer than three '/')".into())
    })?;
    validate_pd_pathname(&lp)?;
    if let Some(tz) = input.time_zone {
        if tz.len() > MAX_TIME_ZONE_BYTES {
            // zlocationStore() line 220 would silently drop it.
            return Err(WriteError::BadInput(format!(
                "time_zone is {} bytes; the writer stores at most {MAX_TIME_ZONE_BYTES} (heclib \
                 silently stores none above 72)",
                tz.len()
            )));
        }
    }
    if input.supplemental.is_some_and(|s| s.len() >= (1 << 26)) {
        return Err(WriteError::BadInput("supplemental text is too long".into()));
    }
    Ok(lp)
}

/// Store one location record (one transaction).
pub(crate) fn write_location_planned(
    file: &File,
    path: &[u8],
    input: &LocationInput,
    opts: &WriteOptions,
) -> Result<WriteReport, WriteError> {
    let mut session = Session::begin(file, opts)?;
    let found = session.eng.lookup_for_write(path)?;
    let mut base = LocationFields::default();
    if let Some(r) = &found {
        if r.record_type != LOCATION || r.info.record_type != LOCATION {
            // zwriteExisting() would overwrite it; the writer refuses.
            return Err(WriteError::Unsupported(format!(
                "{}: the existing record has type {} (info {}), not a location record",
                String::from_utf8_lossy(path),
                r.record_type,
                r.info.record_type
            )));
        }
        if input.keep_existing {
            base = session.eng.stored_location(r)?;
        }
    }
    let f = LocationFields::merged(input, base);
    if !has_data(&f) {
        return Err(WriteError::BadInput(
            "nothing to store: heclib stores a location only with a non-zero coordinate, \
             horizontal or vertical datum, a time zone or supplemental text \
             (zlocationStructValid.c)"
                .into(),
        ));
    }
    // A stored name of 72 bytes (heclib's limit, unterminated) cannot be kept terminated.
    if f.time_zone
        .as_ref()
        .is_some_and(|t| t.len() > MAX_TIME_ZONE_BYTES)
    {
        return Err(WriteError::BadInput(format!(
            "the stored time zone is longer than {MAX_TIME_ZONE_BYTES} bytes; give time_zone"
        )));
    }
    let a = areas(&f);
    update_bounds(&mut session.eng.hdr, &f);
    session.eng.block_index = 0;
    let block = session.eng.commit_areas(path.to_vec(), found, &a)?;
    session.finish(vec![block])
}
