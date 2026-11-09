//! DSS7 store of the time-series variants: multi-word quality, integer and character notes, and
//! pattern records, on the regular writer's records and transactions.
//!
//! Regular blocks follow `Internal/ztsRegStoreBlock.c:ztsRegStoreBlock()` with storage
//! flag 0, irregular blocks `Internal/ztsIrregStoreBlock.c:ztsIrregStoreBlock()` and
//! `Internal/ztsIrregMergeBlocks.c:ztsIrregMergeBlocks()` with storage flag 1, patterns
//! `SemiPublic/ztsStorePattern.c:ztsStorePattern()`. Paths relative to
//! `heclib/heclib_c/src/` of `hec-dss` tag `7-IU-15`.
//!
//! A write with no optional array, into a block with no notes or multi-word quality, stays
//! on the regular and irregular code paths.
//!
//! Deliberate differences from heclib, as observed in the tested build and synthetic cases
//! (observed with HEC's C library and Monolith on synthetic files):
//! - alignment-preserving policy: a write without notes that keeps stored character notes,
//!   and whose first valid value is not at the block start, stores the whole block; the tested
//!   heclib case trims the leading values and keeps every note;
//! - irregular notes after the time window stay with their values; `ztsIrregMergeBlocks()`
//!   restarts `icnotesPosRead` at 0 there, and the tested build associated them differently.

use super::*;
use crate::dss7::ts::{ITD_PATTERN, ITS_PATTERN, RTD, RTD_PATTERN, RTS, RTS_PATTERN};
use crate::dss7::write::path::validate_pattern_pathname;
use crate::dss7::write::variants::{
    aggregate_full, check_block_budget, check_extras, first_last_full, join_cnotes,
    resize_elements, split_cnotes, AggregateInput, Extras, PatternInput, TsVariants,
};

/// What a stored time-series record holds beyond its values.
#[derive(Debug, Clone, Copy, Default)]
struct StoredArrays {
    qes: usize,
    nes: usize,
    cnotes_length: i64,
    depths: i32,
}

impl StoredArrays {
    /// Blocks the regular and irregular paths refuse.
    fn needs_variant_path(&self, new_quality: bool) -> bool {
        self.nes > 0 || self.cnotes_length > 0 || (new_quality && self.qes > 1)
    }
}

