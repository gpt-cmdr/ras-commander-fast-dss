//! DSS7 paired-data store: the full-record branch of
//! `Public/zpdStore.c:zpdStore()` (`boolStoreEntire`, storage flag 0: the arrays' own
//! width, lines 688-725), then `DssInterface/v6and7/zwrite.c:zwrite()` →
//! `Internal/zwriteInternal.c:zwriteInternal()` → `zwriteNew()` / `zwriteExisting()`,
//! committed with the transaction order of `dss7.md` §6.4 through [`Engine::commit_areas`].
//! Paths relative to `heclib/heclib_c/src/` of `hec-dss` tag `7-IU-15`.
//!
//! Record areas (`zpdStore()` lines 736-818):
//! - internal header: `INT_HEAD_pdNumberOrdinates` (0), `pdNumberCurves` (1),
//!   `pdBoolIndependentIsXaxis` (2), `pdLabelsLength` (3), `pdPrecision` (4)
//!   (`headers/zStructAllocation.h` lines 107-112; lines 736-748), int 5 four blanks,
//!   then the units and types and the time zone from int 6
//!   (`Internal/zpdUnitsToHead.c:zpdUnitsToHead()`;
//!   each string trimmed, blank-padded and NUL-terminated to 8-byte words by
//!   `Utilities/stringHeaderUtility.c:appendStringToHeader()` and
//!   `Internal/zcharTo8Byte.c`);
//! - header2: the labels, NUL-terminated and concatenated, `labelsLength` bytes, rounded
//!   up to whole words (lines 757-776);
//! - values1: the ordinates; values2: the curves, curve-major (lines 805-900);
//! - `kinfoNumberData` = `(numberCurves + 1) * numberOrdinates` (line 795), logical
//!   number the same (`zwriteNew()` lines 297-302); `totalAllocatedSize` = values1 and
//!   values2 rounded to words (`zwriteInternal()` lines 255-262); no expansion target
//!   (`zstructTransferNew()` leaves `totalExpandedSize` 0);
//! - first and last dates 0: `zpdStore()` never sets `kdataFirstDate`/`kdataLastDate`
//!   (a fresh heclib handle holds 0; HEC leaves a value from an earlier time-series write
//!   on the same handle).

use super::{block, Areas, Engine, Prepared, Session};
use crate::dss7::write::path::validate_pd_pathname;
use crate::dss7::write::pd::{check_common, PdInput};
use crate::dss7::write::{BlockReport, WriteError, WriteOptions, WriteReport};
use crate::paireddata::{PD, PDD};
use std::fs::File;

/// `carraySize` of `zpdUnitsToHead()` (a 400-byte buffer less 2): HEC stops packing, and
/// leaves the strings unwritten, once the packed position reaches it. Packed strings are
/// whole 8-byte words, so at most 392 bytes fit.
pub(crate) const MAX_PD_STRING_BYTES: usize = 392;

/// The record areas of one input, built without touching a file.
#[derive(Debug, Clone)]
pub(crate) struct PdPlan {
    pub path: Vec<u8>,
    areas: Areas,
    /// `pathnameIsElevPd()`: the C part's independent or dependent parameter is ELEV.
    elev: bool,
}

/// `Utilities/verticalDatum.c:pathnameIsElevPd()` (lines 2000-2018): the first and second
/// `strtok` tokens of the C part on `-`, each tested for a case-insensitive `ELEV` prefix.
/// Returns (independent, dependent).
pub(crate) fn pathname_is_elev_pd(path: &[u8]) -> (bool, bool) {
    let c = path.split(|&b| b == b'/').nth(3).unwrap_or(&[]);
    let mut toks = c.split(|&b| b == b'-').filter(|p| !p.is_empty());
    let elev =
        |t: Option<&[u8]>| t.is_some_and(|p| p.len() >= 4 && p[..4].eq_ignore_ascii_case(b"ELEV"));
    let a = elev(toks.next());
    let b = elev(toks.next());
    (a, b)
}

/// The unit rule shared by both versions. `zpdStore()` lines 324-647
/// (DSS7) and `ZSPDI6` lines 106-306 (DSS6) read an elevation-side unit containing `|` as
/// a unit specification (`U=<unit>|V=<datum>`; `getCurrentVerticalDatum()`,
/// `verticalDatum.c` lines 1442-1495; `crack_unit_spec`, `zspdi6.f` lines 187-196): the
/// unit may be rewritten and a datum conversion requested. Refused, as the writer applies
/// no datum. Returns whether the C part names an ELEV parameter.
pub(crate) fn check_elev_units(input: &PdInput) -> Result<bool, WriteError> {
    let (ind, dep) = pathname_is_elev_pd(input.pathname);
    for (is, what, u) in [
        (ind, "units_independent", input.units_independent),
        (dep, "units_dependent", input.units_dependent),
    ] {
        if is && u.contains(&b'|') {
            return Err(WriteError::Unsupported(format!(
                "{what} {:?} of an ELEV parameter contains '|': heclib reads it as a unit \
                 specification with a vertical datum and may change the unit or convert the \
                 values (zpdStore.c lines 324-647); the writer applies no datum",
                String::from_utf8_lossy(u)
            )));
        }
    }
    Ok(ind || dep)
}

