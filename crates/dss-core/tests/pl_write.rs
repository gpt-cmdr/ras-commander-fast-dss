//! P39 profile and location writer tests: round trips, merges, refusals that leave the
//! file byte-identical, every other record unchanged, a crash at every step with recovery,
//! replay of the heclib-written fixture word for word, and an input fuzz. Synthetic data.
//!
//! Natively written files were also checked with HEC software; those checks need HEC
//! software and do not run in this test suite.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use dss_core::dss6::write::{
    check_file6, create_dss6, write_location6, write_rts6, DSS6_STEP_ORDER,
};
use dss_core::dss6::Dss6File;
use dss_core::dss7::write::{
    create_dss7, validate_profile_input, write_location, write_pd, write_profile, write_rts,
    BlockOutcome, FaultPoint, LocationInput, PdArrays, PdInput, ProfileArrays, ProfileInput,
    RtsInput, RtsValues, WriteError, WriteOptions, WriteStep,
};
use dss_core::dss7::Dss7File;
use dss_core::TsValues;

const J2020: i64 = 43_830;
const MISSING: f32 = -f32::MAX;
const DEPTHS: [f64; 3] = [0.5, 1.5, 3.0];

fn tmp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("pl_write_{}_{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn opts() -> WriteOptions {
    WriteOptions {
        sync: dss_core::dss7::write::SyncMode::None,
        ..WriteOptions::default()
    }
}

fn t(day: i64, hour: i64) -> i64 {
    (J2020 + day) * 86_400 + hour * 3_600
}

fn rows(n: usize, base: f64) -> Vec<f64> {
    (0..n)
        .flat_map(|i| (0..3).map(move |j| base + i as f64 + j as f64 * 0.25))
        .collect()
}

fn input<'a>(path: &'a [u8], start: i64, depths: &'a [f64], v: &'a [f64]) -> ProfileInput<'a> {
    ProfileInput::new(
        path,
        start,
        ProfileArrays::F64 { depths, values: v },
        b"m",
        b"deg C",
        b"INST-VAL",
    )
}

fn loc<'a>(path: &'a [u8]) -> LocationInput<'a> {
    LocationInput {
        pathname: path,
        x: Some(-97.25),
        y: Some(30.5),
        z: Some(612.5),
        coordinate_system: Some(1),
        coordinate_id: Some(0),
        horizontal_units: Some(3),
        horizontal_datum: Some(1),
        vertical_units: Some(1),
        vertical_datum: Some(2),
        time_zone: Some(b"America/Chicago"),
        supplemental: Some(b"Key:Value;"),
        keep_existing: false,
    }
}

/// Every record's type, program-independent areas, sizes and dates.
type Snap = BTreeMap<Vec<u8>, (i32, Vec<Vec<u8>>, i64, i64)>;

fn snapshot7(p: &Path) -> Snap {
    let bytes = std::fs::read(p).unwrap();
    let mut f = Dss7File::open(p).unwrap();
    let cat = f.catalog().unwrap();
    let mut out = BTreeMap::new();
    for i in cat.resolved_live() {
        let name = cat.catalog.entries[i].pathname.clone();
        // A crash before Link leaves an entry in a bin no lookup reaches yet.
        let Some(r) = f.lookup(&name).unwrap() else {
            continue;
        };
        let mut areas = Vec::new();
        for a in [
            &r.info.internal_header,
            &r.info.header2,
            &r.info.user_header,
            &r.info.values1,
            &r.info.values2,
            &r.info.values3,
        ] {
            let (at, n) = (a.address as usize * 8, a.number_ints.max(0) as usize * 4);
            let b = if n > 0 {
                bytes[at..at + n].to_vec()
            } else {
                Vec::new()
            };
            areas.push(b);
        }
        out.insert(
            name,
            (
                r.info.record_type,
                areas,
                r.info.number_data,
                r.info.logical_number,
            ),
        );
    }
    out
}

fn read_f64(p: &Path, path: &[u8]) -> (Vec<f64>, Vec<f64>) {
    let mut f = Dss7File::open(p).unwrap();
    let r = f.lookup(path).unwrap().expect("record");
    let a = f.read_ts_profile_record(&r).unwrap();
    let (TsValues::F64(d), TsValues::F64(v)) = (a.depths, a.values) else {
        panic!("not doubles");
    };
    (d, v)
}

