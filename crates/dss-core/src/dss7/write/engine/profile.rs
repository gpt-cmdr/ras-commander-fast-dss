//! DSS7 regular-interval profile store: `Public/ztsStore.c:ztsStore()` ->
//! `Internal/ztsStoreReg7.c:ztsStoreReg7()` -> `Internal/ztsRegStoreBlock.c:
//! ztsRegStoreBlock()` (storage flag 0) -> `Internal/ztsWriteBlock.c:ztsWriteBlock()`, tag
//! `7-IU-15`, committed with the transaction order of `dss7.md` §6.4. Times and block splitting
//! are the regular writer's [`plan`] (`ztsProcessTimes()` has no profile branch).
//!
//! What a profile changes (the rest follows `prepare_block()`):
//! - `ztsStoreReg7()` lines 183-206: `INT_HEAD_profileDepthsNumber` = depths,
//!   `valueElementSize` 1 or 2, `valueSize` = element size x depths;
//! - `ztsFirstLastValidValues()`: a row is valid when any of its depths is not missing or
//!   its quality is non-zero;
//! - `ztsAggregate()` (`internalHeader[INT_HEAD_profileDepthsNumber] == 0` gate on
//!   compression): profiles are never compressed;
//! - `ztsWriteBlock()` lines 243-244: the depths are values2, in the stored width;
//! - `ztsInternalHeaderPack()` lines 76-83: depth units, value units, type, time zone;
//! - `zwriteInternal()` line 316: both types count in `kmaxOtherSize`.
//!
//! Refused where heclib would misalign data: `ztsRegStoreBlock()` lines
//! 481-485 never compares the stored depths, and `ztsWriteBlock()` overwrites them.

use super::{
    bad_input, block, block::ih, corrupt, iv, missing_block, plan, Areas, BlockPlan, Engine, Plan,
    Prepared, Session,
};
use crate::dss7::ts::{path_parts, MAX_PROFILE_DEPTHS, RTD_PROFILE, RTS_PROFILE};
use crate::dss7::write::profile::{ProfileArrays, ProfileInput};
use crate::dss7::write::{RtsInput, RtsValues, WriteError, WriteOptions, WriteReport};
use crate::timeseries::TsValues;
use block::BlockValues;
use std::fs::File;

/// Most 32-bit words one profile write may hold in memory (1 GiB): every block it touches at
/// full size, depths, element width and quality included. Larger is refused, not attempted.
pub(crate) const MAX_EXPANDED_PROFILE_WORDS: i128 = 1 << 28;

/// The planned blocks of one input, built without touching a file.
pub(crate) struct ProfilePlan {
    plan: Plan,
    depths: usize,
    elem: usize,
    record_type: i32,
    depth_words: Vec<i32>,
}

/// Validate an input and split it into blocks.
pub(crate) fn plan_profile(input: &ProfileInput) -> Result<ProfilePlan, WriteError> {
    let depths = input.data.number_depths();
    if depths == 0 {
        // ztsStore() line 256: no depths is not a profile.
        return Err(bad_input("no depths"));
    }
    if depths > MAX_PROFILE_DEPTHS {
        return Err(bad_input(format!(
            "{depths} depths; the reader takes at most {MAX_PROFILE_DEPTHS}"
        )));
    }
    let total = input.data.number_values();
    if total == 0 {
        return Err(bad_input("no values"));
    }
    if total % depths != 0 {
        return Err(bad_input(format!(
            "{total} values are not a whole number of rows of {depths} depths"
        )));
    }
    let n = total / depths;
    let (elem, record_type) = match input.data {
        ProfileArrays::F32 { .. } => (1usize, RTS_PROFILE),
        ProfileArrays::F64 { .. } => (2usize, RTD_PROFILE),
    };
    // HEC counts everything in 32-bit ints (ztsStoreReg7() lines 204-205, 314).
    let value_size = depths
        .checked_mul(elem)
        .ok_or_else(|| bad_input("too many depths"))?;
    if value_size as u128 * n as u128 > (i32::MAX as u128) / 2 {
        return Err(bad_input(format!(
            "{n} rows of {depths} depths are more than a record holds"
        )));
    }
    if let Some(q) = input.quality {
        if q.len() != n {
            return Err(bad_input(format!(
                "{} quality values for {n} times",
                q.len()
            )));
        }
    }
    super::check_text("depth units", input.depth_units)?;
    super::check_text("value units", input.value_units)?;
    super::check_text("data type", input.data_type)?;
    if let Some(tz) = input.time_zone {
        super::check_text("time zone", tz)?;
    }
    let mut strings = Vec::with_capacity(64);
    block::header_string(Some(input.depth_units), &mut strings);
    block::header_string(Some(input.value_units), &mut strings);
    block::header_string(Some(input.data_type), &mut strings);
    block::header_string(input.time_zone, &mut strings);
    if strings.len() > block::MAX_HEADER_STRING_BYTES {
        return Err(bad_input(format!(
            "units, type and time zone take {} header bytes; HEC reads back at most {}",
            strings.len(),
            block::MAX_HEADER_STRING_BYTES
        )));
    }
    if !(-1..=127).contains(&input.precision) {
        return Err(bad_input("precision must be -1 (unset) or 0..=127"));
    }
    // The regular writer's planning takes the number of times from the values; the strings are replaced.
    let times = vec![0f32; n];
    let scalar = RtsInput::new(
        input.pathname,
        input.start,
        RtsValues::F32(&times),
        b"",
        b"",
    );
    let mut p = plan(&scalar)?;
    p.strings = strings;
    // A partial block is materialized whole (ztsRegReadBlock(), lines 381-399), as is every
    // block with quality (ztsRegStoreBlock() line 414), and all blocks are held until the
    // first commit: bound what the plan expands to, not only what the caller supplied.
    let row_words = value_size as i128 + i128::from(input.quality.is_some());
    let mut expanded: i128 = 0;
    for bp in &p.blocks {
        let rows = if bp.read_block || input.quality.is_some() {
            bp.number_in_block
        } else {
            bp.number_to_store
        };
        expanded += i128::from(rows) * row_words;
    }
    if expanded > MAX_EXPANDED_PROFILE_WORDS {
        return Err(bad_input(format!(
            "this write would expand to {expanded} words in memory (rows of the whole blocks \
             it touches x {depths} depths); the limit is {MAX_EXPANDED_PROFILE_WORDS}. Use a \
             longer interval, fewer depths or whole blocks"
        )));
    }
    let depth_words = match input.data {
        ProfileArrays::F32 { depths, .. } => BlockValues::F32(depths.to_vec()),
        ProfileArrays::F64 { depths, .. } => BlockValues::F64(depths.to_vec()),
    }
    .words(0, depths);
    Ok(ProfilePlan {
        plan: p,
        depths,
        elem,
        record_type,
        depth_words,
    })
}

