//! DSS6 location update. DSS6 has no location records
//! (`Public/zlocationStore.c` lines 131-135: `INCOMPATIBLE_VERSION`). A location lives in
//! the time-series records themselves:
//! - coordinates and the six codes in the internal header, 0-based words 12..24 (regular,
//!   `zsrtsi6.f` lines 876-887) or 16..28 (irregular, `zsitsi6.f`), set by
//!   `ztsStoreReg6.c` lines 50-75 from `tss->locationStruct`;
//! - supplemental text in the user header (Monolith: `Hec_ztsStore.c` lines 599-606,
//!   `Utilities/verticalDatum.c:stringToUserHeader()`, four bytes per word, zero filled).
//!
//! `write_location6` rewrites every live time-series block of the dataset (same A, B, C,
//! E, F parts): the readers' location is one value per dataset, and blocks that disagree
//! raise `DssLocationConflictError` on read. Every block is built and checked
//! before the first store; each store is one transaction (`dss6.md` §9.3). Values, times, quality and
//! precision stay as stored; a compressed regular float block is rewritten uncompressed,
//! as the merge does.

use super::engine::{Engine, Found, Record6, I_COMP, I_LNDA, I_NCHE, I_PREC, I_QUAL, I_TYPE};
use super::{BlockReport, WriteError, WriteOptions, WriteReport};
use crate::dss6::compress;
use crate::dss6::source::ReadAt;
use crate::dss6::Dss6File;
use crate::dss7::write::location::{check_common, LocationFields, LocationInput};
use crate::dss7::write::validate_pd_pathname;
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;

/// `Dss6/ztsRetrieveReg6.c` lines 66-68: HEC reads at most 500 user-header words back.
pub const MAX_SUPPLEMENTAL_BYTES6: usize = 2000;

/// The catalog walk reads through the writer's own handle: another handle cannot read
/// the regions the held lock covers on Windows.
struct Handle<'a>(&'a File);

impl ReadAt for Handle<'_> {
    fn size(&self) -> u64 {
        self.0.metadata().map_or(0, |m| m.len())
    }

    fn read_exact_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        let mut f = self.0;
        f.seek(SeekFrom::Start(offset))?;
        f.read_exact(buf)
    }
}

fn parts(p: &[u8]) -> Option<Vec<&[u8]>> {
    let v: Vec<&[u8]> = p.split(|&b| b == b'/').collect();
    (v.len() == 8 && v[0].is_empty() && v[7].is_empty()).then(|| v[1..7].to_vec())
}

fn plan(input: &LocationInput) -> Result<(), WriteError> {
    check_common(input)?;
    if input.time_zone.is_some() {
        return Err(WriteError::Unsupported(
            "a DSS6 location has no time-zone field of its own; the record's time-zone words \
             are not location data (ztsStoreReg6.c lines 50-60). Pass no time zone"
                .into(),
        ));
    }
    validate_pd_pathname(input.pathname)?;
    if parts(input.pathname).is_none() {
        return Err(WriteError::BadPathname(
            "a DSS6 location pathname needs the /A/B/C/D/E/F/ form of its time series".into(),
        ));
    }
    if let Some(s) = input.supplemental {
        if s.len() > MAX_SUPPLEMENTAL_BYTES6 {
            return Err(WriteError::BadInput(format!(
                "supplemental text is {} bytes; HEC reads back at most \
                 {MAX_SUPPLEMENTAL_BYTES6} from a DSS6 user header (ztsRetrieveReg6.c)",
                s.len()
            )));
        }
    }
    Ok(())
}

fn words_of(ih: &[i32], at: usize) -> LocationFields {
    let d = |i: usize| {
        f64::from_bits(
            u64::from(ih[at + 2 * i] as u32) | (u64::from(ih[at + 2 * i + 1] as u32) << 32),
        )
    };
    let mut codes = [0i32; 6];
    codes.copy_from_slice(&ih[at + 6..at + 12]);
    LocationFields {
        xyz: [d(0), d(1), d(2)],
        codes,
        time_zone: None,
        supplemental: None,
    }
}

