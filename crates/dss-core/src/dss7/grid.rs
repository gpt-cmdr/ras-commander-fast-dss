//! DSS7 grids: `Public/zspatialGridRetrieve.c:zspatialGridRetrieve`,
//! `headers/zStructSpatialGrid.h:INT_HEAD_grid_*` (-100), and Java heclib
//! `GriddedData.retrieveGriddedData` / `GridInfoFlat.unflatten` (legacy headers).

use super::{info::Area, Dss7File, RecordRef};
use crate::grid::{self, bad, bounded, Grid, GridError, GridReadOptions};
use std::io::{Read, Seek};

impl<R: Read + Seek> Dss7File<R> {
    pub fn read_grid(&mut self, pathname: &[u8]) -> Result<Grid, GridError> {
        self.read_grid_with(pathname, &GridReadOptions::default())
    }

    pub fn read_grid_with(
        &mut self,
        pathname: &[u8],
        opts: &GridReadOptions,
    ) -> Result<Grid, GridError> {
        let rec = self.lookup(pathname)?.ok_or(GridError::NotFound)?;
        self.read_grid_record_with(&rec, opts)
    }

    /// Read a lookup/record_at result; also works for catalog recovery entries.
    pub fn read_grid_record(&mut self, rec: &RecordRef) -> Result<Grid, GridError> {
        self.read_grid_record_with(rec, &GridReadOptions::default())
    }

    pub fn read_grid_record_with(
        &mut self,
        rec: &RecordRef,
        opts: &GridReadOptions,
    ) -> Result<Grid, GridError> {
        grid::GridType::from_record_type(rec.record_type)?;
        if rec.info.record_type != rec.record_type {
            return Err(bad("bin/info grid type mismatch"));
        }
        let header = self.grid_area(rec.info.internal_header, opts.max_header_bytes)?;
        let h = grid::ints(&header);
        let version = *h.first().ok_or_else(|| bad("empty grid header"))?;
        let legacy = version >= 0;
        let mut g = if legacy {
            grid::legacy_header(&rec.pathname, rec.record_type, &h, opts)?
        } else if version == -100 {
            let chars = self.grid_area(rec.info.header2, opts.max_header_bytes)?;
            let geometry = self.grid_area(rec.info.user_header, opts.max_header_bytes)?;
            let stats = self.grid_area(rec.info.values1, opts.max_header_bytes)?;
            modern_header(
                &rec.pathname,
                rec.record_type,
                &h,
                &chars,
                &geometry,
                &stats,
                opts,
            )?
        } else {
            return Err(GridError::UnsupportedStructVersion { version });
        };
        // Legacy zreadx stores the payload in values1; -100 stores cells in values3.
        // Source: javaHeclib Hec_zreadx.c and zspatialGridRetrieve.c.
        let area = if legacy {
            rec.info.values1
        } else {
            rec.info.values3
        };
        let bytes = self.grid_area(area, opts.max_compressed_bytes)?;
        g.data = grid::decode_data(&g, &bytes, legacy, opts)?;
        Ok(g)
    }

    fn grid_area(&mut self, area: Area, cap: usize) -> Result<Vec<u8>, GridError> {
        let count = bounded(area.number_ints, cap / 4, "area ints")?;
        if count == 0 {
            return Ok(Vec::new());
        }
        // zget.c: int lengths round up to DSS7 8-byte words; discard only area padding.
        let mut bytes = self.read_words(area.address, count.div_ceil(2) as i64)?;
        bytes.truncate(count * 4);
        Ok(bytes)
    }
}