#[test]
fn profile_round_trip_both_widths() {
    let d = tmp("rt");
    let p = d.join("a.dss");
    create_dss7(&p).unwrap();
    // Across a month boundary: two blocks.
    let v = rows(10, 1.0);
    let path = b"/P39/F64/DEPTH-TEMP//1Hour/T/";
    let mut i = input(path, t(30, 20), &DEPTHS, &v);
    i.time_zone = Some(b"UTC");
    i.precision = 2;
    let rep = write_profile(&p, &i, &opts()).unwrap();
    assert_eq!(rep.blocks.len(), 2);
    assert!(rep.blocks.iter().all(|b| b.outcome == BlockOutcome::Added));
    assert!(rep.blocks.iter().all(|b| b.record_type == 107));
    let mut f = Dss7File::open(&p).unwrap();
    let a = f
        .read_ts_profile(path, &dss_core::dss7::ts::TsReadOptions::default())
        .unwrap();
    assert_eq!(a.record_type, 107);
    assert_eq!(a.depth_units, "m");
    assert_eq!(a.value_units, "deg C");
    assert_eq!(a.time_zone.as_deref(), Some("UTC"));
    let TsValues::F64(got) = &a.values else {
        panic!("doubles")
    };
    let pos = a
        .times
        .iter()
        .position(|&x| x == t(30, 20))
        .expect("first time");
    let got = &got[pos * 3..(pos + 10) * 3];
    assert!(got.iter().zip(&v).all(|(x, y)| x.to_bits() == y.to_bits()));
    assert_eq!(a.times[pos + 9], t(31, 5));
    // Float: 102, stored width kept; NaN keeps its bits.
    let df = [1.0f32, 2.0];
    let vf = [1.5f32, f32::NAN, MISSING, MISSING, 3.5, -901.0];
    let pf = b"/P39/F32/DEPTH-TEMP//1Hour/T/";
    let r = write_profile(
        &p,
        &ProfileInput::new(
            pf,
            t(10, 1),
            ProfileArrays::F32 {
                depths: &df,
                values: &vf,
            },
            b"ft",
            b"F",
            b"PER-AVER",
        ),
        &opts(),
    )
    .unwrap();
    assert_eq!(r.blocks[0].record_type, 102);
    let mut f = Dss7File::open(&p).unwrap();
    let rec = f.lookup(&r.blocks[0].pathname).unwrap().unwrap();
    let a = f.read_ts_profile_record(&rec).unwrap();
    let TsValues::F32(got) = &a.values else {
        panic!("floats")
    };
    let pos = a.times.iter().position(|&s| s == t(10, 1)).expect("time");
    for (k, want) in vf.iter().enumerate() {
        assert_eq!(got[pos * 2 + k].to_bits(), want.to_bits(), "cell {k}");
    }
}

#[test]
fn merge_keeps_rows_and_refuses_other_depths() {
    let d = tmp("merge");
    let p = d.join("a.dss");
    create_dss7(&p).unwrap();
    let path = b"/P39/M/DEPTH-TEMP//1Hour/T/";
    let block = b"/P39/M/DEPTH-TEMP/01Jan2020/1Hour/T/";
    let v = rows(6, 1.0);
    write_profile(&p, &input(path, t(10, 1), &DEPTHS, &v), &opts()).unwrap();
    let (_, stored) = read_f64(&p, block);
    let mut f = Dss7File::open(&p).unwrap();
    let r = f.lookup(block).unwrap().unwrap();
    let times = f.read_ts_profile_record(&r).unwrap().times;
    let at = times.iter().position(|&x| x == t(10, 1)).expect("time") * 3;
    assert_eq!(stored[at..at + 18], v[..]);
    let nv = rows(6, 50.0);
    let r = write_profile(&p, &input(path, t(10, 4), &DEPTHS, &nv), &opts()).unwrap();
    assert_eq!(r.blocks[0].outcome, BlockOutcome::Replaced);
    let (_, got) = read_f64(&p, block);
    assert_eq!(got[at..at + 9], v[..9]); // first three rows kept
    assert_eq!(got[at + 9..at + 27], nv[..]); // six new rows
    let before = std::fs::read(&p).unwrap();
    // Merge with other depths: refused, byte-identical.
    let other = [0.5, 1.5, 4.0];
    let e = write_profile(&p, &input(path, t(10, 5), &other, &nv), &opts()).unwrap_err();
    assert!(matches!(e, WriteError::Unsupported(_)), "{e}");
    // Another number of depths.
    let two = [0.5, 1.5];
    let e = write_profile(&p, &input(path, t(10, 5), &two, &[1.0, 2.0]), &opts()).unwrap_err();
    assert!(matches!(e, WriteError::Unsupported(_)), "{e}");
    // A new block of the dataset with other depths: refused.
    let e = write_profile(&p, &input(path, t(100, 1), &other, &nv), &opts()).unwrap_err();
    assert!(matches!(e, WriteError::Unsupported(_)), "{e}");
    // Width change: refused.
    let (df, vf) = ([0.5f32, 1.5, 3.0], [1.0f32, 2.0, 3.0]);
    let e = write_profile(
        &p,
        &ProfileInput::new(
            path,
            t(10, 5),
            ProfileArrays::F32 {
                depths: &df,
                values: &vf,
            },
            b"m",
            b"deg C",
            b"INST-VAL",
        ),
        &opts(),
    )
    .unwrap_err();
    assert!(matches!(e, WriteError::Unsupported(_)), "{e}");
    // A time series at the pathname: refused; a profile at a time-series pathname too.
    let sv = [1.0f32, 2.0];
    write_rts(
        &p,
        &RtsInput::new(
            b"/P39/S/FLOW//1Hour/T/",
            t(10, 1),
            RtsValues::F32(&sv),
            b"CFS",
            b"INST-VAL",
        ),
        &opts(),
    )
    .unwrap();
    let before2 = std::fs::read(&p).unwrap();
    assert_ne!(before, before2);
    let e = write_profile(
        &p,
        &input(b"/P39/S/FLOW//1Hour/T/", t(10, 1), &DEPTHS, &nv),
        &opts(),
    )
    .unwrap_err();
    assert!(matches!(e, WriteError::Unsupported(_)), "{e}");
    assert_eq!(before2, std::fs::read(&p).unwrap());
    let e = write_location(&p, &loc(b"/P39/S/FLOW//1Hour/T/"), &opts());
    assert!(e.is_ok());
}