fn bytes_to_ints(b: &[u8], ints: usize) -> Vec<i32> {
    let mut buf = b.to_vec();
    buf.resize(ints * 4, 0);
    buf.chunks_exact(4)
        .map(|c| i32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}

/// Validate an input and build its areas (`zpdStore()` lines 688-818).
pub(crate) fn plan_pd(input: &PdInput) -> Result<PdPlan, WriteError> {
    validate_pd_pathname(input.pathname)?;
    check_common(input)?;
    let elev = check_elev_units(input)?;
    let nord = input.data.number_ordinates();
    let ncurve = input.number_curves;
    if nord > i32::MAX as usize || ncurve > i32::MAX as usize {
        return Err(WriteError::BadInput("too many ordinates or curves".into()));
    }
    // zpdUnitsToHead(): five strings, each a whole number of 8-byte words.
    let mut strings = Vec::new();
    for (_, s) in input.strings() {
        block::header_string(Some(s), &mut strings);
    }
    block::header_string(input.time_zone, &mut strings);
    if strings.len() > MAX_PD_STRING_BYTES {
        return Err(WriteError::BadInput(format!(
            "units, types and time zone pack to {} bytes; heclib holds {MAX_PD_STRING_BYTES} \
             (zpdUnitsToHead() would store none of them)",
            strings.len()
        )));
    }
    // Labels in header2 (lines 757-776): each label NUL-terminated, concatenated.
    let mut labels: Vec<u8> = Vec::new();
    if let Some(ls) = input.labels {
        for l in ls {
            labels.extend_from_slice(l);
            labels.push(0);
        }
    }
    if labels.len() > i32::MAX as usize / 2 {
        return Err(WriteError::BadInput("labels are too long".into()));
    }
    let mut ih = vec![
        nord as i32,
        ncurve as i32,
        input.independent_is_x_axis,
        labels.len() as i32,
        input.precision_word(),
        i32::from_le_bytes(*b"    "),
    ];
    ih.extend(bytes_to_ints(&strings, strings.len() / 4));
    // numberLongsInBytes(labelsLength) * 2 ints, zero-filled after the labels.
    let h2 = bytes_to_ints(&labels, labels.len().div_ceil(8) * 2);
    let (v1, v2) = input.data.words();
    let number_data = ((ncurve + 1) * nord) as i64;
    Ok(PdPlan {
        path: input.pathname.to_vec(),
        areas: Areas {
            data_type: input.data.record_type(),
            ih,
            h2,
            uh: Vec::new(),
            v1,
            v2,
            v3: Vec::new(),
            number_data,
            logical: number_data,
            first: (0, 0),
            last: (0, 0),
            total_expanded: 0,
            copy_out: false,
        },
        elev,
    })
}

impl Engine<'_> {
    /// The existing record, if any, and the refusals for it.
    fn prepare_pd(&self, plan: &PdPlan) -> Result<Prepared, WriteError> {
        let found = self.lookup_for_write(&plan.path)?;
        if let Some(r) = &found {
            let t = r.record_type;
            if (t != PD && t != PDD) || r.info.record_type != t {
                // zpdStore() lines 904-911 delete a record of another type and write the
                // paired data in its place; the writer refuses instead.
                return Err(WriteError::Unsupported(format!(
                    "{}: the existing record has type {t} (info {}), not paired data; heclib \
                     would delete it, the writer does not",
                    String::from_utf8_lossy(&r.pathname),
                    r.info.record_type
                )));
            }
        }
        if plan.elev {
            self.check_elev_location(&plan.path)?;
        }
        Ok(Prepared {
            path: plan.path.clone(),
            found,
            areas: plan.areas.clone(),
        })
    }
}

impl Engine<'_> {
    /// `zpdStore()` lines 400-424 read the record's location
    /// (`/A/B/Location Info////`, `zlocationRetrieve()`) and, when its supplemental text
    /// holds `verticalDatumInfo` (`VERTICAL_DATUM_INFO_USER_HEADER_PARAM`), pass it to
    /// `processStorageVdis()` (lines 425-455), which can refuse the store
    /// (`VERTICAL_DATUM_ERROR`) or set an offset that shifts the values (lines 456-495).
    /// That branch is not ported: an ELEV record whose location carries vertical-datum
    /// information is refused. (Observed with HEC's C library 7-JA: with a feet
    /// unit it stored the values unshifted; with a unit it does not recognise as feet or
    /// meters it returned -200536400 and stored nothing.) Without such a location the
    /// branch leaves values and units unchanged, which the writer reproduces word for word.
    fn check_elev_location(&self, path: &[u8]) -> Result<(), WriteError> {
        let mut rdr = self.reader()?;
        let loc = rdr.read_associated_location(path).map_err(|e| {
            WriteError::Corrupt(format!(
                "cannot read the location record of an ELEV record: {e}"
            ))
        })?;
        if let Some(l) = loc {
            let text = l.location.supplemental_text().unwrap_or_default();
            if text.to_ascii_lowercase().contains("verticaldatum") {
                return Err(WriteError::Unsupported(format!(
                    "{}: the location record holds vertical-datum information; heclib applies \
                     its vertical-datum rules to ELEV paired data there (zpdStore.c lines \
                     400-495), which the writer does not port",
                    String::from_utf8_lossy(path)
                )));
            }
        }
        Ok(())
    }
}

/// Store one paired-data record (one transaction, `dss7.md` §6.4).
pub(crate) fn write_pd_planned(
    file: &File,
    plan: &PdPlan,
    opts: &WriteOptions,
) -> Result<WriteReport, WriteError> {
    let mut session = Session::begin(file, opts)?;
    let prepared = session.eng.prepare_pd(plan)?;
    session.eng.block_index = 0;
    let report: BlockReport = session.eng.commit(prepared)?;
    session.finish(vec![report])
}