fn modern_header(
    path: &[u8],
    rt: i32,
    h: &[i32],
    chars: &[u8],
    geometry: &[u8],
    stats: &[u8],
    opts: &GridReadOptions,
) -> Result<Grid, GridError> {
    if h.len() < 24 || geometry.len() < 16 {
        return Err(bad("truncated -100 grid header/geometry"));
    }
    if h[1] != rt {
        return Err(bad("grid header type differs from record"));
    }
    grid::cells(h[6], h[7], opts)?;
    if h[15] != 0 {
        return Err(GridError::UnsupportedStorageType {
            storage_type: h[15],
        });
    }
    let nr = bounded(i64::from(h[10]), opts.max_header_bytes / 8, "ranges")?;
    // zspatialGridRetrieve: five NUL-terminated strings in header2 in this order.
    let mut strings = chars.split(|&v| v == 0);
    let mut next = || {
        strings
            .next()
            .map(grid::text)
            .ok_or_else(|| bad("truncated -100 grid strings"))
    };
    let units = next()?;
    let source = next()?;
    let srs_name = next()?;
    let srs_definition = next()?;
    let time_zone = next()?;
    if chars.iter().filter(|&&v| v == 0).count() < 5 {
        return Err(bad("unterminated -100 grid strings"));
    }
    let geom = grid::floats(geometry);
    let s = grid::ints(stats);
    if !s.is_empty() && s.len() < 3 + nr * 2 {
        return Err(bad("truncated statistics/range table"));
    }
    if nr != 0 && s.is_empty() {
        return Err(bad("missing range table"));
    }
    let stat = |i: usize| s.get(i).map(|&v| f32::from_bits(v as u32));
    let grid_type = grid::GridType::from_record_type(rt)?;
    // GridStructContainer.returnGridInfo: an Albers -100 record reports Monolith's
    // AlbersInfo.getSpatialReferenceSystem (SHG WKT or ""), derived from the stored
    // WKT and the geometry origin; the stored string is kept separately.
    let reported_srs = if grid_type == grid::GridType::Albers {
        grid::albers_srs_from_wkt(&srs_definition, geom[1], geom[2])
    } else {
        srs_definition.clone()
    };
    // Monolith fills an AlbersInfo from the stored WKT
    // (GridStructContainer.returnGridInfo -> setAlbersSpatialReferenceInfo).
    let albers =
        (grid_type == grid::GridType::Albers).then(|| grid::albers_from_wkt(&srs_definition));
    let parts: Vec<_> = path.split(|&v| v == b'/').collect();
    let start = parts.get(4).and_then(|p| grid::pathname_time(p));
    let end = parts.get(5).and_then(|p| grid::pathname_time(p));
    let g = Grid {
        pathname: path.to_vec(),
        record_type: rt,
        grid_type,
        struct_version: -100,
        version: h[2],
        info_size: None,
        grid_info_size: None,
        data_units: units,
        data_type: h[3],
        data_source: source,
        lower_left_cell_x: h[4],
        lower_left_cell_y: h[5],
        nx: h[6] as usize,
        ny: h[7] as usize,
        cell_size: geom[0],
        x_coord_of_grid_cell_zero: geom[1],
        y_coord_of_grid_cell_zero: geom[2],
        null_value: geom[3],
        srs_name,
        srs_definition_type: h[11],
        srs_definition: reported_srs,
        stored_srs_definition: Some(srs_definition),
        albers,
        time_zone_id: time_zone,
        time_zone_raw_offset: h[12],
        is_interval: h[13] != 0,
        is_time_stamped: h[14] != 0,
        start_time: start,
        end_time: end.or(start),
        stored_start_time: None,
        stored_end_time: None,
        compression_method: h[8],
        sizeof_compressed_elements: bounded(
            i64::from(h[9]),
            opts.max_compressed_bytes,
            "compressed bytes",
        )?,
        // No scale/base stored in -100; Java GridStructContainer defaults both to 0.
        compression_scale_factor: 0.0,
        compression_base: 0.0,
        storage_data_type: h[15],
        min_data_value: stat(0),
        max_data_value: stat(1),
        mean_data_value: stat(2),
        range_limit_table: if nr == 0 {
            Vec::new()
        } else {
            s[3..3 + nr]
                .iter()
                .map(|&v| f32::from_bits(v as u32))
                .collect()
        },
        number_equal_or_exceeding_range_limit: if nr == 0 {
            Vec::new()
        } else {
            s[3 + nr..3 + 2 * nr].to_vec()
        },
        data: Vec::new(),
    };
    grid::validate_geometry(&g)?;
    Ok(g)
}