#[test]
fn input_refusals() {
    let v = rows(2, 1.0);
    let ok = |i: &ProfileInput| validate_profile_input(i);
    assert!(ok(&input(b"/A/B/C//1Hour/F/", t(1, 1), &DEPTHS, &v)).is_ok());
    for (i, why) in [
        (input(b"/A/B/C//1Hour/F/", t(1, 1), &[], &v), "no depths"),
        (
            input(b"/A/B/C//1Hour/F/", t(1, 1), &DEPTHS, &[]),
            "no values",
        ),
        (
            input(b"/A/B/C//1Hour/F/", t(1, 1), &DEPTHS, &v[..4]),
            "partial row",
        ),
        (input(b"/A/B/C//NOPE/F/", t(1, 1), &DEPTHS, &v), "interval"),
        (
            input(b"/A/B/C/TS-PATTERN/1Hour/F/", t(1, 1), &DEPTHS, &v),
            "pattern",
        ),
        (input(b"/A/B/C//1Hour/F", t(1, 1), &DEPTHS, &v), "slashes"),
        (input(b"/A/B/C//1Hour/F/", 0, &DEPTHS, &v), "undefined time"),
    ] {
        assert!(ok(&i).is_err(), "{why}");
    }
    let q = [1];
    let mut i = input(b"/A/B/C//1Hour/F/", t(1, 1), &DEPTHS, &v);
    i.quality = Some(&q);
    assert!(ok(&i).is_err());
    let d = tmp("refuse");
    let p = d.join("a.dss");
    create_dss7(&p).unwrap();
    let before = std::fs::read(&p).unwrap();
    // zlocationStructValid(): only horizontal/vertical datum among the codes count.
    let none = LocationInput {
        pathname: b"/A/B/C///F/",
        coordinate_system: Some(1),
        ..LocationInput::default()
    };
    assert!(matches!(
        write_location(&p, &none, &opts()),
        Err(WriteError::BadInput(_))
    ));
    let long = [b'X'; 72];
    let mut l = loc(b"/A/B/C///F/");
    l.time_zone = Some(&long);
    assert!(matches!(
        write_location(&p, &l, &opts()),
        Err(WriteError::BadInput(_))
    ));
    let mut l = loc(b"/A/B/C///F/");
    l.supplemental = Some(b"a\0b");
    assert!(write_location(&p, &l, &opts()).is_err());
    assert!(write_location(&p, &loc(b"/A"), &opts()).is_err());
    assert_eq!(before, std::fs::read(&p).unwrap());
    // 71 bytes fit, terminated; a name of 4 bytes gets a NUL int too.
    let mut l = loc(b"/A/B/C///F/");
    l.time_zone = Some(&long[..71]);
    write_location(&p, &l, &opts()).unwrap();
    let mut f = Dss7File::open(&p).unwrap();
    let got = f
        .read_associated_location(b"/A/B/ANYTHING/D/E/F/")
        .unwrap()
        .unwrap();
    assert_eq!(got.location.time_zone_name.unwrap().len(), 71);
    assert_eq!(got.values1.len(), 30);
    let mut l = loc(b"/A/B4/C///F/");
    l.time_zone = Some(b"ABCD");
    write_location(&p, &l, &opts()).unwrap();
    let got = Dss7File::open(&p)
        .unwrap()
        .read_location(b"/A/B4/Location Info////")
        .unwrap();
    assert_eq!(got.values1.len(), 14);
    assert_eq!(got.values1[13], 0);
    // A DSS6 file is refused by the DSS7 writers, and DSS7 by the DSS6 writer.
    let p6 = d.join("b.dss");
    create_dss6(&p6).unwrap();
    let before6 = std::fs::read(&p6).unwrap();
    assert!(write_location(&p6, &loc(b"/A/B/C///F/"), &opts()).is_err());
    assert!(write_profile(
        &p6,
        &input(b"/A/B/C//1Hour/F/", t(1, 1), &DEPTHS, &v),
        &opts()
    )
    .is_err());
    assert_eq!(before6, std::fs::read(&p6).unwrap());
    let mut l = loc(b"/A/B/FLOW//1HOUR/F/");
    l.time_zone = None;
    assert!(write_location6(&p, &l, &opts()).is_err());
}

