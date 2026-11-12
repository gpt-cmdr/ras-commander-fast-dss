//! DSS6 pattern records: regular type 101 (`zsrtsi6.f` lines 222-233 →
//! `zsrtpa6.f:ZSRTPA6`) and irregular type 111 (`zsitsi6.f` lines 213-266). Hec-dss tag
//! `7-IU-15`, `heclib/heclib_f/src/`.
//!
//! - Floats only: `zsrtsi6.f` refuses doubles (`GO TO 995`), and the irregular branch stores
//!   `SVALUES` whatever the caller passed.
//! - Regular: 5-word internal header (offset, units, type). `zsrtpa6.f` line 29 stores
//!   `IOFSET` uninitialized and `zrrtsi.f` line 183 resets it on read; the writer stores 0.
//! - Irregular: minutes and float pairs, 28-word internal header. Written as the Fortran stores
//!   them but not confirmed by a HEC reader: Monolith 3.3.27 misreads the tested type-111
//!   records from both writers. Input times are seconds, whole minutes, as the
//!   native reader returns them: stored minutes relative to the block. `IIHEAD(3)` is the block
//!   holding 01JAN3000 (the pattern pseudo-date); readers of pattern records do not use it.
//! - HEC stores no quality or notes in a pattern record.

use super::engine::{blank_padded, Engine, Found, Record6, I_TYPE};
use super::{path, BlockReport, Dss6IrregularPattern, PatternInput, RtsValues, WriteError};
use crate::dss6::interval::{block_start_ymd, iymdjl, BlockSpan};

fn bad_input(m: impl Into<String>) -> WriteError {
    WriteError::BadInput(m.into())
}

/// Build the record of one pattern without touching a file. An irregular pattern (type
/// 111) is refused unless `irregular` is [`Dss6IrregularPattern::AllowUnconfirmed`]:
/// no HEC reader has confirmed such records.
pub(super) fn record(
    p: &PatternInput,
    irregular: Dss6IrregularPattern,
) -> Result<Record6, WriteError> {
    let (stored, kind) = path::validate_pattern(p.pathname)?;
    if matches!(kind, path::Kind::Irregular { .. })
        && irregular != Dss6IrregularPattern::AllowUnconfirmed
    {
        return Err(WriteError::Unsupported(
            "a DSS6 irregular pattern (type 111) is written only on explicit opt-in \
             (Dss6IrregularPattern::AllowUnconfirmed; Python \
             allow_unconfirmed_dss6_irregular_pattern=True): HEC Monolith 3.3.27 misreads \
             such records from both writers and no HEC reader has confirmed them"
                .into(),
        ));
    }
    let v =
        match p.values {
            RtsValues::F32(v) => v,
            RtsValues::F64(_) => return Err(WriteError::Unsupported(
                "DSS6 pattern records store floats only (zsrtsi6.f refuses doubles, status 995)"
                    .into(),
            )),
        };
    if v.is_empty() {
        return Err(bad_input("no values"));
    }
    if v.len() > i32::MAX as usize / 4 {
        return Err(bad_input("too many values"));
    }
    path::check_string("units", p.units)?;
    path::check_string("data type", p.data_type)?;
    if p.time_zone.is_some() {
        return Err(WriteError::Unsupported(
            "a time-zone name in a DSS6 record (IWTZONE/CWTZONE) is not written".into(),
        ));
    }
    if p.precision != -1 {
        return Err(bad_input(
            "DSS6 pattern records carry no precision; pass -1",
        ));
    }
    if !matches!(p.time_granularity, None | Some(60)) {
        return Err(bad_input("DSS6 stores whole minutes (granularity 60)"));
    }
    let n = v.len();
    match kind {
        path::Kind::Regular { .. } => {
            if p.times.is_some() {
                return Err(bad_input(
                    "a regular pattern is positional and takes no times",
                ));
            }
            let mut ih = Vec::with_capacity(5);
            ih.push(0);
            ih.extend(blank_padded(p.units, 2));
            ih.extend(blank_padded(p.data_type, 2));
            Ok(Record6 {
                pathname: stored,
                record_type: 101,
                internal_header: ih,
                data: v.iter().map(|x| x.to_bits() as i32).collect(),
                logical: n as i32,
                quality: false,
                precision: 0,
            })
        }
        path::Kind::Irregular { block } => {
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
            let mut data = Vec::with_capacity(2 * n);
            for (i, (&t, x)) in times.iter().zip(v).enumerate() {
                if t.rem_euclid(60) != 0 {
                    return Err(bad_input(format!(
                        "time {i} is not a whole minute (DSS6 stores minutes)"
                    )));
                }
                let m = i32::try_from(t / 60)
                    .map_err(|_| bad_input(format!("time {i} does not fit DSS6's 32-bit times")))?;
                data.push(m);
                data.push(x.to_bits() as i32);
            }
            let jul = pattern_block_julian(block);
            let mut ih = Vec::with_capacity(28);
            ih.push(n as i32);
            ih.push(n as i32);
            ih.push(jul as i32);
            ih.push(data[2 * n - 2]);
            ih.push(0); // INTLPS
            ih.extend(blank_padded(p.units, 2));
            ih.extend(blank_padded(p.data_type, 2));
            ih.push(-1); // IWTZONE
            ih.extend(blank_padded(b"", 6));
            ih.extend(std::iter::repeat(0).take(12));
            Ok(Record6 {
                pathname: stored,
                record_type: 111,
                internal_header: ih,
                data,
                // zwritex6.f takes the stored word count here: NDA = 2n (as Monolith writes it).
                logical: 2 * n as i32,
                quality: false,
                precision: 0,
            })
        }
    }
}

