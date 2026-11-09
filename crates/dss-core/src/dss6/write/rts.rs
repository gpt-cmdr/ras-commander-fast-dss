//! Regular-interval store: a port of `zsrtsi6.f:ZSRTSI6` with `IPLAN = 0` (always
//! replace), `JCOMP = 0` and no file-default compression, and of `zmovbk6.f:ZMOVBK6`
//! with `IFLAG = 1` (hec-dss tag `7-IU-15`, `heclib/heclib_f/src/`).
//!
//! - Time window: the start is cleaned (`DATCLN`) and moved to its standard boundary
//!   with the offset kept (`zofset6.f` `IFLAG = 1`, `zsrtsi6.f` lines 447-457); the end
//!   is `INCTIM` of `NVALS - 1` periods (line 459).
//! - Blocks: `zbegdt6.f` (never the pre-6-I 15/30-minute daily blocks), `zincbk6.f`,
//!   weekly positions from the Sunday on or before the block date (lines 517-524).
//! - Every block is stored full: `NDATA = NOPERS(block start, next block start)` values
//!   (line 677), unwritten slots -901 (lines 699-727).
//! - Merging into an existing block (lines 728-808 and `zmovbk6`): values replace the
//!   stored ones at their positions; double wins (a float block written with doubles
//!   becomes double; float values written into a double block are widened); quality is
//!   kept when the block has it (positions written without quality get 0); quality can
//!   be added to a float block; every other quality/width combination is refused, as
//!   `zsrtsi6` refuses it with status 511 (line 1190).
//! - The record is written uncompressed (compression is optional; `JCOMP = 0` would keep
//!   the scheme of a compressed record being merged, lines 589-595; deviation).
//! - Internal header: 24 words (lines 871-887): offset, units, type, `IWTZONE` -1,
//!   blank time-zone name, zero coordinates.

use super::engine::{
    blank_padded, keep_metadata, Engine, Found, Lookup, Record6, I_COMP, I_LNDA, I_NUHE, I_PREC,
    I_QUAL, I_TYPE,
};
use super::path::{self, Path6};
use super::{BlockReport, RtsInput, RtsValues, WriteError};
use crate::dss6::compress;
use crate::dss6::interval::{
    apply_offset, block_start_ymd, datcln, idaywk, inctim, iymdjl, jliymd, next_block_ymd, nopers,
    regular_block, time_offset, KVERNO_6I,
};

/// Most values one write takes (the same bound as dataset reads).
pub(super) const MAX_VALUES: usize = 50_000_000;
const MISSING_F32: f32 = -901.0;
const MISSING_F64: f64 = -901.0;

#[derive(Debug, Clone)]
pub(super) struct BlockPlan {
    pub pathname: Vec<u8>,
    /// `NDATA`: values in the full block.
    pub ndata: usize,
    /// `NPOS`: 1-based position of the first value written in this block.
    pub npos: usize,
    /// First value (0-based) and number of values written in this block.
    pub first: usize,
    pub count: usize,
}

#[derive(Debug, Clone)]
pub(super) struct Plan {
    pub path: Path6,
    pub offset: i64,
    pub precision: i32,
    pub blocks: Vec<BlockPlan>,
}

fn bad_input(m: impl Into<String>) -> WriteError {
    WriteError::BadInput(m.into())
}

fn year_ok(julian: i64) -> bool {
    (1..=9999).contains(&jliymd(julian).0)
}

/// `zofset6.f:ZOFSET6` with `IFLAG = 1` (lines 25-127): the offset of a stamp from the
/// standard boundary, and the stamp moved to that boundary.
pub(super) fn to_standard(julian: i64, minutes: i64, interval: i64) -> Option<(i64, i64, i64)> {
    let (jul, jtime) = datcln(julian, minutes);
    let ofs = time_offset(julian, minutes, interval)?;
    if ofs == 0 {
        return Some((jul, jtime, 0));
    }
    if interval <= 10_080 {
        let (j, t) = datcln(jul, jtime + interval - ofs);
        return Some((j, t, ofs));
    }
    // Lines 98-124 (tri-monthly and longer).
    let (iyear, mut imon, mut iday) = jliymd(jul);
    if interval == 14_400 {
        if iday >= 30 {
            iday = 9; // line 55, kept by the Fortran for the adjustment below
        }
        iday = if iday <= 10 {
            10
        } else if iday <= 20 {
            20
        } else {
            28
        };
    } else if interval == 21_600 {
        if iday >= 30 {
            iday = 14; // line 63
        }
        iday = if iday <= 15 { 15 } else { 28 };
    } else {
        iday = 28;
    }
    if interval == 525_600 {
        imon = 12;
    }
    let julian = if iday >= 25 {
        iymdjl(iyear, imon + 1, 1) - 1
    } else {
        iymdjl(iyear, imon, iday)
    };
    Some((julian, 1440, ofs))
}