#[test]
fn location_update_loses_nothing() {
    let d = tmp("loc");
    let p = d.join("a.dss");
    create_dss7(&p).unwrap();
    let path = b"/P39/L/DEPTH-TEMP//1Hour/T/";
    let lp = b"/P39/L/Location Info////";
    write_location(&p, &loc(path), &opts()).unwrap();
    let v = rows(4, 1.0);
    write_profile(&p, &input(path, t(10, 1), &DEPTHS, &v), &opts()).unwrap();
    let want = Dss7File::open(&p).unwrap().read_location(lp).unwrap();
    // Replacing the data keeps the location record, word for word.
    let nv = rows(4, 9.0);
    write_profile(&p, &input(path, t(10, 1), &DEPTHS, &nv), &opts()).unwrap();
    let got = Dss7File::open(&p).unwrap().read_location(lp).unwrap();
    assert_eq!(got.values1, want.values1);
    assert_eq!(got.location.supplemental, want.location.supplemental);
    // A partial update keeps every other field.
    let u = LocationInput {
        pathname: path,
        z: Some(1000.0),
        keep_existing: true,
        ..LocationInput::default()
    };
    let r = write_location(&p, &u, &opts()).unwrap();
    assert_eq!(r.blocks[0].outcome, BlockOutcome::Replaced);
    let got = Dss7File::open(&p)
        .unwrap()
        .read_location(lp)
        .unwrap()
        .location;
    assert_eq!(got.z, 1000.0);
    assert_eq!(got.x, -97.25);
    assert_eq!(got.vertical_datum_code, 2);
    assert_eq!(got.time_zone_name.as_deref(), Some("America/Chicago"));
    assert_eq!(got.supplemental.as_deref(), Some(&b"Key:Value;"[..]));
    // keep_existing = false stores the struct as given, as heclib does.
    let u = LocationInput {
        pathname: path,
        x: Some(5.0),
        keep_existing: false,
        ..LocationInput::default()
    };
    write_location(&p, &u, &opts()).unwrap();
    let got = Dss7File::open(&p).unwrap().read_location(lp).unwrap();
    assert_eq!(got.values1.len(), 12);
    assert!(got.location.supplemental.is_none() && got.location.time_zone_name.is_none());
    // An equal write writes nothing.
    let bytes = std::fs::read(&p).unwrap();
    let r = write_location(&p, &u, &opts()).unwrap();
    assert_eq!(r.blocks[0].outcome, BlockOutcome::Unchanged);
    assert_eq!(bytes, std::fs::read(&p).unwrap());
    // A non-location record at the location pathname is refused.
    let (o, cv) = ([1.0f32, 2.0], [3.0f32, 4.0]);
    write_pd(
        &p,
        &PdInput::new(
            b"/X/Y/Location Info////",
            PdArrays::F32 {
                ordinates: &o,
                values: &cv,
            },
            1,
            b"FT",
            b"LINEAR",
            b"CFS",
            b"LINEAR",
        ),
        &opts(),
    )
    .unwrap();
    let bytes = std::fs::read(&p).unwrap();
    let e = write_location(&p, &loc(b"/X/Y/ANY///F/"), &opts()).unwrap_err();
    assert!(matches!(e, WriteError::Unsupported(_)), "{e}");
    assert_eq!(bytes, std::fs::read(&p).unwrap());
}

fn crash7(
    name: &str,
    base: &Path,
    d: &Path,
    go: &dyn Fn(&Path, &WriteOptions) -> Result<(), WriteError>,
) {
    let before = snapshot7(base);
    let reference = d.join(format!("{name}_ref.dss"));
    std::fs::copy(base, &reference).unwrap();
    go(&reference, &opts()).unwrap();
    let want = snapshot7(&reference);
    let changed: Vec<&Vec<u8>> = want
        .keys()
        .filter(|k| before.get(*k) != want.get(*k))
        .collect();
    assert!(!changed.is_empty(), "{name} changes something");
    for step in WriteStep::ALL {
        let f = d.join(format!("{name}_{step:?}.dss"));
        std::fs::copy(base, &f).unwrap();
        let o = WriteOptions {
            fault: Some(FaultPoint { block: 0, step }),
            ..opts()
        };
        match go(&f, &o) {
            Err(WriteError::InjectedFault { .. }) => {}
            other => panic!("{name} {step:?}: {other:?}"),
        }
        // Readable, every record old or new.
        let s = snapshot7(&f);
        for (k, v) in &s {
            assert!(
                before.get(k) == Some(v) || want.get(k) == Some(v),
                "{name} {step:?}: {} is neither old nor new",
                String::from_utf8_lossy(k)
            );
        }
        for k in before.keys() {
            assert!(s.contains_key(k), "{name} {step:?}: lost a record");
        }
        // The next write reconciles and finishes the same content.
        go(&f, &opts()).unwrap();
        assert_eq!(snapshot7(&f), want, "{name} {step:?}: after recovery");
    }
}

#[test]
fn crash_after_every_step_dss7() {
    let d = tmp("crash7");
    let base = d.join("base.dss");
    create_dss7(&base).unwrap();
    let v = rows(4, 1.0);
    let path: &[u8] = b"/C/P/DEPTH-TEMP//1Hour/T/";
    write_profile(&base, &input(path, t(10, 1), &DEPTHS, &v), &opts()).unwrap();
    let other = rows(3, 2.0);
    write_profile(
        &base,
        &input(b"/C/OTHER/DEPTH-TEMP//1Hour/T/", t(10, 1), &DEPTHS, &other),
        &opts(),
    )
    .unwrap();
    write_location(&base, &loc(b"/C/L/X///F/"), &opts()).unwrap();
    let nv = rows(4, 30.0);
    crash7("newprofile", &base, &d, &|f, o| {
        write_profile(
            f,
            &input(b"/C/NEW/DEPTH-TEMP//1Hour/T/", t(10, 1), &DEPTHS, &nv),
            o,
        )
        .map(|_| ())
    });
    crash7("rewrite", &base, &d, &|f, o| {
        write_profile(f, &input(path, t(10, 1), &DEPTHS, &nv), o).map(|_| ())
    });
    crash7("merge", &base, &d, &|f, o| {
        write_profile(f, &input(path, t(10, 3), &DEPTHS, &nv), o).map(|_| ())
    });
    crash7("newloc", &base, &d, &|f, o| {
        write_location(f, &loc(b"/C/N/X///F/"), o).map(|_| ())
    });
    crash7("reloc", &base, &d, &|f, o| {
        let mut l = loc(b"/C/L/X///F/");
        l.supplemental = Some(b"A longer text than the stored one;");
        write_location(f, &l, o).map(|_| ())
    });
}

