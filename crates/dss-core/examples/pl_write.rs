//! P39 local oracle helper: store the profile and location specs of
//! `evidence/P39/scripts/pl_spec.py` with the native writers.
//!
//! usage: pl_write create <file> <6|7>
//!        pl_write store <file> <spec.tsv> <6|7> [<fault step> [<fault block>]]
//!        pl_write collide <file> <stored pathname> <prefix>
//!
//! Spec lines (tab separated, `~` absent; bits in hex):
//!   P float|double path start depths values quality du vu type tz prec
//!   T float|double path start values units type
//!   L path x y z cs cid hu hd vu vd tz(hex) sup(hex)      (keep_existing false, as heclib)
//!   U path x y z cs cid hu hd vu vd sup(hex)              (keep_existing true; ~ keeps)

use dss_core::dss6::write as w6;
use dss_core::dss7::write as w7;
use std::path::Path;
use w7::{
    FaultPoint, LocationInput, ProfileArrays, ProfileInput, RtsInput, RtsValues, WriteOptions,
    WriteStep,
};

fn step(s: &str) -> WriteStep {
    *WriteStep::ALL
        .iter()
        .find(|x| format!("{x:?}") == s)
        .expect("step name")
}

fn hex(s: &str) -> Vec<u64> {
    if s == "~" || s.is_empty() {
        return Vec::new();
    }
    s.split(',')
        .map(|x| u64::from_str_radix(x, 16).expect("hex"))
        .collect()
}

fn f4(v: &[u64]) -> Vec<f32> {
    v.iter().map(|&b| f32::from_bits(b as u32)).collect()
}

fn f8(v: &[u64]) -> Vec<f64> {
    v.iter().map(|&b| f64::from_bits(b)).collect()
}

fn bytes(s: &str) -> Option<Vec<u8>> {
    (s != "~").then(|| {
        (0..s.len() / 2)
            .map(|i| u8::from_str_radix(&s[2 * i..2 * i + 2], 16).expect("hex"))
            .collect()
    })
}

fn opt_f(s: &str) -> Option<f64> {
    (s != "~").then(|| f64::from_bits(u64::from_str_radix(s, 16).expect("hex")))
}

fn opt_i(s: &str) -> Option<i32> {
    (s != "~").then(|| s.parse().expect("int"))
}

fn store(file: &str, spec: &str, version: u8, opts: &WriteOptions) -> i32 {
    let text = std::fs::read_to_string(spec).expect("spec");
    let p = Path::new(file);
    let mut rc = 0;
    for line in text.lines() {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let c: Vec<&str> = line.split('\t').collect();
        let r = match c[0] {
            "P" => {
                let (d, v) = (hex(c[4]), hex(c[5]));
                let (fd, fv, dd, dv) = (f4(&d), f4(&v), f8(&d), f8(&v));
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
                    (c[6] != "~").then(|| c[6].split(',').map(|x| x.parse().expect("q")).collect());
                let start: i64 = c[3].parse().expect("start");
                let mut i = ProfileInput::new(
                    c[2].as_bytes(),
                    start,
                    data,
                    c[7].as_bytes(),
                    c[8].as_bytes(),
                    c[9].as_bytes(),
                );
                i.quality = q.as_deref();
                i.time_zone = (c[10] != "~").then(|| c[10].as_bytes());
                i.precision = c[11].parse().expect("prec");
                w7::write_profile(p, &i, opts)
            }
            "T" => {
                let v = hex(c[4]);
                let (fv, dv) = (f4(&v), f8(&v));
                let vals = if c[1] == "float" {
                    RtsValues::F32(&fv)
                } else {
                    RtsValues::F64(&dv)
                };
                let start: i64 = c[3].parse().expect("start");
                let i = RtsInput::new(
                    c[2].as_bytes(),
                    start,
                    vals,
                    c[5].as_bytes(),
                    c[6].as_bytes(),
                );
                if version == 6 {
                    w6::write_rts6(p, &i, opts)
                } else {
                    w7::write_rts(p, &i, opts)
                }
            }
            "L" | "U" => {
                let keep = c[0] == "U";
                let tz = if keep { None } else { bytes(c[11]) };
                let sup = bytes(c[if keep { 11 } else { 12 }]);
                let i = LocationInput {
                    pathname: c[1].as_bytes(),
                    x: opt_f(c[2]),
                    y: opt_f(c[3]),
                    z: opt_f(c[4]),
                    coordinate_system: opt_i(c[5]),
                    coordinate_id: opt_i(c[6]),
                    horizontal_units: opt_i(c[7]),
                    horizontal_datum: opt_i(c[8]),
                    vertical_units: opt_i(c[9]),
                    vertical_datum: opt_i(c[10]),
                    time_zone: tz.as_deref(),
                    supplemental: sup.as_deref(),
                    keep_existing: keep,
                };
                if version == 6 {
                    w6::write_location6(p, &i, opts)
                } else {
                    w7::write_location(p, &i, opts)
                }
            }
            k => panic!("spec kind {k}"),
        };
        match r {
            Ok(rep) => {
                let o: Vec<&str> = rep.blocks.iter().map(|b| b.outcome.as_str()).collect();
                println!(
                    "{} {} {}",
                    c[0],
                    c[if c[0] == "L" || c[0] == "U" { 1 } else { 2 }],
                    o.join(",")
                );
            }
            Err(e) => {
                println!("ERR {} {e}", c[0]);
                rc = 1;
            }
        }
    }
    rc
}

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let v: u8 = if a[1] == "collide" {
        7
    } else {
        a[3 + usize::from(a[1] == "store")]
            .parse()
            .expect("version")
    };
    let code = match a[1].as_str() {
        "create" => {
            if v == 6 {
                w6::create_dss6(Path::new(&a[2])).expect("create");
            } else {
                w7::create_dss7(Path::new(&a[2])).expect("create");
            }
            0
        }
        "collide" => {
            // A pathname with the same table hash as a stored one: its entry goes in an
            // existing bin. usage: pl_write collide <file> <stored pathname> <prefix>
            let f = dss_core::dss7::Dss7File::open(&a[2]).expect("open");
            let max = f.header().words[15];
            let want = dss_core::dss7::table_hash(a[3].as_bytes(), max);
            let mut i = 0u64;
            loop {
                let p = format!("{}C{i}/FLOW//1Hour/N/", a[4]);
                let block = format!("{}C{i}/FLOW/01Jan2020/1Hour/N/", a[4]);
                if dss_core::dss7::table_hash(block.as_bytes(), max) == want {
                    println!("{p}");
                    break;
                }
                i += 1;
            }
            0
        }
        "store" => {
            let opts = WriteOptions {
                sync: false,
                fault: a.get(5).map(|s| FaultPoint {
                    block: a.get(6).map_or(0, |b| b.parse().expect("block")),
                    step: step(s),
                }),
                ..WriteOptions::default()
            };
            store(&a[2], &a[3], v, &opts)
        }
        o => panic!("command {o}"),
    };
    std::process::exit(code);
}