impl Engine<'_> {
    /// The last checks on a built block, before any block of the write is committed:
    /// the reader's limits on what was built, and every area of the record
    /// it replaces, including areas being dropped, because the commit's equality test reads
    /// them. A damaged area in a later block therefore refuses with the file unchanged.
    pub(super) fn preflight(&self, p: &Prepared) -> Result<(), WriteError> {
        let w = |i: usize| i64::from(p.areas.ih.get(i).copied().unwrap_or(0));
        check_block_budget(
            p.areas.number_data,
            w(ih::VALUE_SIZE).max(1),
            w(ih::QUALITY_ELEMENT_SIZE),
            w(ih::INOTES_ELEMENT_SIZE),
            w(ih::CNOTES_LENGTH),
        )?;
        if let Some(r) = &p.found {
            let i = &r.info;
            for a in [
                &i.internal_header,
                &i.header2,
                &i.user_header,
                &i.values1,
                &i.values2,
                &i.values3,
            ] {
                self.read_area(a)?;
            }
        }
        Ok(())
    }

    /// Run the commits of a multi-record write with the file untouched: writes
    /// are held in an overlay, reads see them, and the engine's state is restored afterwards.
    /// Every refusal that depends on the file, including bin allocation and chain links after
    /// the earlier records, comes from here. A single record refuses before its first write.
    pub(super) fn plan_commits(
        &mut self,
        count: usize,
        run: impl FnOnce(&mut Self) -> Result<(), WriteError>,
    ) -> Result<(), WriteError> {
        if count < 2 {
            return Ok(());
        }
        let saved = (self.hdr, self.disk, self.block_index);
        *self.plan.borrow_mut() = Some(overlay::Overlay::new());
        let r = run(self);
        *self.plan.borrow_mut() = None;
        (self.hdr, self.disk, self.block_index) = saved;
        r
    }

    /// [`Engine::plan_commits`] for stored blocks.
    pub(super) fn plan_stores(&mut self, prepared: &[Prepared]) -> Result<(), WriteError> {
        self.plan_commits(prepared.len(), |eng| {
            for (i, p) in prepared.iter().enumerate() {
                eng.block_index = i;
                eng.commit_areas(p.path.clone(), p.found.clone(), &p.areas)?;
            }
            Ok(())
        })
    }

    fn stored_arrays(&self, r: &RecordRef) -> Result<StoredArrays, WriteError> {
        let w = self.read_area(&r.info.internal_header)?;
        let g = |i: usize| w.get(i).copied().unwrap_or(0);
        let size = |x: i32| -> Result<usize, WriteError> {
            usize::try_from(x)
                .ok()
                .filter(|&k| k <= 1 << 20)
                .ok_or_else(|| corrupt("stored element size out of range"))
        };
        Ok(StoredArrays {
            qes: size(g(ih::QUALITY_ELEMENT_SIZE))?,
            nes: size(g(ih::INOTES_ELEMENT_SIZE))?,
            cnotes_length: i64::from(g(ih::CNOTES_LENGTH)),
            depths: g(ih::PROFILE_DEPTHS),
        })
    }

    /// One regular block: chooses the regular path, or the port of the full
    /// `ztsRegStoreBlock()` below.
    fn prepare_block_v(
        &self,
        input: &RtsInput,
        plan: &Plan,
        bp: &BlockPlan,
        x: &Extras,
    ) -> Result<Prepared, WriteError> {
        let path = plan.path.block_pathname(bp.julian);
        let found = self.lookup_for_write(&path)?;
        let stored = match &found {
            Some(r)
                if (r.record_type == RTS || r.record_type == RTD)
                    && r.info.record_type == r.record_type =>
            {
                Some(self.stored_arrays(r)?)
            }
            _ => None,
        };
        // Single-word quality given as the input's `quality` is what the regular path stores.
        let single = input.quality.is_some() && x.nes == 0 && x.cnotes.is_none();
        if (!x.any() || single) && !stored.is_some_and(|s| s.needs_variant_path(single)) {
            return self.prepare_block(input, plan, bp);
        }
        if let Some(r) = &found {
            if r.record_type != RTS && r.record_type != RTD || r.info.record_type != r.record_type {
                return Err(WriteError::Unsupported(format!(
                    "existing record type {} (info {}) is not a regular float/double time \
                     series (HEC DIFFERENT_RECORD_TYPE)",
                    r.record_type, r.info.record_type
                )));
            }
        }
        let st = stored.unwrap_or_default();
        if st.depths != 0 {
            return Err(WriteError::Unsupported(
                "the existing block stores profile depths".into(),
            ));
        }
        let (from, count) = (bp.position_in_values as usize, bp.number_to_store as usize);
        let new_values = match input.values {
            RtsValues::F32(v) => BlockValues::F32(v[from..from + count].to_vec()),
            RtsValues::F64(v) => BlockValues::F64(v[from..from + count].to_vec()),
        };
        let (qes, nes) = (x.qes, x.nes);
        let new_q = x.quality.map(|q| &q[from * qes..(from + count) * qes]);
        let new_n = x.inotes.map(|q| &q[from * nes..(from + count) * nes]);
        let new_c = x.cnotes.map(|c| &c[from..from + count]);
        let to_double = matches!(new_values, BlockValues::F64(_))
            || found.as_ref().is_some_and(|r| r.record_type == RTD);
        let new_values = widen(new_values, to_double);
        let elem = new_values.element_size();
        let interval = plan.interval;
        let nib = bp.number_in_block as usize;

        // The first/last valid of the new arrays, kept when the block is not read.
        let nv = &new_values;
        let (_, mut f, mut l) = first_last_full(
            &|i| nv.is_missing(i),
            count,
            new_q.map(|q| (q, qes)),
            new_n.map(|q| (q, nes)),
            None,
        );
        let stamp =
            |pos: i64| iv::increment_time(interval, pos, plan.start_julian, plan.start_seconds);
        let mut dates = (
            stamp(bp.position_in_values + f as i64),
            stamp(bp.position_in_values + l as i64),
        );
        // `ztsRegStoreBlock()`: any new array forces the read of the full block (a block
        // the series spans is then not looked up, so nothing stored survives: every
        // position is overwritten), and a write without arrays never matches stored notes
        // in the "same lengths" test, so it reads as well.
        let mut read_block = bp.read_block;
        if let Some(r) = &found {
            if read_block && r.info.logical_number < 1 {
                read_block = false;
            }
        }
        let use_stored = read_block;
        if x.any() {
            read_block = true;
        }
        if use_stored && found.is_some() {
            refuse_inotes_over_cnotes(st, nes, &path)?;
        }

        let block_at = bp.block_start_position as usize;
        let mut kept_cnotes = false;
        let (values, quality, inotes, cnotes, mut bsp) = if read_block {
            // ztsRegReadBlock(): the whole block, missing and undefined where nothing is
            // stored.
            let (mut full, mut fq, mut fnotes, stored_c) = match (&found, use_stored) {
                (Some(r), true) => {
                    let mut rdr = self.reader()?;
                    let t = rdr.read_ts_record(r).map_err(WriteError::Ts)?;
                    if t.ts.values.len() != nib {
                        return Err(corrupt(format!(
                            "existing block holds {} positions, expected {nib}",
                            t.ts.values.len()
                        )));
                    }
                    let v = match t.ts.values {
                        TsValues::F32(v) => BlockValues::F32(v),
                        TsValues::F64(v) => BlockValues::F64(v),
                    };
                    let stored_q = t
                        .meta
                        .quality_words
                        .clone()
                        .or(t.ts.quality.clone())
                        .unwrap_or_default();
                    let q = resize_elements(&stored_q, st.qes, qes, nib);
                    let stored_n = t.meta.inotes.clone().unwrap_or_default();
                    let n = resize_elements(&stored_n, st.nes, nes, nib);
                    // The reader maps note bytes one to one onto chars.
                    let c = t.meta.cnotes.as_ref().map(|c| {
                        c.iter()
                            .map(|s| s.chars().map(|ch| ch as u32 as u8).collect::<Vec<u8>>())
                            .collect::<Vec<_>>()
                    });
                    (widen(v, to_double), q, n, c)
                }
                _ => (
                    missing_block(nib, to_double),
                    vec![0; nib * qes],
                    vec![0; nib * nes],
                    None,
                ),
            };
            overwrite(&mut full, block_at, &new_values);
            if let Some(q) = new_q {
                fq[block_at * qes..(block_at + count) * qes].copy_from_slice(q);
            }
            if let Some(q) = new_n {
                fnotes[block_at * nes..(block_at + count) * nes].copy_from_slice(q);
            }
            let c: Option<Vec<u8>> = match (new_c, stored_c) {
                (Some(nc), s) => {
                    let mut all = s.unwrap_or_else(|| vec![Vec::new(); nib]);
                    for (i, one) in nc.iter().enumerate() {
                        all[block_at + i] = one.to_vec();
                    }
                    Some(join_cnotes(&all))
                }
                // "Notes read, but none to store": kept as they are.
                (None, Some(s)) => {
                    kept_cnotes = true;
                    Some(join_cnotes(&s))
                }
                (None, None) => None,
            };
            let fullr = &full;
            (_, f, l) = first_last_full(
                &|i| fullr.is_missing(i),
                nib,
                (qes > 0).then_some((&fq[..], qes)),
                (nes > 0).then_some((&fnotes[..], nes)),
                c.as_deref(),
            );
            // Dates from the block start, without the time offset.
            let stamp_block =
                |pos: usize| iv::increment_time(interval, pos as i64, bp.julian, interval);
            dates = (stamp_block(f), stamp_block(l));
            (full, fq, fnotes, c, 0i64)
        } else {
            (
                new_values,
                Vec::new(),
                Vec::new(),
                None,
                bp.block_start_position,
            )
        };

        // Quality or notes store the full block.
        let (first_index, to_store) = if x.any() || (kept_cnotes && f > 0) {
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
        fixed[ih::BLOCK_START] = bsp as i32;
        fixed[ih::BLOCK_END] = (bsp + to_store as i64 - 1) as i32;
        fixed[ih::VALUE_ELEMENT_SIZE] = elem as i32;
        let words = values.words(first_index, to_store);
        let (v1, h2) = aggregate_full(
            &AggregateInput {
                n: to_store,
                times: None,
                values: &words,
                value_size: elem,
                quality: &quality,
                qes,
                inotes: &inotes,
                nes,
                cnotes: cnotes.as_deref().unwrap_or(b""),
            },
            &mut fixed,
        );
        let areas = Areas {
            data_type: if to_double { RTD } else { RTS },
            ih: block::pack_internal_header(&fixed, &plan.strings),
            h2,
            uh: Vec::new(),
            v1,
            v2: Vec::new(),
            v3: Vec::new(),
            number_data: to_store as i64,
            logical: bp.number_in_block,
            first: (jf as i32, sf as i32),
            last: (jl as i32, sl as i32),
            // ztsWriteBlock() line 199.
            total_expanded: bp.number_in_block * (elem + qes + nes) as i64,
            copy_out: false,
        };
        Ok(Prepared { path, found, areas })
    }

    /// Refusals for an existing irregular record once notes and multi-word quality can be
    /// stored: only ITS/ITD records without profile depths.
    fn check_existing_its_v(&self, r: &RecordRef) -> Result<StoredArrays, WriteError> {
        if r.record_type != ITS && r.record_type != ITD || r.info.record_type != r.record_type {
            return Err(WriteError::Unsupported(format!(
                "existing record {} has type {} (info {}), not an irregular float/double \
                 time series (HEC DIFFERENT_RECORD_TYPE)",
                String::from_utf8_lossy(&r.pathname),
                r.record_type,
                r.info.record_type
            )));
        }
        let st = self.stored_arrays(r)?;
        if st.depths != 0 {
            return Err(WriteError::Unsupported(format!(
                "the existing block {} stores profile depths",
                String::from_utf8_lossy(&r.pathname)
            )));
        }
        Ok(st)
    }

    /// One irregular block: the irregular path, or the port of the full `ztsIrregStoreBlock()`.
    fn prepare_its_block_v(
        &self,
        input: &ItsInput,
        plan: &ItsPlan,
        bp: &ItsBlockPlan,
        x: &Extras,
    ) -> Result<Prepared, WriteError> {
        let path = plan.path.block_pathname(bp.julian);
        let found = self.lookup_for_write(&path)?;
        let st = match &found {
            Some(r) => self.check_existing_its_v(r)?,
            None => StoredArrays::default(),
        };
        let single = input.quality.is_some() && x.nes == 0 && x.cnotes.is_none();
        if (!x.any() || single) && st.nes == 0 && st.cnotes_length <= 0 && st.qes <= 1 {
            return self.prepare_its_block(input, plan, bp);
        }
        let century = plan.century();
        let unit = plan.unit();
        let base = bp.julian * iv::SECS_IN_1_DAY;
        let (from, count) = (bp.from, bp.count);
        let (qes, nes) = (x.qes, x.nes);
        let new_times: Vec<i32> = input.times[from..from + count]
            .iter()
            .map(|&t| ((t - base) / unit) as i32)
            .collect();
        let new_values = match input.values {
            RtsValues::F32(v) => BlockValues::F32(v[from..from + count].to_vec()),
            RtsValues::F64(v) => BlockValues::F64(v[from..from + count].to_vec()),
        };
        let to_double = matches!(new_values, BlockValues::F64(_))
            || found.as_ref().is_some_and(|r| r.record_type == ITD);
        let new_values = widen(new_values, to_double);
        let elem = new_values.element_size();
        let new_c: Option<Vec<Vec<u8>>> = x
            .cnotes
            .map(|c| c[from..from + count].iter().map(|s| s.to_vec()).collect());
        let new = BlockV {
            times: new_times,
            values: new_values,
            quality: x
                .quality
                .map_or_else(Vec::new, |q| q[from * qes..(from + count) * qes].to_vec()),
            inotes: x
                .inotes
                .map_or_else(Vec::new, |q| q[from * nes..(from + count) * nes].to_vec()),
            cnotes: new_c,
        };
        let read = bp.read_block && found.as_ref().is_some_and(|r| r.info.logical_number >= 1);
        if read {
            refuse_inotes_over_cnotes(st, nes, &path)?;
        }
        let data = match (&found, read) {
            (Some(r), true) => {
                let old = self.read_its_block_v(r, to_double, qes, nes, st)?;
                merge_replace_v(old, new, bp.from_start, bp.to_end, qes, nes)
            }
            _ => new,
        };
        let total = data.times.len();
        let mut gran = plan.granularity;
        if data
            .times
            .iter()
            .any(|&t| (i64::from(t) * unit) % gran != 0)
        {
            if plan.granularity_explicit {
                return Err(bad_input(format!(
                    "stored values kept in block {} are not whole multiples of the \
                     {gran}-second granularity",
                    String::from_utf8_lossy(&path)
                )));
            }
            gran = unit;
        }
        let first = its_stamp(bp.julian, data.times[0], century);
        let last = its_stamp(bp.julian, data.times[total - 1], century);
        let logical = its_logical(bp.julian, bp.next, first, last, total as i64);
        let mut fixed = [0i32; ih::FIXED];
        fixed[ih::TIME_GRANULARITY] = gran as i32;
        fixed[ih::PRECISION] = input.precision;
        fixed[ih::VALUE_ELEMENT_SIZE] = elem as i32;
        let words = data.values.words(0, total);
        let cnotes = data.cnotes.as_ref().map(|c| join_cnotes(c));
        let (v1, h2) = aggregate_full(
            &AggregateInput {
                n: total,
                times: Some(&data.times),
                values: &words,
                value_size: elem,
                quality: &data.quality,
                qes,
                inotes: &data.inotes,
                nes,
                cnotes: cnotes.as_deref().unwrap_or(b""),
            },
            &mut fixed,
        );
        let areas = Areas {
            data_type: if to_double { ITD } else { ITS },
            ih: block::pack_internal_header(&fixed, &plan.strings),
            h2,
            uh: Vec::new(),
            v1,
            v2: Vec::new(),
            v3: Vec::new(),
            number_data: total as i64,
            logical,
            first: (first.0 as i32, first.1 as i32),
            last: (last.0 as i32, last.1 as i32),
            // ztsWriteBlock() lines 199-203.
            total_expanded: logical * (elem + qes + nes) as i64 + logical,
            copy_out: false,
        };
        Ok(Prepared { path, found, areas })
    }

    /// A stored irregular block in full (`ztsIrregReadBlock()`): quality and integer notes
    /// at the requested element sizes, truncated or zero-filled as `ztsDisaggregate()` does.
    fn read_its_block_v(
        &self,
        r: &RecordRef,
        to_double: bool,
        qes: usize,
        nes: usize,
        st: StoredArrays,
    ) -> Result<BlockV, WriteError> {
        let w = self.read_area(&r.info.internal_header)?;
        let h2 = self.read_area(&r.info.header2)?;
        let v1 = self.read_area(&r.info.values1)?;
        let n = r.info.number_data;
        if n < 0 || n as usize > v1.len() {
            return Err(corrupt(
                "the existing block's value count exceeds its values area",
            ));
        }
        let elem = if to_double { 2 } else { 1 };
        // Read at the stored sizes; resize below, so a request of 0 stays an empty array.
        let d = Disaggregate {
            data_in: &v1,
            data_in_position: n as usize,
            header2: &h2,
            ih: &w,
            number_to_read: n,
            number_stored: n,
            position_relative_first_valid: 0,
            position_relative_last_valid: 0,
            deprecated: false,
            values_requested: elem,
            quality_requested: st.qes,
            inotes_requested: st.nes,
            want_cnotes: st.cnotes_length > 0,
        };
        let e = d
            .run()
            .map_err(|e| corrupt(format!("the existing block does not decode: {}", e.0)))?;
        let n = n as usize;
        if e.values.len() < n * elem || e.quality.len() < n * st.qes || e.inotes.len() < n * st.nes
        {
            return Err(corrupt(
                "the existing block decodes to fewer values than stored",
            ));
        }
        let values = if to_double {
            BlockValues::F64(
                (0..n)
                    .map(|i| f64::from_bits(i4toi8(e.values[2 * i], e.values[2 * i + 1]) as u64))
                    .collect(),
            )
        } else {
            BlockValues::F32(
                e.values[..n]
                    .iter()
                    .map(|&w| f32::from_bits(w as u32))
                    .collect(),
            )
        };
        Ok(BlockV {
            times: v1[..n].to_vec(),
            values,
            quality: resize_elements(&e.quality[..n * st.qes], st.qes, qes, n),
            inotes: resize_elements(&e.inotes[..n * st.nes], st.nes, nes, n),
            cnotes: (st.cnotes_length > 0).then(|| split_cnotes(&e.cnotes, n)),
        })
    }
}