fn locate6(p: &Path, block: &[u8]) -> dss_core::Location {
    let f = Dss6File::open(p).unwrap();
    let r = f.read_ts_record(block).unwrap();
    dss_core::Location::from_dss6_ts(block, r.coordinates, r.coordinate_descriptors)
        .expect("location")
        .with_dss6_user_header(&r.user_header)
}

fn snap6(p: &Path) -> BTreeMap<Vec<u8>, (Vec<f64>, Option<dss_core::Location>)> {
    let f = Dss6File::open(p).unwrap();
    let cat = f.catalog().unwrap();
    let mut out = BTreeMap::new();
    for e in cat.entries {
        let r = f.read_ts_record(&e.pathname).unwrap();
        let TsValues::F32(v) = &r.values else {
            panic!("floats");
        };
        let l =
            dss_core::Location::from_dss6_ts(&e.pathname, r.coordinates, r.coordinate_descriptors)
                .map(|l| l.with_dss6_user_header(&r.user_header));
        out.insert(e.pathname, (v.iter().map(|&x| f64::from(x)).collect(), l));
    }
    out
}

#[test]
fn dss6_location_on_every_block_and_kept_by_rewrites() {
    let d = tmp("loc6");
    let p = d.join("a.dss");
    create_dss6(&p).unwrap();
    let v: Vec<f32> = (0..8).map(|i| i as f32).collect();
    let path: &[u8] = b"/L/G/FLOW//1HOUR/T/";
    let rep = write_rts6(
        &p,
        &RtsInput::new(path, t(30, 22), RtsValues::F32(&v), b"CFS", b"INST-VAL"),
        &opts(),
    )
    .unwrap();
    assert_eq!(rep.blocks.len(), 2);
    let names: Vec<Vec<u8>> = rep.blocks.iter().map(|b| b.pathname.clone()).collect();
    write_rts6(
        &p,
        &RtsInput::new(
            b"/L/OTHER/FLOW//1HOUR/T/",
            t(1, 1),
            RtsValues::F32(&v),
            b"CFS",
            b"INST-VAL",
        ),
        &opts(),
    )
    .unwrap();
    // A time zone has no DSS6 location field; nothing to attach to is refused.
    let before = std::fs::read(&p).unwrap();
    assert!(write_location6(&p, &loc(path), &opts()).is_err());
    let mut l = loc(b"/L/NONE/FLOW//1HOUR/T/");
    l.time_zone = None;
    assert!(matches!(
        write_location6(&p, &l, &opts()),
        Err(WriteError::Unsupported(_))
    ));
    let zero = LocationInput {
        pathname: path,
        coordinate_system: Some(1),
        keep_existing: true,
        ..LocationInput::default()
    };
    assert!(matches!(
        write_location6(&p, &zero, &opts()),
        Err(WriteError::BadInput(_))
    ));
    let long = vec![b'a'; 2001];
    let mut l = loc(path);
    l.time_zone = None;
    l.supplemental = Some(&long);
    assert!(write_location6(&p, &l, &opts()).is_err());
    assert_eq!(before, std::fs::read(&p).unwrap());
    // Stored on every block, values unchanged, other dataset unchanged.
    let values_before = snap6(&p);
    let mut l = loc(path);
    l.time_zone = None;
    let r = write_location6(&p, &l, &opts()).unwrap();
    assert_eq!(r.blocks.len(), 2);
    assert!(check_file6(&p).unwrap().problems.is_empty());
    let after = snap6(&p);
    for (k, (vals, _)) in &values_before {
        assert_eq!(
            &after[k].0,
            vals,
            "values of {}",
            String::from_utf8_lossy(k)
        );
    }
    assert!(after[&b"/L/OTHER/FLOW/01JAN2020/1HOUR/T/".to_vec()]
        .1
        .is_none());
    for n in &names {
        let g = locate6(&p, n);
        assert_eq!((g.x, g.y, g.z), (-97.25, 30.5, 612.5));
        assert_eq!(g.vertical_datum_code, 2);
        assert_eq!(g.supplemental.as_deref(), Some(&b"Key:Value;"[..]));
    }
    // A time-series rewrite keeps coordinates and text (a deliberate deviation from zsrtsi6.f).
    let nv: Vec<f32> = (0..4).map(|i| 100.0 + i as f32).collect();
    write_rts6(
        &p,
        &RtsInput::new(path, t(30, 23), RtsValues::F32(&nv), b"CFS", b"INST-VAL"),
        &opts(),
    )
    .unwrap();
    assert!(check_file6(&p).unwrap().problems.is_empty());
    for n in &names {
        let g = locate6(&p, n);
        assert_eq!(g.x, -97.25);
        assert_eq!(g.supplemental.as_deref(), Some(&b"Key:Value;"[..]));
    }
    // A partial update changes one field on every block and keeps the text.
    let u = LocationInput {
        pathname: path,
        z: Some(1.25),
        keep_existing: true,
        ..LocationInput::default()
    };
    write_location6(&p, &u, &opts()).unwrap();
    for n in &names {
        let g = locate6(&p, n);
        assert_eq!((g.x, g.z), (-97.25, 1.25));
        assert_eq!(g.supplemental.as_deref(), Some(&b"Key:Value;"[..]));
    }
    // An equal write stores nothing.
    let bytes = std::fs::read(&p).unwrap();
    let r = write_location6(&p, &u, &opts()).unwrap();
    assert!(r
        .blocks
        .iter()
        .all(|b| b.outcome == BlockOutcome::Unchanged));
    assert_eq!(bytes, std::fs::read(&p).unwrap());
}

