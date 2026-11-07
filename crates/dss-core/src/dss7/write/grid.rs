//! DSS7 grid records (packet P24): the areas `Public/zspatialGridStore.c:
//! zspatialGridStore()` (tag `7-IU-15`) hands to `zwrite()`, filled the way HEC Monolith
//! 3.3.27 fills the struct (`GriddedData.storeGriddedContainer` ->
//! `GridStructContainer(GridInfo)` -> `javaHeclib/src/Hec_zgridStore.c`). The record is
//! then stored by the P20 engine (`engine::write_areas`), one transaction in the P20 §4
//! order.
//!
//! Layout (`dss7.md` §5.2, modern `-100` grids):
//! - internal header, 24 ints (`INT_HEAD_grid_*`, `headers/zStructSpatialGrid.h` lines
//!   92-108; `zspatialGridStore.c` lines 96-115, 225/272);
//! - header2: units, data source, SRS name, SRS definition, time zone, each
//!   NUL-terminated (lines 119-189);
//! - user header, 11 ints: cell size, cell-zero x and y, null value (lines 191-202);
//! - values1: min, max, mean, the range limits and the counts (lines 208-217);
//! - values3: the cells, ZLIB-compressed or plain, padded to an even count (lines
//!   218-278);
//! - info first/last date from the D and E parts (lines 297-317).

use super::engine::Areas;
use crate::grid_write::{
    self as gw, GridCompression, GridInput, GridPlan, GridProjection, GRID_NODATA,
};

/// Ints of a byte string packed as heclib's `charInt` does (little-endian, zero filled).
fn bytes_to_ints(b: &[u8]) -> Vec<i32> {
    b.chunks(4)
        .map(|c| {
            let mut w = [0u8; 4];
            w[..c.len()].copy_from_slice(c);
            i32::from_le_bytes(w)
        })
        .collect()
}

/// The five header2 strings as Monolith fills them (`GridStructContainer(GridInfo)`
/// offsets 192-481): HRAP `data source`, `WKT`, the HRAP WKT and `UTC`; Albers an empty
/// source, `WKT`, the Albers WKT (SHG or built from the parameters) and no time zone;
/// specified grids their own fields.
fn strings(input: &GridInput) -> [Vec<u8>; 5] {
    let units = input.units.to_vec();
    match &input.projection {
        GridProjection::Hrap { data_source } => [
            units,
            data_source.clone(),
            b"WKT".to_vec(),
            gw::hrap_wkt().as_bytes().to_vec(),
            b"UTC".to_vec(),
        ],
        GridProjection::Albers(a) => [
            units,
            Vec::new(),
            b"WKT".to_vec(),
            gw::albers_wkt(
                a,
                input.x_coord_of_grid_cell_zero,
                input.y_coord_of_grid_cell_zero,
            )
            .into_bytes(),
            Vec::new(),
        ],
        GridProjection::Specified(s) => [
            units,
            Vec::new(),
            s.srs_name.clone(),
            s.srs_definition.clone(),
            s.time_zone_id.clone(),
        ],
    }
}

/// The cells as stored in values3 and the compressed byte count
/// (`INT_HEAD_grid_sizeofCompressedElements`).
fn cells(input: &GridInput) -> (i32, Vec<i32>, i32) {
    let n = input.data.len();
    // zspatialGridStore.c lines 218-221: numLongs = (n-1)/2 + 1 words of floats; an odd
    // grid gets one zero alignment float.
    let num_longs = (n - 1) / 2 + 1;
    let mut raw = Vec::with_capacity(num_longs * 8);
    for v in input.data {
        raw.extend_from_slice(&v.to_le_bytes());
    }
    raw.resize(num_longs * 8, 0);
    match input.compression {
        GridCompression::Uncompressed => {
            // Lines 223-231: sizeofCompressedElements = n*4; values3Number = numLongs*2.
            (gw::NO_COMPRESSION, bytes_to_ints(&raw), (n * 4) as i32)
        }
        _ => {
            // Lines 234-278: compress_zlib(dataValues, numLongs*2*4) at
            // Z_DEFAULT_COMPRESSION (zlibcompress.c line 29).
            let z = gw::zlib(&raw, 6);
            (gw::ZLIB_COMPRESSION, bytes_to_ints(&z), z.len() as i32)
        }
    }
}

/// Build the record areas of a planned grid.
pub(super) fn areas(input: &GridInput, plan: &GridPlan) -> Areas {
    let (method, v3, size) = cells(input);
    let st = &plan.stats;
    let (srs_type, tz_offset, is_interval, is_time_stamped, null) = match &input.projection {
        // GridStructContainer(GridInfo) offsets 221-263 (HRAP) and 313-336 (Albers):
        // srsDefinitionType 0, offset 0, isInterval 0, isTimeStamped 0. Monolith leaves
        // the null value at the Java default 0.0; the writer stores HEC's no-data value
        // (P24 spec §4, deviation 1).
        GridProjection::Hrap { .. } | GridProjection::Albers(_) => (0, 0, 0, 0, GRID_NODATA),
        GridProjection::Specified(s) => (
            s.srs_definition_type,
            s.time_zone_raw_offset,
            i32::from(s.is_interval),
            i32::from(input.time_stamped),
            s.null_value,
        ),
    };
    let mut ih = vec![0i32; 24];
    ih[0] = -100; // VERSION_100 (zstructSpatialGridNew)
    ih[1] = plan.record_type;
    ih[2] = input.projection.grid_info_version();
    ih[3] = input.data_type;
    ih[4] = input.lower_left_cell_x;
    ih[5] = input.lower_left_cell_y;
    ih[6] = input.nx as i32;
    ih[7] = input.ny as i32;
    ih[8] = method;
    ih[9] = size;
    ih[10] = st.range_limits.len() as i32;
    ih[11] = srs_type;
    ih[12] = tz_offset;
    ih[13] = is_interval;
    ih[14] = is_time_stamped;
    ih[15] = 0; // GRID_FLOAT
    let mut text = Vec::new();
    for s in strings(input) {
        text.extend_from_slice(&s);
        text.push(0);
    }
    let h2 = bytes_to_ints(&text);
    let mut uh = vec![0i32; 11];
    uh[0] = input.cell_size.to_bits() as i32;
    uh[1] = input.x_coord_of_grid_cell_zero.to_bits() as i32;
    uh[2] = input.y_coord_of_grid_cell_zero.to_bits() as i32;
    uh[3] = null.to_bits() as i32;
    let mut v1 = vec![
        st.min.to_bits() as i32,
        st.max.to_bits() as i32,
        st.mean.to_bits() as i32,
    ];
    v1.extend(st.range_limits.iter().map(|v| v.to_bits() as i32));
    v1.extend_from_slice(&st.range_counts);
    let to_i32 = |t: Option<(i64, i64)>| t.map_or((0, 0), |(j, s)| (j as i32, s as i32));
    Areas {
        data_type: plan.record_type,
        ih,
        h2,
        uh,
        v1,
        v2: Vec::new(),
        v3,
        number_data: 0,
        logical: 0,
        first: to_i32(plan.start),
        last: to_i32(plan.end),
        total_expanded: 0,
    }
}