/// Start (cleaned, standard) of a series starting at `start` seconds.
fn standard_start(start: i64, minutes: i64) -> Result<(i64, i64, i64), WriteError> {
    if start == 0 {
        return Err(bad_input(
            "start 1899-12-31 00:00 is HEC's undefined time (julian 0, minute 0)",
        ));
    }
    if start.rem_euclid(60) != 0 {
        return Err(bad_input(
            "DSS6 stores whole minutes: the start must be a whole minute",
        ));
    }
    let j = start.div_euclid(86_400);
    let m = start.rem_euclid(86_400) / 60;
    if !year_ok(j) {
        return Err(bad_input("start year must be 1..=9999"));
    }
    // zsrtsi6.f lines 448-451: time 0 is 24:00 of the previous day.
    let (j, m) = datcln(j, m);
    to_standard(j, m, minutes).ok_or_else(|| bad_input("no standard time for this start"))
}

/// Plan the blocks of a regular write (`zsrtsi6.f` lines 457-524 and 677).
pub(super) fn plan(input: &RtsInput) -> Result<Plan, WriteError> {
    let path = path::validate_regular(input.pathname)?;
    let minutes = match path.kind {
        path::Kind::Regular { minutes } => minutes,
        path::Kind::Irregular { .. } => unreachable!("validated as regular"),
    };
    let n = input.values.len();
    if n == 0 || n > MAX_VALUES {
        return Err(bad_input(format!(
            "number of values must be 1..={MAX_VALUES}"
        )));
    }
    if let Some(q) = input.quality {
        if q.len() != n {
            return Err(bad_input(format!(
                "{} quality flags for {n} values",
                q.len()
            )));
        }
    }
    path::check_string("units", input.units)?;
    path::check_string("data type", input.data_type)?;
    if input.time_zone.is_some() {
        return Err(WriteError::Unsupported(
            "a time-zone name in a DSS6 record (IWTZONE/CWTZONE) is not written".into(),
        ));
    }
    let precision = match input.precision {
        -1 => 0,
        p @ 0..=9 => p,
        p => return Err(bad_input(format!("precision {p} is not -1 or 0..=9"))),
    };
    let (juls, istime, offset) = standard_start(input.start, minutes)?;
    let (jule, ietime) = inctim(minutes, n as i64 - 1, juls, istime);
    if !year_ok(jule) {
        return Err(bad_input("the series must end by year 9999"));
    }
    let span = regular_block(minutes as i32);
    let mut ymd = block_start_ymd(juls, span);
    let julast = {
        let l = block_start_ymd(jule, span);
        iymdjl(l.0, l.1, l.2)
    };
    let _ = ietime;
    let mut blocks = Vec::new();
    let mut nstart: i64 = 1;
    loop {
        let jul_d = iymdjl(ymd.0, ymd.1, ymd.2);
        let mut julsd = jul_d;
        ymd = next_block_ymd(span, ymd);
        let mut jul = iymdjl(ymd.0, ymd.1, ymd.2);
        if minutes == 10_080 {
            // Lines 517-524: weekly values fall on Saturday 24:00.
            jul = jul - idaywk(jul) + 1;
            julsd = julsd - idaywk(julsd) + 1;
        }
        let ndata = nopers(minutes, julsd, 0, jul, 0);
        let npos = nopers(minutes, julsd, 0, juls, istime) + nstart - 1;
        let nstore = (ndata - npos + 1).min(n as i64 - nstart + 1);
        if npos < 1 || ndata < 1 || nstore < 1 {
            return Err(bad_input(format!(
                "cannot place values in block {} (position {npos} of {ndata})",
                String::from_utf8_lossy(&path.block_pathname(jul_d))
            )));
        }
        blocks.push(BlockPlan {
            pathname: path.block_pathname(jul_d),
            ndata: ndata as usize,
            npos: npos as usize,
            first: (nstart - 1) as usize,
            count: nstore as usize,
        });
        nstart += nstore;
        // zsrtsi6.f lines 1072-1073.
        if nstart > n as i64 || jul > julast {
            break;
        }
    }
    if nstart <= n as i64 {
        return Err(bad_input("values past the last block"));
    }
    Ok(Plan {
        path,
        offset,
        precision,
        blocks,
    })
}

