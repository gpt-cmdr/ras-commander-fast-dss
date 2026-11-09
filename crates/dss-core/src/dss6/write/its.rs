//! Irregular-interval store: a port of `zsitsi6.f:ZSITSI6` (hec-dss tag `7-IU-15`,
//! `heclib/heclib_f/src/`) with its helpers `zircpy6.f`, `zirdow6.f`, `zirdbl6.f` and
//! `zirbeg6.f`.
//!
//! The block loop, the six replace cases (`INFLAG = 1`, lines 622-818), the merge loop
//! (`INFLAG = 0`, lines 819-893), the block capacity `IBSIZE` (minimum `KITS*`, growth
//! `KITSIN`, lines 958-964), the slack kept when the capacity does not change (lines
//! 965-976) and the 28-word internal header (lines 981-1003) follow the Fortran on a
//! 1-based word buffer, as HEC does on `ILBUFF`.
//!
//! Deliberate differences:
//! - a float write into a stored double block keeps the block double (HEC 7-IU damages
//!   it: loops 160/180, lines 560-583, narrow the pairs to two or three words but
//!   `IMULT` still counts the stored double width, lines 615-616); a block wholly replaced takes
//!   the new width, as in HEC;
//! - -902 as the first or last value is refused (HEC treats it as a time-window marker
//!   and does not store it, lines 676, 724, 744, 774-795, 910, 943);
//! - a replace that would delete a stored block (no new value falls in it, lines
//!   917-928) or leave it empty is refused (deletion is out of scope);
//! - slack words past the stored data are zero (HEC leaves its buffer's previous
//!   contents there).

use super::engine::{
    blank_padded, keep_metadata, Engine, Found, Record6, I_COMP, I_NUHE, I_PREC, I_QUAL, I_TYPE,
    K_ITSDA, K_ITSIN,
};
use super::path::{self, Path6};
use super::{BlockReport, ItsInput, ItsMode, RtsValues, WriteError};
use crate::dss6::interval::{block_start_ymd, datcln, iymdjl, jliymd, next_block_ymd, BlockSpan};

/// `KLBUFF` (`headers/zdssts.h`): HEC's time-series buffer. HEC refuses a block whose
/// words reach it (`zsitsi6.f` lines 682, 734, 783, 801, 850), so the writer never makes one.
const KLBUFF: i64 = 500_000;
/// Most values one irregular write takes.
const MAX_VALUES: usize = 10_000_000;

#[derive(Debug, Clone)]
pub(super) struct Plan {
    pub path: Path6,
    pub block: BlockSpan,
    /// `IBDATE` and `ITIMES` (minutes from `IBDATE` 00:00).
    pub ibdate: i64,
    pub itimes: Vec<i64>,
}

fn bad_input(m: impl Into<String>) -> WriteError {
    WriteError::BadInput(m.into())
}

fn is_902(v: &RtsValues, i: usize) -> bool {
    match v {
        RtsValues::F32(x) => x[i] == -902.0,
        RtsValues::F64(x) => x[i] == -902.0,
    }
}

