//! DSS6 grid records: what HEC Monolith 3.3.27 hands to `zwritx` for a DSS6
//! file, `GriddedData.storeGriddedData` (DSS6 branch, offsets 19-1054) ->
//! `javaHeclib/src/Hec_zwritx.c` -> `heclib_c/src/DssInterface/v6and7/zwritex.c` ->
//! `heclib_f/src/zwritex6.f` (tag `7-IU-15`). The record is stored by the engine as
//! one transaction (`dss6.md` §9.3).
//!
//! - Internal header: the Java `GridInfoFlat` int array (`GridInfoFlat.flatten`, with
//!   `HrapInfoFlat`, `AlbersInfoFlat` or `SpecifiedGridInfoFlat` appended; filled by
//!   `GridInfo.loadFlatData` and the subclasses' `makeGridInfoFlat`); `dss6.md` §5.2.
//! - Data: the compressed cells. Before compressing, Monolith replaces -FLT_MAX cells
//!   with the null value (-1.0E30, or a specified grid's own; offsets 81-133), after the
//!   statistics were taken. ZLIB: `DataCompression.zcompress` (-FLT_MAX becomes -1.0E38,
//!   little-endian floats, deflate level 9, padded to whole words). PRECIP_2_BYTE:
//!   `DataCompression.compress`, two codes per word with the first in the high half-word
//!   (`storeGriddedData` swaps the halves on Windows, offsets 824-881; the reader swaps
//!   them back, `grid.rs` `decode_data`).
//! - Info block: `KITYPE` the grid type, `KILNDA` = `KINDAT`, `KIPREC` 0, as `zwritex6`
//!   leaves them (`znwrit6.f` lines 181-202).

use super::engine::Record6;
use crate::grid_write::{
    self as gw, GridCompression, GridInput, GridPlan, GridProjection, GridWriteError,
};