/// Integer notes into a block that keeps stored character notes: the two share one storage
/// area. From `ztsAggregate()`, the tested compressed case stores `INT_HEAD_cnotesLength` with
/// no characters behind it (line 719 tests `inoteElementSize == 0`), which the native decoder
/// rejects; the uncompressed branch (line 443) was not qualified. Mixed representations are
/// not qualified, so the writer refuses.
fn refuse_inotes_over_cnotes(st: StoredArrays, nes: usize, path: &[u8]) -> Result<(), WriteError> {
    if nes > 0 && st.cnotes_length > 0 {
        return Err(WriteError::Unsupported(format!(
            "{}: integer notes into a block that stores character notes (they share one \
             storage area; write the whole block, or delete the record first)",
            String::from_utf8_lossy(path)
        )));
    }
    Ok(())
}

/// One irregular block with every array.
struct BlockV {
    times: Vec<i32>,
    values: BlockValues,
    quality: Vec<i32>,
    inotes: Vec<i32>,
    cnotes: Option<Vec<Vec<u8>>>,
}

/// `ztsIrregMergeBlocks()`, storage flag 1, with the keep rule of [`merge_replace`]: every
/// stored value outside the window stays, with its own quality and notes.
fn merge_replace_v(
    old: BlockV,
    new: BlockV,
    from_start: bool,
    to_end: bool,
    qes: usize,
    nes: usize,
) -> BlockV {
    let first = new.times[0];
    let last = new.times[new.times.len() - 1];
    let n_old = old.times.len();
    let pre: Vec<usize> = (0..n_old)
        .take_while(|&i| !from_start && old.times[i] < first)
        .collect();
    let post: Vec<usize> = (0..n_old)
        .filter(|&i| !to_end && old.times[i] > last)
        .collect();
    let idx: Vec<(bool, usize)> = pre
        .iter()
        .map(|&i| (false, i))
        .chain((0..new.times.len()).map(|i| (true, i)))
        .chain(post.iter().map(|&i| (false, i)))
        .collect();
    let times = idx
        .iter()
        .map(|&(isnew, i)| if isnew { new.times[i] } else { old.times[i] })
        .collect();
    let values = match (&old.values, &new.values) {
        (BlockValues::F32(o), BlockValues::F32(v)) => BlockValues::F32(
            idx.iter()
                .map(|&(isnew, i)| if isnew { v[i] } else { o[i] })
                .collect(),
        ),
        (BlockValues::F64(o), BlockValues::F64(v)) => BlockValues::F64(
            idx.iter()
                .map(|&(isnew, i)| if isnew { v[i] } else { o[i] })
                .collect(),
        ),
        // Both sides are read or widened to the same width before merging.
        _ => unreachable!("merge widths differ"),
    };
    let pick = |o: &[i32], n: &[i32], k: usize| -> Vec<i32> {
        let mut out = Vec::with_capacity(idx.len() * k);
        if k > 0 {
            for &(isnew, i) in &idx {
                let src = if isnew { n } else { o };
                out.extend_from_slice(&src[i * k..(i + 1) * k]);
            }
        }
        out
    };
    let quality = pick(&old.quality, &new.quality, qes);
    let inotes = pick(&old.inotes, &new.inotes, nes);
    // A side without notes contributes empty notes (`cnotesOut[icnotesPosOut++] = '\0'`).
    let cnotes = (old.cnotes.is_some() || new.cnotes.is_some()).then(|| {
        idx.iter()
            .map(|&(isnew, i)| {
                let side = if isnew { &new.cnotes } else { &old.cnotes };
                side.as_ref().map_or_else(Vec::new, |c| c[i].clone())
            })
            .collect()
    });
    BlockV {
        times,
        values,
        quality,
        inotes,
        cnotes,
    }
}