fn pattern_block_julian(block: BlockSpan) -> i64 {
    let (y, m, d) = block_start_ymd(iymdjl(3000, 1, 1), block);
    iymdjl(y, m, d)
}

/// Store a built pattern record: new, or a replacement of a pattern of the same type.
pub(super) fn store(eng: &mut Engine, rec: &Record6) -> Result<Vec<BlockReport>, WriteError> {
    if let Found::Yes(ex) = &eng.lookup(&rec.pathname)?.found {
        let t = ex.f(I_TYPE);
        if t != rec.record_type || ex.bin_type != rec.record_type {
            return Err(WriteError::Unsupported(format!(
                "the existing record is type {t}, not pattern type {}",
                rec.record_type
            )));
        }
    }
    eng.store_all(std::slice::from_ref(rec))
}

#[cfg(test)]
mod tests {
    use super::super::engine::{write_with, Found};
    use super::super::{create_dss6, write_pattern6_with, Dss6IrregularPattern, WriteOptions};
    use super::*;
    use crate::dss6::Dss6File;
    use std::fs::OpenOptions;

    /// The rule in `Engine::store_all` covers record types 100..=119: a rewrite of a pattern
    /// keeps the stored user header (`zsrtsi6.f` drops it; a deliberate difference). No
    /// native write can attach a header to a pattern, so the test stores one directly.
    #[test]
    fn pattern_rewrite_keeps_the_stored_user_header() {
        let d = std::env::temp_dir().join(format!("p38_uh_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let opts = WriteOptions {
            sync: crate::dss7::write::SyncMode::None,
            ..WriteOptions::default()
        };
        let times = [3600i64, 7200];
        for (name, path, stored, t) in [
            (
                "r.dss",
                &b"/P/U/FLOW/TS-PATTERN/1DAY/R/"[..],
                &b"/P/U/FLOW/TS-PATTERN/1DAY/R/"[..],
                None,
            ),
            (
                "i.dss",
                b"/P/U/FLOW/TS-PATTERN/IR-YEAR/I/",
                b"/P/U/FLOW/TS-PATTERN/IR-YEAR/I/",
                Some(&times[..]),
            ),
        ] {
            let f = d.join(name);
            create_dss6(&f).unwrap();
            let mut i = PatternInput::new(path, RtsValues::F32(&[1.0, 2.0]), b"CFS", b"PER-AVER");
            i.times = t;
            let rec = record(&i, Dss6IrregularPattern::AllowUnconfirmed).unwrap();
            let header = vec![0x4142_4344, 0x4546_4748, 7];
            let file = OpenOptions::new().read(true).write(true).open(&f).unwrap();
            write_with(&file, &opts, |eng| {
                let look = eng.lookup(&rec.pathname)?;
                assert!(matches!(look.found, Found::No(_)));
                eng.next_user_header = Some(header.clone());
                Ok(vec![eng.store(&rec, look)?])
            })
            .unwrap();
            drop(file);
            let read =
                |f: &std::path::Path| Dss6File::open(f).unwrap().read_ts_record(stored).unwrap();
            assert_eq!(read(&f).user_header, header, "{name} stored");

            // Equal values: nothing is written.
            let before = std::fs::read(&f).unwrap();
            let r =
                write_pattern6_with(&f, &i, &opts, Dss6IrregularPattern::AllowUnconfirmed).unwrap();
            assert_eq!(
                r.blocks[0].outcome,
                crate::dss7::write::BlockOutcome::Unchanged
            );
            assert_eq!(before, std::fs::read(&f).unwrap());

            // Changed values: a new copy with the same header.
            i.values = RtsValues::F32(&[3.0, 4.0]);
            let r =
                write_pattern6_with(&f, &i, &opts, Dss6IrregularPattern::AllowUnconfirmed).unwrap();
            assert_eq!(
                r.blocks[0].outcome,
                crate::dss7::write::BlockOutcome::Replaced
            );
            let after = read(&f);
            assert_eq!(after.user_header, header, "{name} rewritten");
            assert!(super::super::check_file6(&f).unwrap().problems.is_empty());
        }
        let _ = std::fs::remove_dir_all(&d);
    }
}