pub(super) fn plan(input: &ItsInput) -> Result<Plan, WriteError> {
    let path = path::validate_irregular(input.pathname)?;
    let block = match path.kind {
        path::Kind::Irregular { block } => block,
        path::Kind::Regular { .. } => unreachable!("validated as irregular"),
    };
    let n = input.values.len();
    if n == 0 || n > MAX_VALUES {
        return Err(bad_input(format!(
            "number of values must be 1..={MAX_VALUES}"
        )));
    }
    if input.times.len() != n {
        return Err(bad_input(format!(
            "{} times for {n} values",
            input.times.len()
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
    if is_902(&input.values, 0) || is_902(&input.values, n - 1) {
        return Err(bad_input(
            "-902 as the first or last value is HEC's time-window marker and is not stored \
             (zsitsi6.f); remove it or give the window with real values",
        ));
    }
    let mut mins = Vec::with_capacity(n);
    for (i, &t) in input.times.iter().enumerate() {
        if t.rem_euclid(60) != 0 {
            return Err(bad_input(format!(
                "time {i} is not a whole minute (DSS6 stores minutes)"
            )));
        }
        mins.push(t.div_euclid(60));
    }
    // zsitsi6.f lines 158-163: times must not decrease.
    if let Some(i) = mins.windows(2).position(|w| w[1] < w[0]) {
        return Err(bad_input(format!("time {} is before time {i}", i + 1)));
    }
    let first = datcln(0, mins[0]).0;
    let last = datcln(0, mins[n - 1]).0;
    if !(1..=9999).contains(&jliymd(first).0) || !(1..=9999).contains(&jliymd(last).0) {
        return Err(bad_input("times must lie in years 1..=9999"));
    }
    if mins[0] == 0 {
        return Err(bad_input("time 1899-12-31 00:00 is HEC's undefined time"));
    }
    let ibdate = mins[0].div_euclid(1440);
    let itimes: Vec<i64> = mins.iter().map(|m| m - ibdate * 1440).collect();
    if itimes[n - 1] > i64::from(i32::MAX) {
        return Err(bad_input(
            "times span more than 2^31 minutes (DSS6 relative times are 32-bit)",
        ));
    }
    Ok(Plan {
        path,
        block,
        ibdate,
        itimes,
    })
}

/// HEC's `ILBUFF`: 1-based words; reads past the end give 0.
struct Buf {
    w: Vec<i32>,
    overflow: bool,
}

impl Buf {
    fn new(w: Vec<i32>) -> Self {
        Buf { w, overflow: false }
    }

    fn get(&self, i: i64) -> i32 {
        if i >= 1 && (i as usize) <= self.w.len() {
            self.w[i as usize - 1]
        } else {
            0
        }
    }

    fn set(&mut self, i: i64, v: i32) {
        if !(1..KLBUFF).contains(&i) {
            self.overflow = true;
            return;
        }
        let i = i as usize;
        if self.w.len() < i {
            self.w.resize(i, 0);
        }
        self.w[i - 1] = v;
    }

    /// `zirdbl6` with `LTODBL = true`: the float at `from` as a double at `to`, `to + 1`.
    fn widen_float(&mut self, from: i64, to: i64) {
        let f = f32::from_bits(self.get(from) as u32);
        let d = f64::from(f).to_bits();
        self.set(to, d as u32 as i32);
        self.set(to + 1, (d >> 32) as u32 as i32);
    }

    /// `zirdow6`: move pairs from `icur` down (`ndown > 0`) or up (`ndown < 0`).
    fn zirdow6(&mut self, icur: i64, npairs: i64, ndown: i64, imult: i64) {
        let mv = |b: &mut Buf, k: i64, j: i64| {
            if imult >= 4 {
                b.set(k + 3, b.get(j + 3));
            }
            if imult >= 3 {
                b.set(k + 2, b.get(j + 2));
            }
            b.set(k + 1, b.get(j + 1));
            b.set(k, b.get(j));
        };
        if ndown > 0 {
            let jcur = icur + ndown;
            let mut i = npairs;
            while i >= jcur {
                mv(self, (i - 1) * imult + 1, (i - ndown - 1) * imult + 1);
                i -= 1;
            }
        } else if ndown < 0 {
            for i in icur..=npairs {
                let k = (i - 1) * imult + 1;
                let j = (i - ndown - 1) * imult + 1;
                self.set(k, self.get(j));
                self.set(k + 1, self.get(j + 1));
                if imult >= 3 {
                    self.set(k + 2, self.get(j + 2));
                }
                if imult >= 4 {
                    self.set(k + 3, self.get(j + 3));
                }
            }
        }
    }
}

struct Ctx<'a> {
    input: &'a ItsInput<'a>,
    plan: &'a Plan,
    /// Block width (double) and record quality after conversion.
    ldbl: bool,
    lrqual: bool,
    julsb: i64,
}

impl Ctx<'_> {
    /// Absolute minutes of value `ipos` (1-based).
    fn ct(&self, ipos: i64) -> i64 {
        self.plan.ibdate * 1440 + self.plan.itimes[ipos as usize - 1]
    }

    /// Absolute minutes of a block-relative time.
    fn bt(&self, jtime: i32) -> i64 {
        self.julsb * 1440 + i64::from(jtime)
    }

    /// `zircpy6` with `LSTORE = true` at word `n` (1-based) of `b`.
    fn cpy(&self, b: &mut Buf, n: i64, ipos: i64) {
        let k = ipos as usize - 1;
        let jtime = self.ct(ipos) - self.julsb * 1440;
        b.set(n, jtime as i32);
        if self.ldbl {
            let d = match self.input.values {
                RtsValues::F32(v) => f64::from(v[k]),
                RtsValues::F64(v) => v[k],
            }
            .to_bits();
            b.set(n + 1, d as u32 as i32);
            b.set(n + 2, (d >> 32) as u32 as i32);
        } else {
            let f = match self.input.values {
                RtsValues::F32(v) => v[k].to_bits(),
                RtsValues::F64(_) => unreachable!("a double write makes the block double"),
            };
            b.set(n + 1, f as i32);
        }
        let qi = n + if self.ldbl { 3 } else { 2 };
        if let Some(q) = self.input.quality {
            b.set(qi, q[k]);
        } else if self.lrqual {
            b.set(qi, 0);
        }
    }
}

fn span_index(b: BlockSpan) -> usize {
    match b {
        BlockSpan::Day => 0,
        BlockSpan::Month => 1,
        BlockSpan::Year => 2,
        BlockSpan::Decade => 3,
        BlockSpan::Century => 4,
    }
}

pub(super) fn store(
    eng: &mut Engine,
    input: &ItsInput,
    plan: &Plan,
) -> Result<Vec<BlockReport>, WriteError> {
    // zirbeg6.f lines 59-88: growth increment and minimum block size from the header.
    let incblk = i64::from(eng.h[K_ITSIN]);
    let minblk = i64::from(eng.h[K_ITSDA + span_index(plan.block)]);
    if incblk <= 0 || minblk <= 0 {
        return Err(WriteError::Unsupported(format!(
            "the file's irregular block sizes are not set (KITSIN {incblk}, minimum {minblk})"
        )));
    }
    let nvals = plan.itimes.len() as i64;
    let ldouble = matches!(input.values, RtsValues::F64(_));
    let lsqual = input.quality.is_some();
    let replace = input.mode == ItsMode::Replace;
    let (juls, _) = datcln(plan.ibdate, plan.itimes[0]);
    let (jule, _) = datcln(plan.ibdate, plan.itimes[nvals as usize - 1]);
    let mut ymd = block_start_ymd(juls, plan.block);
    let mut jul = iymdjl(ymd.0, ymd.1, ymd.2);
    let mut ipos: i64 = 1;
    let mut out = Vec::new();
    loop {
        let julsb = jul;
        let mut lclear = false;
        let pathname = plan.path.block_pathname(jul);
        ymd = next_block_ymd(plan.block, ymd);
        jul = iymdjl(ymd.0, ymd.1, ymd.2);
        let juleb = jul - 1;
        let eob = juleb * 1440 + 1440;
        let look = eng.lookup(&pathname)?;
        // The stored header and precision, kept on a rewrite (engine::keep_metadata).
        let kept = match &look.found {
            Found::Yes(ex) => Some((eng.read_stored(ex)?.internal_header, ex.f(I_PREC))),
            Found::No(_) => None,
        };
        let mut cx = Ctx {
            input,
            plan,
            ldbl: ldouble,
            lrqual: lsqual,
            julsb,
        };
        let mut buf = Buf::new(Vec::new());
        let mut npairs: i64 = 0;
        let mut ibsize_old: i64 = 0;
        let mut ldel = false;
        // Lines 489-491: a replace whose window spans the block does not read it.
        let mut to500 = replace && juls < julsb && jule >= jul;
        if !to500 {
            match &look.found {
                Found::No(_) => to500 = true,
                Found::Yes(ex) => {
                    let t = ex.f(I_TYPE);
                    if t != 110 && t != 115 {
                        return Err(WriteError::Unsupported(format!(
                            "{}: the stored record has type {t}, not irregular time series \
                             (110/115)",
                            String::from_utf8_lossy(&pathname)
                        )));
                    }
                    let st = eng.read_stored(ex)?;
                    if st.internal_header.len() < 2 {
                        return Err(WriteError::Corrupt("irregular internal header".into()));
                    }
                    ibsize_old = i64::from(st.internal_header[0]);
                    npairs = i64::from(st.internal_header[1]);
                    // Lines 506-508.
                    if ibsize_old <= 0 || npairs <= 0 {
                        to500 = true;
                    } else {
                        if ex.f(I_COMP) != 0 {
                            return Err(WriteError::Corrupt(
                                "a compressed irregular record".into(),
                            ));
                        }
                        if plan.path.is_elevation() && ex.f(I_NUHE) > 0 {
                            return Err(WriteError::Unsupported(format!(
                                "{}: an elevation record with a user header (HEC applies \
                                 vertical-datum handling, zsitsi6.f lines 285-453)",
                                String::from_utf8_lossy(&pathname)
                            )));
                        }
                        let rq = ex.f(I_QUAL) == 1;
                        let lfildob = t == 115;
                        let stored_mult = 2 + i64::from(rq) + i64::from(lfildob);
                        if (st.data.len() as i64) < npairs * stored_mult {
                            return Err(WriteError::Corrupt(format!(
                                "{}: {} data words for {npairs} values",
                                String::from_utf8_lossy(&pathname),
                                st.data.len()
                            )));
                        }
                        buf = Buf::new(st.data);
                        // Lines 550-584: convert the stored block to the incoming width;
                        // a double block stays double (deviation, see module docs).
                        if ldouble && !lfildob {
                            if !rq {
                                for i in (1..=npairs).rev() {
                                    buf.widen_float(i * 2, i * 3 - 1);
                                    buf.set(i * 3 - 2, buf.get(i * 2 - 1));
                                }
                            } else {
                                for i in (1..=npairs).rev() {
                                    buf.set(i * 4, buf.get(i * 3));
                                    buf.widen_float(i * 3 - 1, i * 4 - 2);
                                    buf.set(i * 4 - 3, buf.get(i * 3 - 2));
                                }
                            }
                        }
                        cx.ldbl = ldouble || lfildob;
                        cx.lrqual = rq;
                        // Lines 587-613: make room for quality.
                        if lsqual && !rq {
                            if !cx.ldbl {
                                for i in (1..=npairs).rev() {
                                    buf.set(i * 3, 0);
                                    buf.set(i * 3 - 1, buf.get(i * 2));
                                    buf.set(i * 3 - 2, buf.get(i * 2 - 1));
                                }
                            } else {
                                for i in (1..=npairs).rev() {
                                    buf.set(i * 4, 0);
                                    buf.set(i * 4 - 1, buf.get(i * 3));
                                    buf.set(i * 4 - 2, buf.get(i * 3 - 1));
                                    buf.set(i * 4 - 3, buf.get(i * 3 - 2));
                                }
                            }
                            cx.lrqual = true;
                        }
                    }
                }
            }
        }
        if !to500 {
            let imult = 2 + i64::from(cx.lrqual) + i64::from(cx.ldbl);
            let first_t = |b: &Buf, j: i64| b.get((j - 1) * imult + 1);
            if replace {
                // ----- REPLACE (lines 622-818) -----
                let mut ibeg: i64 = 1;
                let mut goto250 = ipos > 1;
                let mut done = false;
                if !goto250 {
                    let jtime = first_t(&buf, npairs);
                    if cx.ct(ipos) > cx.bt(jtime) {
                        // Case 1: new data after the last stored value.
                        loop {
                            if cx.ct(ipos) > eob {
                                break;
                            }
                            npairs += 1;
                            cx.cpy(&mut buf, (npairs - 1) * imult + 1, ipos);
                            ipos += 1;
                            if ipos > nvals {
                                break;
                            }
                        }
                        done = true;
                    } else {
                        for j in 1..=npairs {
                            ibeg = j;
                            if cx.ct(ipos) <= cx.bt(first_t(&buf, j)) {
                                break;
                            }
                        }
                        goto250 = true;
                    }
                }
                if goto250 && !done {
                    // Label 250.
                    let jtime = first_t(&buf, npairs);
                    if cx.ct(nvals) >= cx.bt(jtime) {
                        if ibeg == 1 {
                            // Case 4: the new data cover the whole stored block.
                            to500 = true;
                        } else {
                            // Case 2: replace from IBEG to the end of the block.
                            let jpos = ipos;
                            for _ in jpos..=nvals {
                                if cx.ct(ipos) > eob {
                                    break;
                                }
                                npairs = ibeg;
                                cx.cpy(&mut buf, (ibeg - 1) * imult + 1, ipos);
                                ibeg += 1;
                                ipos += 1;
                            }
                        }
                    } else {
                        // Cases 3, 5, 6: the new data end inside the stored block.
                        let mut iend = ibeg;
                        for j in ibeg..=npairs {
                            iend = j;
                            if cx.ct(nvals) < cx.bt(first_t(&buf, j)) {
                                break;
                            }
                        }
                        let jj = nvals - ipos + 1;
                        let k = iend - ibeg;
                        let ndown = jj - k;
                        if ndown < 0 {
                            lclear = true;
                        }
                        npairs += ndown;
                        buf.zirdow6(ibeg, npairs, ndown, imult);
                        let iend = iend + ndown - 1;
                        for kk in ibeg..=iend {
                            cx.cpy(&mut buf, (kk - 1) * imult + 1, ipos);
                            ipos += 1;
                        }
                    }
                }
            } else {
                // ----- MERGE (lines 819-893) -----
                let mut ibeg: i64 = 1;
                let mut at410 = cx.ct(ipos) > cx.bt(first_t(&buf, npairs));
                if !at410 {
                    loop {
                        let jtime = first_t(&buf, ibeg);
                        if cx.ct(ipos) > cx.bt(jtime) {
                            if cx.ct(ipos) > eob {
                                break;
                            }
                            ibeg += 1;
                            if ibeg > npairs {
                                at410 = true;
                                break;
                            }
                            continue;
                        }
                        if cx.ct(ipos) < cx.bt(jtime) {
                            npairs += 1;
                            buf.zirdow6(ibeg, npairs, 1, imult);
                        }
                        cx.cpy(&mut buf, (ibeg - 1) * imult + 1, ipos);
                        ipos += 1;
                        if ipos > nvals {
                            break;
                        }
                    }
                }
                if at410 {
                    while cx.ct(ipos) <= eob {
                        npairs += 1;
                        cx.cpy(&mut buf, (npairs - 1) * imult + 1, ipos);
                        ipos += 1;
                        if ipos > nvals {
                            break;
                        }
                    }
                }
            }
        }
        if to500 {
            // Label 500: a new block (lines 894-950).
            cx.lrqual = lsqual;
            cx.ldbl = ldouble;
            buf = Buf::new(Vec::new());
            lclear = true;
            npairs = nvals;
            let start = ipos;
            let mut i: i64 = 1;
            loop {
                if cx.ct(ipos) > eob {
                    npairs = i - 1;
                    if npairs <= 0 && replace {
                        if matches!(look.found, Found::Yes(_)) {
                            return Err(WriteError::Unsupported(format!(
                                "{}: this replace has no value in the stored block, which HEC \
                                 would delete (zsitsi6.f lines 917-928); deleting records is \
                                 not supported",
                                String::from_utf8_lossy(&pathname)
                            )));
                        }
                        ldel = true;
                    }
                    break;
                }
                let m = 2 + i64::from(cx.lrqual) + i64::from(cx.ldbl);
                cx.cpy(&mut buf, (i - 1) * m + 1, ipos);
                ipos += 1;
                if ipos > nvals {
                    npairs = i;
                    break;
                }
                i += 1;
                if i > nvals - start + 1 {
                    break;
                }
            }
        }
        let imult = 2 + i64::from(cx.lrqual) + i64::from(cx.ldbl);
        // Label 600 (lines 951-1028).
        let skip = npairs <= 0 && (ldel || !replace);
        if !skip {
            if npairs <= 0 {
                return Err(WriteError::Unsupported(format!(
                    "{}: the replace would leave the stored block empty",
                    String::from_utf8_lossy(&pathname)
                )));
            }
            let jbsize = ibsize_old;
            let ibsize = if npairs <= minblk {
                minblk
            } else {
                minblk + ((npairs - minblk) / incblk + 1) * incblk
            };
            if jbsize != ibsize {
                lclear = true;
            }
            let nda = ibsize * imult;
            if nda + 2 >= KLBUFF || buf.overflow {
                return Err(WriteError::Unsupported(format!(
                    "{}: the block would need {nda} words; HEC's buffer holds {KLBUFF}",
                    String::from_utf8_lossy(&pathname)
                )));
            }
            if lclear {
                for i in npairs * imult + 1..=nda {
                    buf.set(i, 0);
                }
            }
            let data: Vec<i32> = (1..=nda).map(|i| buf.get(i)).collect();
            let mut ih = Vec::with_capacity(28);
            ih.push(ibsize as i32);
            ih.push(npairs as i32);
            ih.push(julsb as i32);
            ih.push(buf.get(npairs * imult - 1)); // line 985, as HEC indexes it
            ih.push(plan.path.pseudo_minutes); // INTLPS (zsitsi6.f line 986)
            ih.extend(blank_padded(input.units, 2));
            ih.extend(blank_padded(input.data_type, 2));
            ih.push(-1); // IWTZONE
            ih.extend(blank_padded(b"", 6));
            ih.extend(std::iter::repeat(0).take(12));
            if let Some((stored, _)) = &kept {
                keep_metadata(&mut ih, stored, 9..16, 16..28);
            }
            let rec = Record6 {
                pathname: pathname.clone(),
                record_type: if cx.ldbl { 115 } else { 110 },
                internal_header: ih,
                data,
                logical: npairs as i32,
                quality: cx.lrqual,
                precision: kept.as_ref().map_or(0, |k| k.1),
            };
            // Stored after the loop, once every block is built and validated.
            out.push(rec);
        }
        // Label 620 (lines 1029-1034).
        if jule < jul {
            break;
        }
    }
    eng.pseudo_minutes = plan.path.pseudo_minutes;
    // Every block is built before the first store (each block has its own pathname, and
    // the loop reads only that block), so a refused write leaves the file unchanged.
    eng.store_all(&out)
}