// ---------------------------------------------------------------------------------
// Entry points
// ---------------------------------------------------------------------------------

pub(in crate::dss7::write) fn write_rts_v(
    file: &File,
    input: &RtsInput,
    plan: &Plan,
    v: &TsVariants,
    opts: &WriteOptions,
) -> Result<WriteReport, WriteError> {
    let x = check_extras(input.values.len(), input.quality, v)?;
    let mut session = Session::begin(file, opts)?;
    // Preflight every block before the first commit (without it, a refusal in block k
    // would leave blocks 0..k-1 written).
    let mut prepared = Vec::with_capacity(plan.blocks.len());
    for bp in &plan.blocks {
        let p = session.eng.prepare_block_v(input, plan, bp, &x)?;
        session.eng.preflight(&p)?;
        prepared.push(p);
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

pub(in crate::dss7::write) fn check_rts_variants(
    input: &RtsInput,
    v: &TsVariants,
) -> Result<(), WriteError> {
    check_extras(input.values.len(), input.quality, v).map(|_| ())
}

pub(in crate::dss7::write) fn write_its_v(
    file: &File,
    input: &ItsInput,
    plan: &ItsPlan,
    v: &TsVariants,
    opts: &WriteOptions,
) -> Result<WriteReport, WriteError> {
    let x = check_extras(input.values.len(), input.quality, v)?;
    let mut session = Session::begin(file, opts)?;
    // Preflight: every lookup, read and refusal of every block and
    // every delete, before the first transaction. Later transactions touch other
    // pathnames only, so what was read stays valid while earlier ones commit.
    // Areas gained values2/values3, which tips this short-lived enum over clippy's
    // size-ratio threshold; boxing would add an allocation per block for nothing.
    #[allow(clippy::large_enum_variant)]
    enum Op {
        Store(Prepared),
        Delete(RecordRef),
    }
    let mut ops = Vec::new();
    for st in &plan.steps {
        match st {
            ItsStep::Store(bp) => {
                let p = session.eng.prepare_its_block_v(input, plan, bp, &x)?;
                session.eng.preflight(&p)?;
                ops.push(Op::Store(p));
            }
            ItsStep::DeleteGap { first, stop } => {
                for r in session.eng.gap_records(plan, *first, *stop)? {
                    session.eng.check_existing_its_v(&r)?;
                    ops.push(Op::Delete(r));
                }
            }
        }
    }
    let count = ops.len();
    session.eng.plan_commits(count, |eng| {
        for (txn, op) in ops.iter().enumerate() {
            eng.block_index = txn;
            match op {
                Op::Store(p) => {
                    eng.commit_areas(p.path.clone(), p.found.clone(), &p.areas)?;
                }
                Op::Delete(r) => {
                    eng.delete(r)?;
                }
            }
        }
        Ok(())
    })?;
    let mut blocks = Vec::with_capacity(count);
    for (txn, op) in ops.into_iter().enumerate() {
        session.eng.block_index = txn;
        blocks.push(
            match op {
                Op::Store(p) => session.eng.commit(p),
                Op::Delete(r) => session.eng.delete(&r),
            }
            .map_err(|e| e.after_planning(count))?,
        );
    }
    session.finish(blocks)
}

pub(in crate::dss7::write) fn check_its_variants(
    input: &ItsInput,
    v: &TsVariants,
) -> Result<(), WriteError> {
    check_extras(input.values.len(), input.quality, v).map(|_| ())
}

/// A pattern record, built without touching a file.
#[derive(Debug, Clone)]
pub(in crate::dss7::write) struct PatternPlan {
    path: Vec<u8>,
    areas: Areas,
}

/// `ztsStorePattern()` and `ztsProcessTimes()` lines 268-289. The stored time offset is 0:
/// `ztsProcessTimes()` returns before it derives one, so HEC writers store 0, and the
/// The pattern convention gives a regular pattern no start time to keep.
pub(in crate::dss7::write) fn plan_pattern(p: &PatternInput) -> Result<PatternPlan, WriteError> {
    let (path, regular_seconds) = validate_pattern_pathname(p.pathname)?;
    let n = p.values.len();
    if n == 0 {
        return Err(bad_input("no values"));
    }
    if n as i64 > i64::from(i32::MAX) / 4 {
        return Err(bad_input("too many values"));
    }
    check_text("units", p.units)?;
    check_text("data type", p.data_type)?;
    if let Some(tz) = p.time_zone {
        check_text("time zone", tz)?;
    }
    let strings = block::header_strings(p.units, p.data_type, p.time_zone);
    if strings.len() > block::MAX_HEADER_STRING_BYTES {
        return Err(bad_input(format!(
            "units, type and time zone take {} header bytes; HEC reads back at most {}",
            strings.len(),
            block::MAX_HEADER_STRING_BYTES
        )));
    }
    if !(-1..=127).contains(&p.precision) {
        return Err(bad_input("precision must be -1 (unset) or 0..=127"));
    }
    let words: Vec<i32> = match p.values {
        RtsValues::F32(v) => v.iter().map(|x| x.to_bits() as i32).collect(),
        RtsValues::F64(v) => v
            .iter()
            .flat_map(|x| {
                let b = x.to_bits();
                [b as u32 as i32, (b >> 32) as u32 as i32]
            })
            .collect(),
    };
    let double = matches!(p.values, RtsValues::F64(_));
    let mut fixed = [0i32; ih::FIXED];
    fixed[ih::PRECISION] = p.precision;
    let (data_type, v1) = if let Some(secs) = regular_seconds {
        if p.times.is_some() {
            return Err(bad_input(
                "a regular pattern is positional and takes no times",
            ));
        }
        if p.time_granularity.is_some() {
            return Err(bad_input(
                "time_granularity applies to irregular patterns only",
            ));
        }
        // ztsProcessTimes() lines 276-283.
        fixed[ih::TIME_GRANULARITY] = if secs < iv::SECS_IN_1_MINUTE {
            iv::SECOND_GRANULARITY
        } else {
            iv::MINUTE_GRANULARITY
        } as i32;
        (if double { RTD_PATTERN } else { RTS_PATTERN }, words)
    } else {
        let times = p
            .times
            .ok_or_else(|| bad_input("an irregular pattern needs one time per value"))?;
        if times.len() != n {
            return Err(bad_input(format!("{} times for {n} values", times.len())));
        }
        if let Some(i) = times.windows(2).position(|w| w[1] <= w[0]) {
            return Err(bad_input(format!(
                "pattern times are not strictly ascending at {}",
                i + 1
            )));
        }
        let gran = match p.time_granularity {
            Some(g) => {
                if !GRANULARITIES.contains(&g) {
                    return Err(bad_input(format!(
                        "time granularity {g} is not 1, 60, 3600 or 86400 seconds"
                    )));
                }
                g
            }
            None if times.iter().all(|t| t % iv::SECS_IN_1_MINUTE == 0) => iv::MINUTE_GRANULARITY,
            None => iv::SECOND_GRANULARITY,
        };
        let mut v1 = Vec::with_capacity(n + words.len());
        for (i, &t) in times.iter().enumerate() {
            if t % gran != 0 {
                return Err(bad_input(format!(
                    "time {i} is not a whole multiple of the {gran}-second granularity"
                )));
            }
            v1.push(
                i32::try_from(t / gran)
                    .map_err(|_| bad_input(format!("time {i} does not fit HEC's int32 times")))?,
            );
        }
        v1.extend_from_slice(&words);
        fixed[ih::TIME_GRANULARITY] = gran as i32;
        (if double { ITD_PATTERN } else { ITS_PATTERN }, v1)
    };
    let areas = Areas {
        data_type,
        ih: block::pack_internal_header(&fixed, &strings),
        h2: Vec::new(),
        uh: Vec::new(),
        v1,
        v2: Vec::new(),
        v3: Vec::new(),
        number_data: n as i64,
        logical: n as i64,
        first: (0, 0),
        last: (0, 0),
        total_expanded: 0,
        copy_out: false,
    };
    Ok(PatternPlan { path, areas })
}

pub(in crate::dss7::write) fn write_pattern_planned(
    file: &File,
    plan: &PatternPlan,
    opts: &WriteOptions,
) -> Result<WriteReport, WriteError> {
    let want = plan.areas.data_type;
    // zwriteExisting() lines 208-230: a pattern is overwritten only by its own type.
    let check = |found: Option<&RecordRef>| -> Result<(), WriteError> {
        match found {
            Some(r) if r.info.record_type != want || r.record_type != want => {
                Err(WriteError::Unsupported(format!(
                    "the existing record is type {}, not pattern type {want} (HEC \
                     DIFFERENT_RECORD_TYPE)",
                    r.info.record_type
                )))
            }
            _ => Ok(()),
        }
    };
    write_areas(file, &plan.path, &plan.areas, &check, opts)
}