/// Stamps HEC gives the values: `INCTIM` from the standard start, then the offset
/// (`zofset6.f` `IFLAG = 2`, lines 142-155).
pub(super) fn value_times(pathname: &[u8], start: i64, n: usize) -> Result<Vec<i64>, WriteError> {
    let path = path::validate_regular(pathname)?;
    let minutes = match path.kind {
        path::Kind::Regular { minutes } => minutes,
        path::Kind::Irregular { .. } => unreachable!("validated as regular"),
    };
    if n == 0 || n > MAX_VALUES {
        return Err(bad_input(format!("n must be 1..={MAX_VALUES}")));
    }
    let (juls, istime, offset) = standard_start(start, minutes)?;
    Ok((0..n as i64)
        .map(|k| {
            let (j, m) = inctim(minutes, k, juls, istime);
            let (j, m) = apply_offset(j, m, minutes, offset);
            (j * 1440 + m) * 60
        })
        .collect())
}

/// Block values as stored: float or double bits, plus optional quality.
enum Vals {
    S(Vec<u32>),
    D(Vec<u64>),
}

pub(super) fn internal_header(offset: i64, units: &[u8], data_type: &[u8]) -> Vec<i32> {
    let mut ih = Vec::with_capacity(24);
    ih.push(offset as i32);
    ih.extend(blank_padded(units, 2));
    ih.extend(blank_padded(data_type, 2));
    ih.push(-1); // IWTZONE, zsrtsi6.f line 876; default -1, zinit6.f line 929
    ih.extend(blank_padded(b"", 6)); // CWTZONE, 24 blanks
    ih.extend(std::iter::repeat(0).take(12));
    ih
}

fn refuse_511(path: &[u8], what: &str) -> WriteError {
    WriteError::Unsupported(format!(
        "{}: {what}; HEC refuses this store too (zsrtsi6 status 511)",
        String::from_utf8_lossy(path)
    ))
}