/// `ztsFirstLastValidFloat/Double()` with `profileDepthsNumber > 0`. All missing gives
/// positions (0, 0).
fn first_last_rows(v: &BlockValues, depths: usize, quality: Option<&[i32]>) -> (usize, usize) {
    let n = v.len() / depths;
    let valid = |r: usize| {
        (0..depths).any(|d| !v.is_missing(r * depths + d)) || quality.is_some_and(|q| q[r] != 0)
    };
    match (0..n).find(|&r| valid(r)) {
        None => (0, 0),
        Some(first) => (first, (0..n).rev().find(|&r| valid(r)).unwrap_or(first)),
    }
}

impl Engine<'_> {
    /// `ztsRegStoreBlock()` for one block, with every refusal before any commit.
    fn prepare_profile_block(
        &self,
        input: &ProfileInput,
        pp: &ProfilePlan,
        bp: &BlockPlan,
    ) -> Result<Prepared, WriteError> {
        let plan = &pp.plan;
        let nd = pp.depths;
        let path = plan.path.block_pathname(bp.julian);
        let interval = plan.interval;
        let from = bp.position_in_values as usize;
        let count = bp.number_to_store as usize;
        let new_values = match input.data {
            ProfileArrays::F32 { values, .. } => {
                BlockValues::F32(values[from * nd..(from + count) * nd].to_vec())
            }
            ProfileArrays::F64 { values, .. } => {
                BlockValues::F64(values[from * nd..(from + count) * nd].to_vec())
            }
        };
        let new_quality: Option<Vec<i32>> = input.quality.map(|q| q[from..from + count].to_vec());
        let elem = pp.elem as i64;
        let value_size = elem * nd as i64;
        let qes = i64::from(new_quality.is_some());

        let (mut f, mut l) = first_last_rows(&new_values, nd, new_quality.as_deref());
        let stamp =
            |pos: i64| iv::increment_time(interval, pos, plan.start_julian, plan.start_seconds);
        let mut dates = (
            stamp(bp.position_in_values + f as i64),
            stamp(bp.position_in_values + l as i64),
        );

        let found = self.lookup_for_write(&path)?;
        let mut old_ih: Vec<i32> = Vec::new();
        let mut same_depths = false;
        if let Some(r) = &found {
            let t = r.record_type;
            if !matches!(t, RTS_PROFILE | RTD_PROFILE) || r.info.record_type != t {
                return Err(WriteError::Unsupported(format!(
                    "{}: the existing record has type {t} (info {}), not a regular profile",
                    String::from_utf8_lossy(&path),
                    r.info.record_type
                )));
            }
            if t != pp.record_type {
                // ztsRegStoreBlock() line 844 widens only scalar RTS into RTD; a profile's
                // depths and values would change width with the stored rows left behind.
                return Err(WriteError::Unsupported(format!(
                    "{}: the existing profile has type {t}; the writer does not change a \
                     profile's width (give {})",
                    String::from_utf8_lossy(&path),
                    if t == RTS_PROFILE {
                        "float32"
                    } else {
                        "float64"
                    }
                )));
            }
            old_ih = self.read_area(&r.info.internal_header)?;
            let g = |i: usize| old_ih.get(i).copied().unwrap_or(0);
            if g(ih::INOTES_ELEMENT_SIZE) != 0
                || g(ih::CNOTES_LENGTH) != 0
                || g(ih::INOTES_NUMBER) != 0
            {
                return Err(WriteError::Unsupported(
                    "the existing block stores notes".into(),
                ));
            }
            if g(ih::QUALITY_ELEMENT_SIZE) > 1 {
                return Err(WriteError::Unsupported(
                    "the existing block stores multi-word quality".into(),
                ));
            }
            same_depths = g(ih::PROFILE_DEPTHS) as i64 == nd as i64
                && self.read_area(&r.info.values2)? == pp.depth_words;
        }
        let gih = |i: usize| i64::from(old_ih.get(i).copied().unwrap_or(0));

        // ztsRegStoreBlock() lines 345-416.
        let mut read_block = bp.read_block;
        if let Some(r) = &found {
            if read_block && r.info.logical_number < 1 {
                read_block = false;
            }
            if read_block
                && value_size == gih(ih::VALUE_SIZE)
                && qes == gih(ih::QUALITY_ELEMENT_SIZE)
                && gih(ih::INOTES_ELEMENT_SIZE) == 0
                && gih(ih::CNOTES_LENGTH) == 0
            {
                let (s, e) = if new_quality.is_none() {
                    let s = bp.block_start_position + f as i64;
                    (s, s + l as i64 - f as i64)
                } else {
                    (
                        bp.block_start_position,
                        bp.block_start_position + bp.number_to_store - 1,
                    )
                };
                if s == gih(ih::BLOCK_START) && e == gih(ih::BLOCK_END) {
                    read_block = false;
                }
            }
        }
        if !read_block && new_quality.is_some() {
            read_block = true;
        }
        if read_block && found.is_some() && !same_depths {
            return Err(WriteError::Unsupported(format!(
                "{}: the existing block has different depths; heclib would keep its rows under \
                 the new depths (ztsRegStoreBlock.c lines 481-485). Replace the whole block, or \
                 write the stored depths",
                String::from_utf8_lossy(&path)
            )));
        }

        let nib = bp.number_in_block as usize;
        let (values, quality, mut bsp) = if read_block {
            // ztsRegReadBlock(): the whole block, missing where nothing is stored.
            let (mut full, mut fq) = match &found {
                Some(r) => {
                    let mut rdr = self.reader()?;
                    let t = rdr.read_ts_profile_record(r).map_err(WriteError::Ts)?;
                    if t.values.len() != nib * nd {
                        return Err(corrupt(format!(
                            "existing block holds {} values, expected {nib} rows of {nd}",
                            t.values.len()
                        )));
                    }
                    let v = match t.values {
                        TsValues::F32(v) => BlockValues::F32(v),
                        TsValues::F64(v) => BlockValues::F64(v),
                    };
                    let q = t.meta.quality_words.unwrap_or_else(|| vec![0; nib]);
                    if q.len() != nib {
                        return Err(corrupt("existing block's quality length"));
                    }
                    (v, q)
                }
                None => (missing_block(nib * nd, pp.elem == 2), vec![0; nib]),
            };
            let at = bp.block_start_position as usize;
            match (&mut full, &new_values) {
                (BlockValues::F32(a), BlockValues::F32(b)) => {
                    a[at * nd..(at + count) * nd].copy_from_slice(b)
                }
                (BlockValues::F64(a), BlockValues::F64(b)) => {
                    a[at * nd..(at + count) * nd].copy_from_slice(b)
                }
                _ => return Err(corrupt("stored width differs from its record type")),
            }
            if let Some(q) = &new_quality {
                fq[at..at + q.len()].copy_from_slice(q);
            }
            let fq = new_quality.as_ref().map(|_| fq);
            (f, l) = first_last_rows(&full, nd, fq.as_deref());
            // Lines 783-788: dates from the block start, without the time offset.
            let stamp_block =
                |pos: usize| iv::increment_time(interval, pos as i64, bp.julian, interval);
            dates = (stamp_block(f), stamp_block(l));
            (full, fq, 0i64)
        } else {
            (new_values, new_quality, bp.block_start_position)
        };

        // Lines 816-836.
        let (first_row, to_store) = if quality.is_some() {
            bsp = 0;
            (0usize, nib)
        } else {
            bsp += f as i64;
            (f, l - f + 1)
        };
        let ((jf, sf), (jl, sl)) = dates;
        let mut fixed = [0i32; ih::FIXED];
        fixed[ih::TIME_GRANULARITY] = plan.granularity as i32;
        fixed[ih::PRECISION] = input.precision;
        fixed[ih::TIME_OFFSET] = plan.time_offset as i32;
        fixed[ih::PROFILE_DEPTHS] = nd as i32;
        fixed[ih::BLOCK_START] = bsp as i32;
        fixed[ih::BLOCK_END] = (bsp + to_store as i64 - 1) as i32;
        fixed[ih::VALUE_SIZE] = value_size as i32;
        fixed[ih::VALUE_ELEMENT_SIZE] = elem as i32;
        // ztsAggregate(), uncompressed branch: values, even pad, quality, even pad.
        let mut v1 = values.words(first_row * nd, to_store * nd);
        fixed[ih::VALUES_NUMBER] = v1.len() as i32;
        if v1.len() % 2 == 1 {
            v1.push(0);
        }
        if let Some(q) = &quality {
            fixed[ih::QUALITY_NUMBER] = to_store as i32;
            fixed[ih::QUALITY_ELEMENT_SIZE] = 1;
            v1.extend_from_slice(&q[first_row..first_row + to_store]);
            if v1.len() % 2 == 1 {
                v1.push(0);
            }
        }
        let ihw = block::pack_internal_header(&fixed, &plan.strings);
        let areas = Areas {
            data_type: pp.record_type,
            ih: ihw,
            h2: Vec::new(),
            // As for scalar blocks: heclib never keeps an existing user header here.
            uh: Vec::new(),
            v1,
            v2: pp.depth_words.clone(),
            v3: Vec::new(),
            number_data: to_store as i64,
            logical: bp.number_in_block,
            first: (jf as i32, sf as i32),
            last: (jl as i32, sl as i32),
            total_expanded: bp.number_in_block * (value_size + qes),
            copy_out: false,
        };
        Ok(Prepared { path, found, areas })
    }

    /// Readers cannot stitch a dataset whose blocks differ in depths or width
    /// (HEC misaligns them; the native reader refuses). A block outside this write must
    /// already match.
    fn check_profile_dataset(
        &self,
        pp: &ProfilePlan,
        prepared: &[Prepared],
    ) -> Result<(), WriteError> {
        let Some(ours) = prepared.first().and_then(|p| path_parts(&p.path)) else {
            return Ok(());
        };
        let eq = |a: &[u8], b: &[u8]| a.eq_ignore_ascii_case(b);
        let mut rdr = self.reader()?;
        let cat = rdr.catalog()?;
        for i in cat.resolved_live() {
            let e = &cat.catalog.entries[i];
            if !matches!(e.record_type, RTS_PROFILE | RTD_PROFILE) {
                continue;
            }
            let Some(p) = path_parts(&e.pathname) else {
                continue;
            };
            if ![0usize, 1, 2, 4, 5].iter().all(|&k| eq(p[k], ours[k]))
                || prepared.iter().any(|x| eq(&x.path, &e.pathname))
            {
                continue;
            }
            let Some(r) = self.lookup_for_write(&e.pathname)? else {
                continue;
            };
            let old = self.read_area(&r.info.internal_header)?;
            if r.record_type != pp.record_type
                || old.get(ih::PROFILE_DEPTHS).copied().unwrap_or(0) as usize != pp.depths
                || self.read_area(&r.info.values2)? != pp.depth_words
            {
                return Err(WriteError::Unsupported(format!(
                    "block {} of this dataset has a different width or depths; a dataset keeps \
                     one depth axis, and HEC readers misalign blocks that differ",
                    String::from_utf8_lossy(&e.pathname)
                )));
            }
        }
        Ok(())
    }
}

/// Store a profile: every block prepared first, then one transaction per block.
pub(crate) fn write_profile_planned(
    file: &File,
    input: &ProfileInput,
    pp: &ProfilePlan,
    opts: &WriteOptions,
) -> Result<WriteReport, WriteError> {
    let mut session = Session::begin(file, opts)?;
    let prepared = pp
        .plan
        .blocks
        .iter()
        .map(|bp| session.eng.prepare_profile_block(input, pp, bp))
        .collect::<Result<Vec<_>, _>>()?;
    session.eng.check_profile_dataset(pp, &prepared)?;
    // The scalar writers' checks and planning, before the first store.
    for p in &prepared {
        session.eng.preflight(p)?;
    }
    session.eng.plan_stores(&prepared)?;
    let count = prepared.len();
    let mut blocks = Vec::with_capacity(count);
    for (i, p) in prepared.into_iter().enumerate() {
        session.eng.block_index = i;
        blocks.push(session.eng.commit(p).map_err(|e| e.after_planning(count))?);
    }
    session.finish(blocks)
}