#[test]
fn crash_after_every_step_dss6() {
    let d = tmp("crash6");
    let base = d.join("base.dss");
    create_dss6(&base).unwrap();
    let v: Vec<f32> = (0..3).map(|i| i as f32).collect();
    let path: &[u8] = b"/C/G/FLOW//1HOUR/T/";
    write_rts6(
        &base,
        &RtsInput::new(path, t(10, 1), RtsValues::F32(&v), b"CFS", b"INST-VAL"),
        &opts(),
    )
    .unwrap();
    write_rts6(
        &base,
        &RtsInput::new(
            b"/C/OTHER/FLOW//1HOUR/T/",
            t(10, 1),
            RtsValues::F32(&v),
            b"CFS",
            b"INST-VAL",
        ),
        &opts(),
    )
    .unwrap();
    let mut l = loc(path);
    l.time_zone = None;
    let reference = d.join("ref.dss");
    std::fs::copy(&base, &reference).unwrap();
    write_location6(&reference, &l, &opts()).unwrap();
    let want = snap6(&reference);
    let before = snap6(&base);
    for step in DSS6_STEP_ORDER {
        let f = d.join(format!("{step:?}.dss"));
        std::fs::copy(&base, &f).unwrap();
        let o = WriteOptions {
            fault: Some(FaultPoint { block: 0, step }),
            ..opts()
        };
        match write_location6(&f, &l, &o) {
            Err(WriteError::InjectedFault { .. }) => {}
            other => panic!("{step:?}: {other:?}"),
        }
        let s = snap6(&f);
        for (k, val) in &s {
            assert!(
                before.get(k) == Some(val) || want.get(k) == Some(val),
                "{step:?}"
            );
        }
        write_location6(&f, &l, &opts()).unwrap();
        assert_eq!(snap6(&f), want, "{step:?}: after recovery");
        assert!(check_file6(&f).unwrap().problems.is_empty(), "{step:?}");
    }
}

// -- replay of the heclib-written fixture ---------------------------------------------

fn hex(s: &str) -> Vec<u64> {
    s.split(',')
        .filter(|x| !x.is_empty() && *x != "~")
        .map(|x| u64::from_str_radix(x, 16).unwrap())
        .collect()
}

fn unhex(s: &str) -> Option<Vec<u8>> {
    (s != "~").then(|| {
        (0..s.len() / 2)
            .map(|i| u8::from_str_radix(&s[2 * i..2 * i + 2], 16).unwrap())
            .collect()
    })
}

fn fixture(name: &str) -> PathBuf {
    let p = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/public")
        .join(name);
    assert!(p.exists(), "committed fixture {name} is missing");
    p
}