/// Build the record of one block (`zsrtsi6.f` lines 529-905).
fn build_block(
    eng: &Engine,
    input: &RtsInput,
    plan: &Plan,
    bp: &BlockPlan,
    look: &Lookup,
) -> Result<Record6, WriteError> {
    let ndata = bp.ndata;
    let ldouble = matches!(input.values, RtsValues::F64(_));
    let lqual = input.quality.is_some();
    let mut kept_header: Option<Vec<i32>> = None;
    let mut kept_precision: Option<i32> = None;
    let (mut vals, mut q): (Vals, Option<Vec<i32>>) = match &look.found {
        Found::No(_) => (
            if ldouble {
                Vals::D(vec![MISSING_F64.to_bits(); ndata])
            } else {
                Vals::S(vec![MISSING_F32.to_bits(); ndata])
            },
            lqual.then(|| vec![0; ndata]),
        ),
        Found::Yes(ex) => {
            let t = ex.f(I_TYPE);
            if t != 100 && t != 105 {
                return Err(WriteError::Unsupported(format!(
                    "{}: the stored record has type {t}, not regular time series (100/105)",
                    String::from_utf8_lossy(&bp.pathname)
                )));
            }
            if ex.bin_type != t {
                return Err(WriteError::Corrupt(format!(
                    "{}: bin type {} differs from info type {t}",
                    String::from_utf8_lossy(&bp.pathname),
                    ex.bin_type
                )));
            }
            if ex.f(I_LNDA) as usize != ndata {
                return Err(WriteError::Unsupported(format!(
                    "{}: the stored block holds {} values, the standard block {ndata}",
                    String::from_utf8_lossy(&bp.pathname),
                    ex.f(I_LNDA)
                )));
            }
            if plan.path.is_elevation() && ex.f(I_NUHE) > 0 {
                return Err(WriteError::Unsupported(format!(
                    "{}: an elevation record with a user header (HEC applies vertical-datum \
                     handling and may keep the header, zsrtsi6.f lines 252-441)",
                    String::from_utf8_lossy(&bp.pathname)
                )));
            }
            let lfildob = t == 105;
            let lqread = ex.f(I_QUAL) > 0;
            let comp = ex.f(I_COMP);
            let st = eng.read_stored(ex)?;
            kept_header = Some(st.internal_header.clone());
            kept_precision = Some(ex.f(I_PREC));
            let d = &st.data;
            let short = || {
                WriteError::Corrupt(format!(
                    "{}: {} data words are too few for {ndata} values",
                    String::from_utf8_lossy(&bp.pathname),
                    d.len()
                ))
            };
            let (v, sq) = if comp > 0 {
                if lfildob || lqread {
                    return Err(WriteError::Corrupt(
                        "a compressed record with quality or doubles".into(),
                    ));
                }
                let (_, f) = compress::decode(&st.compression_header, d, true)
                    .map_err(|e| WriteError::Corrupt(format!("compressed block: {e}")))?;
                if f.len() != ndata {
                    return Err(short());
                }
                (Vals::S(f.iter().map(|x| x.to_bits()).collect()), None)
            } else {
                let need = ndata * if lfildob { 2 } else { 1 } + if lqread { ndata } else { 0 };
                if d.len() < need {
                    return Err(short());
                }
                match (lfildob, lqread) {
                    (false, false) => (
                        Vals::S(d[..ndata].iter().map(|&w| w as u32).collect()),
                        None,
                    ),
                    (false, true) => (
                        Vals::S((0..ndata).map(|i| d[2 * i] as u32).collect()),
                        Some((0..ndata).map(|i| d[2 * i + 1]).collect()),
                    ),
                    (true, qd) => (
                        Vals::D(
                            (0..ndata)
                                .map(|i| {
                                    (d[2 * i] as u32 as u64) | ((d[2 * i + 1] as u32 as u64) << 32)
                                })
                                .collect(),
                        ),
                        qd.then(|| d[2 * ndata..3 * ndata].to_vec()),
                    ),
                }
            };
            // Lines 730-808: the allowed width/quality combinations.
            let v = match (lqual, lqread) {
                (false, false) => match v {
                    Vals::S(s) if ldouble => Vals::D(
                        s.iter()
                            .map(|&b| f64::from(f32::from_bits(b)).to_bits())
                            .collect(),
                    ),
                    other => other,
                },
                (true, true) => {
                    if ldouble != lfildob {
                        return Err(refuse_511(
                            &bp.pathname,
                            "the width differs from the stored record and quality is involved",
                        ));
                    }
                    v
                }
                (true, false) if !ldouble && !lfildob => v,
                (false, true) if !ldouble && !lfildob => v,
                (true, false) => {
                    return Err(refuse_511(
                        &bp.pathname,
                        "quality cannot be added to a double record",
                    ))
                }
                (false, true) => {
                    return Err(refuse_511(
                        &bp.pathname,
                        "the stored double record has quality and the write has none",
                    ))
                }
            };
            let q = match (lqual, sq) {
                (_, Some(q)) => Some(q),
                (true, None) => Some(vec![0; ndata]),
                (false, None) => None,
            };
            (v, q)
        }
    };
    // zmovbk6 with IFLAG = 1: replace values (and quality) at positions NPOS...
    let qin = input.quality;
    for i in 0..bp.count {
        let m = bp.npos - 1 + i;
        let k = bp.first + i;
        match (&mut vals, &input.values) {
            (Vals::S(s), RtsValues::F32(v)) => s[m] = v[k].to_bits(),
            (Vals::D(d), RtsValues::F64(v)) => d[m] = v[k].to_bits(),
            (Vals::D(d), RtsValues::F32(v)) => d[m] = f64::from(v[k]).to_bits(),
            (Vals::S(_), RtsValues::F64(_)) => unreachable!("double input makes the block double"),
        }
        if let Some(qb) = q.as_mut() {
            qb[m] = qin.map_or(0, |qq| qq[k]);
        }
    }
    let (record_type, data) = match (&vals, &q) {
        (Vals::S(s), None) => (100, s.iter().map(|&b| b as i32).collect::<Vec<_>>()),
        (Vals::S(s), Some(qq)) => (
            100,
            s.iter()
                .zip(qq)
                .flat_map(|(&b, &f)| [b as i32, f])
                .collect(),
        ),
        (Vals::D(d), qq) => {
            let mut w: Vec<i32> = d
                .iter()
                .flat_map(|&b| [b as u32 as i32, (b >> 32) as u32 as i32])
                .collect();
            if let Some(qq) = qq {
                w.extend_from_slice(qq);
            }
            (105, w)
        }
    };
    let mut ih = internal_header(plan.offset, input.units, input.data_type);
    if let Some(stored) = &kept_header {
        keep_metadata(&mut ih, stored, 5..12, 12..24);
    }
    let precision = match (input.precision, kept_precision) {
        (-1, Some(p)) => p,
        _ => plan.precision,
    };
    Ok(Record6 {
        pathname: bp.pathname.clone(),
        record_type,
        internal_header: ih,
        data,
        logical: ndata as i32,
        quality: q.is_some(),
        precision,
    })
}