/// `stringToUserHeader()`: four bytes per word, zero filled; no words for empty text.
fn user_header_words(s: &[u8]) -> Vec<i32> {
    let mut b = s.to_vec();
    b.resize(s.len().div_ceil(4) * 4, 0);
    b.chunks_exact(4)
        .map(|c| i32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}

fn store(
    eng: &mut Engine,
    src: &File,
    input: &LocationInput,
) -> Result<Vec<BlockReport>, WriteError> {
    let upper: Vec<u8> = input.pathname.to_ascii_uppercase();
    let ours = parts(&upper).ok_or_else(|| WriteError::BadPathname("not /A/B/C/D/E/F/".into()))?;
    let rdr = Dss6File::new(Handle(src)).map_err(|e| WriteError::Corrupt(e.to_string()))?;
    let cat = rdr
        .catalog()
        .map_err(|e| WriteError::Corrupt(e.to_string()))?;
    // `DatasetIndex::build`: the record types of a time-series dataset.
    let mut names: Vec<Vec<u8>> = cat
        .entries
        .iter()
        .filter(|e| (100..=119).contains(&e.record_type))
        .filter(|e| {
            parts(&e.pathname).is_some_and(|p| {
                [0usize, 1, 2, 4, 5]
                    .iter()
                    .all(|&k| p[k].eq_ignore_ascii_case(ours[k]))
            })
        })
        .map(|e| e.pathname.clone())
        .collect();
    names.sort();
    names.dedup();
    if names.is_empty() {
        return Err(WriteError::Unsupported(format!(
            "{}: no time-series record matches; a DSS6 location is stored in time-series \
             records, and DSS6 has no location records (zlocationStore.c lines 131-135)",
            String::from_utf8_lossy(input.pathname)
        )));
    }
    let new_header = input.supplemental.map(user_header_words);
    let mut recs: Vec<Record6> = Vec::with_capacity(names.len());
    let mut headers: Vec<Vec<i32>> = Vec::with_capacity(names.len());
    for name in names {
        let shown = String::from_utf8_lossy(&name).into_owned();
        let look = eng.lookup(&name)?;
        let Found::Yes(ex) = &look.found else {
            return Err(WriteError::Corrupt(format!(
                "{shown}: listed in the catalog but not found"
            )));
        };
        let t = ex.f(I_TYPE);
        let at = match t {
            100 | 105 => 12,
            110 | 115 => 16,
            _ => {
                return Err(WriteError::Unsupported(format!(
                    "{shown}: record type {t} is not a plain regular or irregular series"
                )))
            }
        };
        let st = eng.read_stored(ex)?;
        if st.internal_header.len() < at + 12 {
            return Err(WriteError::Unsupported(format!(
                "{shown}: its internal header has {} words, too few for coordinates",
                st.internal_header.len()
            )));
        }
        let elevation = ours[2].len() >= 4 && ours[2][..4].eq_ignore_ascii_case(b"ELEV");
        if elevation
            && input.supplemental.is_some_and(|s| {
                s.to_ascii_lowercase()
                    .windows(13)
                    .any(|w| w == b"verticaldatum")
            })
        {
            // zsrtsi6.f lines 252-441 and the readers act on datum items in an elevation
            // record's user header; this is not informational text there.
            return Err(WriteError::Unsupported(format!(
                "{shown}: vertical-datum text in an elevation record's user header; HEC applies \
                 its datum rules to it (zsrtsi6.f lines 252-441), which the writer does not port"
            )));
        }
        let base = if input.keep_existing {
            words_of(&st.internal_header, at)
        } else {
            LocationFields::default()
        };
        let f = LocationFields::merged(input, base);
        // LCOORDS (ztsRetrieveReg6.c lines 239-248): without a non-zero coordinate readers
        // report no location.
        if f.xyz.iter().all(|&v| v == 0.0) {
            return Err(WriteError::BadInput(format!(
                "{shown}: a DSS6 location exists only with a non-zero coordinate \
                 (ztsRetrieveReg6.c lines 239-248); give x, y or z"
            )));
        }
        let mut ih = st.internal_header.clone();
        for (i, v) in f.xyz.iter().enumerate() {
            let b = v.to_bits();
            ih[at + 2 * i] = b as u32 as i32;
            ih[at + 2 * i + 1] = (b >> 32) as u32 as i32;
        }
        ih[at + 6..at + 12].copy_from_slice(&f.codes);
        let comp = ex.f(I_COMP);
        let data = if comp > 0 || ex.f(I_NCHE) > 0 {
            if t != 100 || ex.f(I_QUAL) > 0 {
                return Err(WriteError::Unsupported(format!(
                    "{shown}: a compressed record other than regular float without quality"
                )));
            }
            let (_, v) = compress::decode(&st.compression_header, &st.data, true)
                .map_err(|e| WriteError::Corrupt(format!("{shown}: compressed block: {e}")))?;
            if v.len() != ex.f(I_LNDA) as usize {
                return Err(WriteError::Corrupt(format!(
                    "{shown}: {} values decoded for a block of {}",
                    v.len(),
                    ex.f(I_LNDA)
                )));
            }
            v.iter().map(|x| x.to_bits() as i32).collect()
        } else {
            st.data.clone()
        };
        // Always read: this checks the stored header's length and address before the first
        // store, and Engine::store may compare against it.
        let stored_header = eng.read_user_header(ex)?;
        let header = match (&new_header, input.keep_existing) {
            (Some(h), _) => h.clone(),
            (None, true) => stored_header,
            (None, false) => Vec::new(),
        };
        recs.push(Record6 {
            pathname: name,
            record_type: t,
            internal_header: ih,
            data,
            logical: ex.f(I_LNDA),
            quality: ex.f(I_QUAL) > 0,
            precision: ex.f(I_PREC),
        });
        headers.push(header);
    }
    eng.store_prepared(&recs, headers)
}

/// Update the location of a time-series dataset in the existing DSS6 file at `path`.
pub fn write_location6(
    path: &Path,
    input: &LocationInput,
    opts: &WriteOptions,
) -> Result<WriteReport, WriteError> {
    plan(input)?;
    let file = OpenOptions::new().read(true).write(true).open(path)?;
    write_location6_to(&file, input, opts)
}

/// Same as [`write_location6`] on an already open read-write handle.
pub fn write_location6_to(
    file: &File,
    input: &LocationInput,
    opts: &WriteOptions,
) -> Result<WriteReport, WriteError> {
    plan(input)?;
    super::engine::write_with(file, opts, |eng| store(eng, file, input))
}