/// `p39_hec_pl7.dss` holds `p39_hec_pl7_spec.tsv` stored by heclib 7-JA. The same spec
/// through the native writers gives the same records word for word; only a location
/// record's first and last dates differ (heclib leaves those of the previous time-series
/// write on the handle), and they are not part of the snapshot.
#[test]
fn replay_matches_heclib_fixture() {
    let d = tmp("replay");
    let p = d.join("n.dss");
    create_dss7(&p).unwrap();
    let spec = std::fs::read_to_string(fixture("p39_hec_pl7_spec.tsv")).unwrap();
    for line in spec.lines().filter(|l| !l.is_empty()) {
        let c: Vec<&str> = line.split('\t').collect();
        match c[0] {
            "P" => {
                let (dw, vw) = (hex(c[4]), hex(c[5]));
                let (fd, fv): (Vec<f32>, Vec<f32>) = (
                    dw.iter().map(|&b| f32::from_bits(b as u32)).collect(),
                    vw.iter().map(|&b| f32::from_bits(b as u32)).collect(),
                );
                let (dd, dv): (Vec<f64>, Vec<f64>) = (
                    dw.iter().map(|&b| f64::from_bits(b)).collect(),
                    vw.iter().map(|&b| f64::from_bits(b)).collect(),
                );
                let data = if c[1] == "float" {
                    ProfileArrays::F32 {
                        depths: &fd,
                        values: &fv,
                    }
                } else {
                    ProfileArrays::F64 {
                        depths: &dd,
                        values: &dv,
                    }
                };
                let q: Option<Vec<i32>> =
                    (c[6] != "~").then(|| c[6].split(',').map(|x| x.parse().unwrap()).collect());
                let mut i = ProfileInput::new(
                    c[2].as_bytes(),
                    c[3].parse().unwrap(),
                    data,
                    c[7].as_bytes(),
                    c[8].as_bytes(),
                    c[9].as_bytes(),
                );
                i.quality = q.as_deref();
                i.time_zone = (c[10] != "~").then(|| c[10].as_bytes());
                i.precision = c[11].parse().unwrap();
                write_profile(&p, &i, &opts()).unwrap();
            }
            "T" => {
                let fv: Vec<f32> = hex(c[4])
                    .iter()
                    .map(|&b| f32::from_bits(b as u32))
                    .collect();
                write_rts(
                    &p,
                    &RtsInput::new(
                        c[2].as_bytes(),
                        c[3].parse().unwrap(),
                        RtsValues::F32(&fv),
                        c[5].as_bytes(),
                        c[6].as_bytes(),
                    ),
                    &opts(),
                )
                .unwrap();
            }
            _ => {
                let f = |i: usize| Some(f64::from_bits(u64::from_str_radix(c[i], 16).unwrap()));
                let n = |i: usize| Some(c[i].parse::<i32>().unwrap());
                let (tz, sup) = (unhex(c[11]), unhex(c[12]));
                let l = LocationInput {
                    pathname: c[1].as_bytes(),
                    x: f(2),
                    y: f(3),
                    z: f(4),
                    coordinate_system: n(5),
                    coordinate_id: n(6),
                    horizontal_units: n(7),
                    horizontal_datum: n(8),
                    vertical_units: n(9),
                    vertical_datum: n(10),
                    time_zone: tz.as_deref(),
                    supplemental: sup.as_deref(),
                    keep_existing: false,
                };
                write_location(&p, &l, &opts()).unwrap();
            }
        }
    }
    let (ours, hec) = (snapshot7(&p), snapshot7(&fixture("p39_hec_pl7.dss")));
    assert_eq!(ours.len(), hec.len());
    for (k, v) in &hec {
        assert_eq!(ours.get(k), Some(v), "{}", String::from_utf8_lossy(k));
    }
    // Header location bounds (zlocationUpdatePerm()): words 84-88 as heclib left them.
    let word = |f: &Path, i: usize| {
        let b = std::fs::read(f).unwrap();
        i64::from_le_bytes(b[i * 8..i * 8 + 8].try_into().unwrap())
    };
    for i in 84..=88 {
        assert_eq!(
            word(&p, i),
            word(&fixture("p39_hec_pl7.dss"), i),
            "word {i}"
        );
    }
    assert_ne!(word(&p, 84), 0);
}

#[test]
fn fuzz_inputs_never_panic() {
    let d = tmp("fuzz");
    let p = d.join("f.dss");
    create_dss7(&p).unwrap();
    let mut s = 0x9e37_79b9_7f4a_7c15u64;
    let mut next = move || {
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        s
    };
    let intervals: [&[u8]; 4] = [b"1Hour", b"1Day", b"15Minute", b"Bad"];
    for i in 0..300 {
        let nd = (next() % 4) as usize;
        let n = (next() % 6) as usize;
        let extra = usize::from(next() % 5 == 0);
        let depths: Vec<f64> = (0..nd).map(|k| k as f64).collect();
        let vals: Vec<f64> = (0..n * nd + extra).map(|k| k as f64).collect();
        let e = intervals[(next() % 4) as usize];
        let path = [b"/F/B", i.to_string().as_bytes(), b"/C//", e, b"/F/"].concat();
        let start = t((next() % 800) as i64, (next() % 24) as i64);
        let inp = input(&path, start, &depths, &vals);
        if write_profile(&p, &inp, &opts()).is_ok() {
            assert!(nd > 0 && !vals.is_empty() && vals.len() % nd == 0);
        }
    }
    assert!(!snapshot7(&p).is_empty());
}

// -- Refusals and edge cases ------------------------------------------------------------

/// One row of 65,536 doubles in a 1Second block is 86,400 x 65,536 x 8
/// bytes (45 GB) of missing-value buffer. Validation refuses it before any allocation.
#[test]
fn expanded_block_is_bounded_before_allocation() {
    let depths = vec![0.0f64; 65_536];
    let values = vec![1.0f64; 65_536];
    let path: &[u8] = b"/P39/BIG/DEPTH-TEMP//1Second/T/";
    let i = input(path, t(10, 1), &depths, &values);
    let e = validate_profile_input(&i).unwrap_err();
    assert!(matches!(e, WriteError::BadInput(_)), "{e}");
    assert!(e.to_string().contains("expand"), "{e}");
    // The same through a write: refused before the file is touched.
    let d = tmp("big");
    let p = d.join("a.dss");
    create_dss7(&p).unwrap();
    let before = std::fs::read(&p).unwrap();
    assert!(matches!(
        write_profile(&p, &i, &opts()),
        Err(WriteError::BadInput(_))
    ));
    assert_eq!(before, std::fs::read(&p).unwrap());
    // A whole-block store materializes only what it is given, so a long small profile passes.
    let v = vec![1.0f64; 3 * 24 * 400];
    let long = input(b"/P39/LONG/DEPTH-TEMP//1Hour/T/", t(1, 1), &DEPTHS, &v);
    assert!(validate_profile_input(&long).is_ok());
}