/// Store every planned block, each as its own transaction.
pub(super) fn store(
    eng: &mut Engine,
    input: &RtsInput,
    plan: &Plan,
) -> Result<Vec<BlockReport>, WriteError> {
    // HEC readers of files before 6-I put 15- and 30-minute data in daily blocks
    // (dss6.md §7.2). Those files are older than 6-M, which the engine
    // refuses ([`Engine::new`]), so the writer never meets them.
    debug_assert!(eng.kverno() >= KVERNO_6I);
    // Build and validate every block before the first store, so a refused write leaves
    // the file unchanged (the blocks have distinct pathnames: no block depends on another).
    let mut recs = Vec::with_capacity(plan.blocks.len());
    for bp in &plan.blocks {
        let look = eng.lookup(&bp.pathname)?;
        recs.push(build_block(eng, input, plan, bp, &look)?);
    }
    eng.store_all(&recs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dss6::interval::iymdjl;

    fn secs(y: i64, m: i64, d: i64, hh: i64, mm: i64) -> i64 {
        (iymdjl(y, m, d) * 1440 + hh * 60 + mm) * 60
    }

    #[test]
    fn plans_blocks() {
        let v = [1.0f32; 48];
        let input = RtsInput::new(
            b"/A/B/FLOW//1HOUR/F/",
            secs(2020, 1, 31, 1, 0),
            RtsValues::F32(&v),
            b"CFS",
            b"INST-VAL",
        );
        let p = plan(&input).unwrap();
        assert_eq!(p.blocks.len(), 2);
        assert_eq!(
            p.blocks[0].pathname,
            b"/A/B/FLOW/01JAN2020/1HOUR/F/".to_vec()
        );
        assert_eq!(p.blocks[0].ndata, 744);
        assert_eq!(p.blocks[0].npos, 721);
        assert_eq!(p.blocks[0].count, 24);
        assert_eq!(p.blocks[1].ndata, 696);
        assert_eq!(p.blocks[1].npos, 1);
        // Offset: 00:30 hourly.
        let input = RtsInput::new(
            b"/A/B/FLOW//1HOUR/F/",
            secs(2020, 1, 1, 0, 30),
            RtsValues::F32(&v),
            b"CFS",
            b"INST-VAL",
        );
        let p = plan(&input).unwrap();
        assert_eq!(p.offset, 30);
        assert_eq!(p.blocks[0].npos, 1);
        // Weekly: Sunday-aligned decade blocks (2000-2009 holds 522 values).
        let input = RtsInput::new(
            b"/A/B/FLOW//1WEEK/F/",
            secs(2000, 1, 2, 0, 0),
            RtsValues::F32(&v),
            b"CFS",
            b"INST-VAL",
        );
        let p = plan(&input).unwrap();
        assert_eq!(
            p.blocks[0].pathname,
            b"/A/B/FLOW/01JAN2000/1WEEK/F/".to_vec()
        );
        assert_eq!(p.blocks[0].ndata, 522);
        assert_eq!(p.offset, 0);
        // 15-minute data: monthly blocks.
        let input = RtsInput::new(
            b"/A/B/FLOW//15MIN/F/",
            secs(2020, 12, 1, 0, 15),
            RtsValues::F32(&v),
            b"CFS",
            b"INST-VAL",
        );
        assert_eq!(plan(&input).unwrap().blocks[0].ndata, 2976);
        let t = value_times(b"/A/B/FLOW//1MON/F/", secs(2020, 2, 1, 0, 0), 3).unwrap();
        assert_eq!(
            t,
            vec![
                secs(2020, 2, 1, 0, 0),
                secs(2020, 3, 1, 0, 0),
                secs(2020, 4, 1, 0, 0)
            ]
        );
    }

    #[test]
    fn refusals() {
        let v = [1.0f32; 3];
        let ok = secs(2020, 1, 1, 1, 0);
        let mk = |s: i64| {
            RtsInput::new(
                b"/A/B/F//1HOUR/F/",
                s,
                RtsValues::F32(&v),
                b"CFS",
                b"INST-VAL",
            )
        };
        assert!(plan(&mk(ok + 1)).is_err());
        assert!(plan(&mk(0)).is_err());
        let mut i = mk(ok);
        i.time_zone = Some(b"UTC");
        assert!(matches!(plan(&i), Err(WriteError::Unsupported(_))));
        let mut i = mk(ok);
        i.units = b"123456789";
        assert!(plan(&i).is_err());
        let mut i = mk(ok);
        i.quality = Some(&[1, 2]);
        assert!(plan(&i).is_err());
        let mut i = mk(ok);
        i.precision = 12;
        assert!(plan(&i).is_err());
    }
}