/// `Heclib.StringHol`: the bytes in whole ints, NUL filled.
fn hol(b: &[u8], words: usize) -> Vec<i32> {
    let mut v = vec![0u8; words * 4];
    v[..b.len()].copy_from_slice(b);
    v.chunks(4)
        .map(|c| i32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}

/// `SpecifiedGridInfoFlat` strings: `ceil(len / 4)` words, then the words.
fn counted(b: &[u8], out: &mut Vec<i32>) {
    let words = b.len().div_ceil(4);
    out.push(words as i32);
    out.extend(hol(b, words));
}

/// HEC minutes of a (julian, seconds) time (`HecTime.value()`); -2147483647 when there
/// is none: the value of an unset `HecTime` that Monolith stores (`UNDEFINED_TIME`,
/// `headers/heclibDate.h`; checked on Monolith-written grids).
fn minutes(t: Option<(i64, i64)>) -> i32 {
    t.map_or(-2_147_483_647, |(j, s)| (j * 1440 + s / 60) as i32)
}

/// Compressed cells: (method, data words, compressed bytes, scale factor, base).
fn compress(
    input: &GridInput,
    plan: &GridPlan,
) -> Result<(i32, Vec<i32>, i32, f32, f32), GridWriteError> {
    let null = input.projection.statistics_null();
    // storeGriddedData offsets 81-133: -FLT_MAX cells become the null value.
    let stored: Vec<f32> = input
        .data
        .iter()
        .map(|&v| if v == -f32::MAX { null } else { v })
        .collect();
    match input.compression {
        GridCompression::Precip2Byte { scale_factor, base } => {
            let (scale, base) = match (scale_factor, base) {
                (Some(s), Some(b)) => (s, b),
                _ => gw::precip_auto_scale_base(&plan.stats),
            };
            let spec_null = match &input.projection {
                GridProjection::Specified(s) => Some(s.null_value),
                _ => None,
            };
            gw::check_precip_2_byte(input.data, spec_null, scale, base)?;
            let codes = gw::precip_2_byte_compress(&stored, scale, base);
            let bytes = (codes.len() * 2) as i32;
            // Two codes per word, the first in the high half-word.
            let words = codes
                .chunks(2)
                .map(|c| {
                    let hi = c[0] as u16 as u32;
                    let lo = c.get(1).map_or(0, |&x| x as u16 as u32);
                    ((hi << 16) | lo) as i32
                })
                .collect();
            Ok((gw::PRECIP_2_BYTE, words, bytes, scale, base))
        }
        GridCompression::Zlib => {
            let mut raw = Vec::with_capacity(stored.len() * 4);
            for v in stored {
                // DataCompression.zcompress offsets 0-29.
                let v = if v == -f32::MAX { -1.0e38f32 } else { v };
                raw.extend_from_slice(&v.to_le_bytes());
            }
            let z = gw::zlib(&raw, 9);
            let words = z
                .chunks(4)
                .map(|c| {
                    let mut w = [0u8; 4];
                    w[..c.len()].copy_from_slice(c);
                    i32::from_le_bytes(w)
                })
                .collect();
            // ZLIB leaves the GridInfo scale and base as they were (offsets 615-633): the
            // class defaults, -FLT_MAX for HRAP and Albers, 1.0 and 0.0 for specified
            // grids (SpecifiedGridInfo.<init> setCompressionInfo(26, 0, 1.0, 0.0)).
            let (scale, base) = match input.projection {
                GridProjection::Specified(_) => (1.0, 0.0),
                _ => (-f32::MAX, -f32::MAX),
            };
            Ok((gw::ZLIB_COMPRESSION, words, z.len() as i32, scale, base))
        }
        GridCompression::Uncompressed => Err(GridWriteError::Unsupported(
            "DSS6 grids are not stored uncompressed".into(),
        )),
    }
}

/// Build the DSS6 record of a planned grid.
pub(super) fn record(input: &GridInput, plan: &GridPlan) -> Result<Record6, GridWriteError> {
    let (method, data, size, scale, base) = compress(input, plan)?;
    let st = &plan.stats;
    let f = |v: f32| v.to_bits() as i32;
    // GridInfo.<init> _gridInfoSize 124; _infoSize adds 4 (HRAP), 40 (Albers) or 36
    // (specified); GridInfoFlat size 252 + 12 (HRAP), + 48 (Albers), + 44 (specified,
    // set before its strings are known, so it does not count them).
    let (info_size, flat_size) = match input.projection {
        GridProjection::Hrap { .. } => (128, 264),
        GridProjection::Albers(_) => (164, 300),
        GridProjection::Specified(_) => (160, 296),
    };
    let mut h = vec![0i32; 63];
    h[0] = flat_size;
    h[1] = plan.record_type;
    h[2] = info_size;
    h[3] = 124;
    h[4] = minutes(plan.start);
    h[5] = minutes(plan.end);
    h[6..9].copy_from_slice(&hol(input.units, 3));
    h[9] = input.data_type;
    h[10] = input.lower_left_cell_x;
    h[11] = input.lower_left_cell_y;
    h[12] = input.nx as i32;
    h[13] = input.ny as i32;
    h[14] = f(input.cell_size);
    h[15] = method;
    h[16] = size;
    h[17] = f(scale);
    h[18] = f(base);
    // GridInfoFlat order: max, min, mean.
    h[19] = f(st.max);
    h[20] = f(st.min);
    h[21] = f(st.mean);
    let nr = st.range_limits.len().min(20);
    h[22] = nr as i32;
    for i in 0..nr {
        h[23 + i] = f(st.range_limits[i]);
        h[43 + i] = st.range_counts[i];
    }
    match &input.projection {
        GridProjection::Hrap { data_source } => h.extend(hol(data_source, 3)),
        GridProjection::Albers(a) => {
            h.push(a.datum);
            h.extend(hol(&a.units, 3));
            for v in [
                a.first_standard_parallel,
                a.second_standard_parallel,
                a.central_meridian,
                a.latitude_of_projection_origin,
                a.false_easting,
                a.false_northing,
                input.x_coord_of_grid_cell_zero,
                input.y_coord_of_grid_cell_zero,
            ] {
                h.push(f(v));
            }
        }
        GridProjection::Specified(s) => {
            h.push(2); // SpecifiedGridInfo._version
            counted(&s.srs_name, &mut h);
            h.push(s.srs_definition_type);
            counted(&s.srs_definition, &mut h);
            h.push(f(input.x_coord_of_grid_cell_zero));
            h.push(f(input.y_coord_of_grid_cell_zero));
            h.push(f(s.null_value));
            counted(&s.time_zone_id, &mut h);
            h.push(s.time_zone_raw_offset);
            h.push(i32::from(s.is_interval));
            h.push(i32::from(input.time_stamped));
        }
    }
    let logical = data.len() as i32;
    Ok(Record6 {
        pathname: plan.pathname.clone(),
        record_type: plan.record_type,
        internal_header: h,
        data,
        logical,
        quality: false,
        precision: 0,
    })
}