/// The writer's depth limit is the reader's.
#[test]
fn depth_limit_is_the_readers() {
    use dss_core::dss7::ts::MAX_PROFILE_DEPTHS;
    let d = tmp("depths");
    let p = d.join("a.dss");
    create_dss7(&p).unwrap();
    let path: &[u8] = b"/P39/D/DEPTH-TEMP//1Year/T/";
    let over = vec![0f32; MAX_PROFILE_DEPTHS + 1];
    let bytes = std::fs::read(&p).unwrap();
    let e = write_profile(
        &p,
        &ProfileInput::new(
            path,
            t(10, 0),
            ProfileArrays::F32 {
                depths: &over,
                values: &over,
            },
            b"m",
            b"C",
            b"INST-VAL",
        ),
        &opts(),
    )
    .unwrap_err();
    assert!(matches!(e, WriteError::BadInput(_)), "{e}");
    assert_eq!(bytes, std::fs::read(&p).unwrap());
    let most = vec![1f32; MAX_PROFILE_DEPTHS];
    let r = write_profile(
        &p,
        &ProfileInput::new(
            path,
            t(10, 0),
            ProfileArrays::F32 {
                depths: &most,
                values: &most,
            },
            b"m",
            b"C",
            b"INST-VAL",
        ),
        &opts(),
    )
    .unwrap();
    let mut f = Dss7File::open(&p).unwrap();
    let rec = f.lookup(&r.blocks[0].pathname).unwrap().unwrap();
    assert_eq!(
        f.read_ts_profile_record(&rec).unwrap().depths.len(),
        MAX_PROFILE_DEPTHS
    );
}

/// Set `KINUHE` of a DSS6 block to an invalid length (dss6.md §5.1).
fn corrupt_user_header_length(p: &Path, block: &[u8]) {
    let f = Dss6File::open(p).unwrap();
    let walk = f.walk_bins(&Default::default()).unwrap();
    let e = walk
        .entries
        .iter()
        .find(|e| e.pathname == block)
        .expect("block");
    let nbw = (e.pathname.len() - 1) / 4 + 1;
    let word = i64::from(e.info_address) + (3 + nbw + 22) as i64;
    let a = (word - 1) as usize;
    let at = (a / 127) * 512 + (a % 127) * 4;
    drop(f);
    let mut b = std::fs::read(p).unwrap();
    b[at..at + 4].copy_from_slice(&100_001i32.to_le_bytes());
    std::fs::write(p, b).unwrap();
}

/// A retained user header is checked during preparation, so a refusal on
/// a later block leaves the earlier blocks, and the whole file, as they were.
#[test]
fn later_block_user_header_refusal_leaves_file_identical() {
    let d = tmp("uh");
    let v: Vec<f32> = (0..8).map(|i| i as f32).collect();
    let path: &[u8] = b"/L/G/FLOW//1HOUR/T/";
    let base = d.join("base.dss");
    create_dss6(&base).unwrap();
    write_rts6(
        &base,
        &RtsInput::new(path, t(30, 22), RtsValues::F32(&v), b"CFS", b"INST-VAL"),
        &opts(),
    )
    .unwrap();
    let set = LocationInput {
        pathname: path,
        x: Some(1.0),
        supplemental: Some(b"old"),
        keep_existing: true,
        ..LocationInput::default()
    };
    write_location6(&base, &set, &opts()).unwrap();
    let update = LocationInput {
        pathname: path,
        z: Some(9.0),
        keep_existing: true,
        ..LocationInput::default()
    };
    let replace_text = LocationInput {
        pathname: path,
        z: Some(9.0),
        supplemental: Some(b"new"),
        keep_existing: true,
        ..LocationInput::default()
    };
    // write_location6 goes in sorted order, FEB then JAN; the time-series writer JAN then FEB.
    for (name, block, go) in [
        ("loc", &b"/L/G/FLOW/01JAN2020/1HOUR/T/"[..], 0),
        ("text", &b"/L/G/FLOW/01JAN2020/1HOUR/T/"[..], 1),
        ("ts", &b"/L/G/FLOW/01FEB2020/1HOUR/T/"[..], 2),
    ] {
        let p = d.join(format!("{name}.dss"));
        std::fs::copy(&base, &p).unwrap();
        corrupt_user_header_length(&p, block);
        let before = std::fs::read(&p).unwrap();
        let nv: Vec<f32> = (0..8).map(|i| 50.0 + i as f32).collect();
        let e = match go {
            0 => write_location6(&p, &update, &opts()).unwrap_err(),
            1 => write_location6(&p, &replace_text, &opts()).unwrap_err(),
            _ => write_rts6(
                &p,
                &RtsInput::new(path, t(30, 22), RtsValues::F32(&nv), b"CFS", b"INST-VAL"),
                &opts(),
            )
            .unwrap_err(),
        };
        assert!(matches!(e, WriteError::Corrupt(_)), "{name}: {e}");
        assert_eq!(before, std::fs::read(&p).unwrap(), "{name}: file changed");
    }
}
