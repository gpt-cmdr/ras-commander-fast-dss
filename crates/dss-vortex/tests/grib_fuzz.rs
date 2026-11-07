//! Hostile input: every path returns a typed result, never a panic or an unbounded allocation.
//! Deterministic, so a failure reproduces. Third-party decoders are guarded inside the reader;
//! their panic message may print, but the result is still an error.

use dss_vortex::source::grib::{Grib2File, Limits};
use std::fs;
use std::path::Path;

const LIMITS: Limits = Limits {
    max_cells: 1 << 20,
    max_records: 64,
    max_input_bytes: 1 << 24,
    max_jpeg2000_bytes: 1 << 26,
};

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }
}

fn exercise(bytes: Vec<u8>) {
    let Ok(file) = Grib2File::from_bytes("hrrr.t00z.wrfsfcf01.grib2", bytes, LIMITS) else {
        return;
    };
    for i in 0..file.record_count() {
        if let Ok(cells) = file.decode_record(i) {
            assert!(cells.len() <= LIMITS.max_cells);
        }
        let _ = file.georeference(i);
        let _ = file.product(i);
    }
    for variable in file.variables() {
        for step in 0..variable.steps.len() {
            let _ = file.frame(&variable.name, step);
        }
    }
    let _ = file.unlisted();
}

fn fixtures() -> Vec<Vec<u8>> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/public/grib");
    let mut out = Vec::new();
    for entry in fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().is_some_and(|e| e == "grib2") {
            out.push(fs::read(path).unwrap());
        }
    }
    assert!(out.len() >= 13);
    out
}

/// Section starts, so mutations also land on length and count fields.
fn sections(bytes: &[u8]) -> Vec<usize> {
    let mut starts = vec![0];
    let mut at = 16;
    while let Some(head) = bytes.get(at..at + 5) {
        if head[..4] == *b"7777" {
            break;
        }
        let length = u32::from_be_bytes([head[0], head[1], head[2], head[3]]) as usize;
        starts.push(at);
        if length == 0 {
            break;
        }
        at += length;
    }
    starts
}

#[test]
fn random_mutations_never_panic() {
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    for original in fixtures() {
        let starts = sections(&original);
        for _ in 0..300 {
            let mut bytes = original.clone();
            match rng.below(5) {
                0 => {
                    let at = rng.below(bytes.len());
                    bytes[at] = rng.next() as u8;
                }
                1 => {
                    for _ in 0..1 + rng.below(16) {
                        let at = rng.below(bytes.len());
                        bytes[at] ^= 1 << rng.below(8);
                    }
                }
                2 => bytes.truncate(rng.below(bytes.len())),
                3 => {
                    // A field near a section start: lengths, templates, counts.
                    let base = starts[rng.below(starts.len())];
                    let at = (base + rng.below(72)).min(bytes.len() - 1);
                    bytes[at] = [0x00, 0x7f, 0x80, 0xff][rng.below(4)];
                }
                _ => {
                    // A whole 32-bit field set to an extreme.
                    let base = starts[rng.below(starts.len())];
                    let at = base + rng.below(72);
                    let value = [0u32, 1, 0x7fff_ffff, 0x8000_0000, 0xffff_ffff][rng.below(5)];
                    if let Some(field) = bytes.get_mut(at..at + 4) {
                        field.copy_from_slice(&value.to_be_bytes());
                    }
                }
            }
            exercise(bytes);
        }
    }
}

#[test]
fn random_bytes_and_empty_files_never_panic() {
    let mut rng = Rng(45);
    exercise(Vec::new());
    exercise(b"GRIB".to_vec());
    exercise(b"GRIB\0\0\0\x02".to_vec());
    for _ in 0..500 {
        let mut bytes = b"GRIB\0\0\0\x02".to_vec();
        for _ in 0..rng.below(300) {
            bytes.push(rng.next() as u8);
        }
        exercise(bytes);
    }
}

/// A 200-byte file declaring two billion cells is refused before anything is allocated.
#[test]
fn a_huge_declared_grid_is_refused() {
    for original in fixtures() {
        let starts = sections(&original);
        let section3 = starts[2];
        let mut bytes = original.clone();
        // Octets 7-10 points; 31-34 Ni; 35-38 Nj.
        bytes[section3 + 6..section3 + 10].copy_from_slice(&0x8000_0000u32.to_be_bytes());
        bytes[section3 + 30..section3 + 34].copy_from_slice(&0x0000_8000u32.to_be_bytes());
        bytes[section3 + 34..section3 + 38].copy_from_slice(&0x0001_0000u32.to_be_bytes());
        let file = Grib2File::from_bytes("x.grib2", bytes, LIMITS).unwrap();
        assert!(file.decode_record(0).is_err());
        assert!(file.georeference(0).is_err());
    }
}

#[test]
fn too_many_records_are_refused() {
    let one = fixtures().remove(0);
    let many: Vec<u8> = one.iter().cycle().take(one.len() * 65).copied().collect();
    assert!(Grib2File::from_bytes("x.grib2", many, LIMITS).is_err());
}
