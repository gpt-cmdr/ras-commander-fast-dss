//! DSS6 grids. Java heclib `GridInfoFlat.unflatten` plus projection-specific
//! subclasses; `GriddedData.retrieveGriddedData` uses zreadx to read the legacy
//! int header and short payload. `zgtrec6.f` supplies logical words (pad skipped).

use super::{Dss6File, Dss6Info, Dss6Record, ReadAt};
use crate::grid::{self, bad, bounded, Grid, GridError, GridReadOptions};

impl<S: ReadAt> Dss6File<S> {
    pub fn read_grid(&self, pathname: &[u8]) -> Result<Grid, GridError> {
        self.read_grid_with(pathname, &GridReadOptions::default())
    }

    pub fn read_grid_with(
        &self,
        pathname: &[u8],
        opts: &GridReadOptions,
    ) -> Result<Grid, GridError> {
        let rec = self.lookup(pathname)?.ok_or(GridError::NotFound)?;
        self.decode_grid_info_with(&rec.info, opts)
    }

    pub fn decode_grid(&self, rec: &Dss6Record) -> Result<Grid, GridError> {
        self.decode_grid_info(&rec.info)
    }

    pub fn decode_grid_info(&self, info: &Dss6Info) -> Result<Grid, GridError> {
        self.decode_grid_info_with(info, &GridReadOptions::default())
    }

    pub fn decode_grid_info_with(
        &self,
        info: &Dss6Info,
        opts: &GridReadOptions,
    ) -> Result<Grid, GridError> {
        grid::GridType::from_record_type(info.record_type)?;
        if !info.has_valid_flag() || !matches!(info.status, 1 | 11) {
            return Err(bad("invalid grid info flag/status"));
        }
        // zrdinf6.f denies password-protected records before retrieving data.
        if self.is_record_denied(info) {
            return Err(GridError::PasswordProtected);
        }
        let nh = bounded(
            i64::from(info.internal_header.1),
            opts.max_header_bytes / 4,
            "header ints",
        )?;
        let header = self.read_words(i64::from(info.internal_header.0), nh)?;
        let mut grid = grid::legacy_header(&info.pathname, info.record_type, &header, opts)?;
        let nd = bounded(
            i64::from(info.data_words),
            opts.max_compressed_bytes / 4,
            "data ints",
        )?;
        let data = self.read_words(i64::from(info.data_address), nd)?;
        let bytes: Vec<_> = data.iter().flat_map(|v| v.to_le_bytes()).collect();
        grid.data = grid::decode_data(&grid, &bytes, true, opts)?;
        Ok(grid)
    }
}
