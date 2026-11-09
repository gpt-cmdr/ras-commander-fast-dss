//! A record must never be reported under another record's variable.

use dss_vortex::error::VortexError;
use dss_vortex::source::grib::pds::read_product;
use dss_vortex::source::grib::{Grib2File, Limits};
use std::fs;
use std::path::Path;

fn fixture() -> Vec<u8> {
    fs::read(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/public/grib/simple.grib2"))
        .unwrap()
}

fn section(bytes: &[u8], number: u8) -> usize {
    let mut at = 16;
    loop {
        let length = u32::from_be_bytes(bytes[at..at + 4].try_into().unwrap()) as usize;
        if bytes[at + 4] == number {
            return at;
        }
        at += length;
    }
}

/// The fixture with another generating process type and forecast hour.
fn with_process(process: u8, hour: u32) -> Vec<u8> {
    let mut bytes = fixture();
    let product = section(&bytes, 4);
    bytes[product + 11] = process;
    bytes[product + 17] = 1; // hours
    bytes[product + 18..product + 22].copy_from_slice(&hour.to_be_bytes());
    bytes
}

fn open(parts: &[Vec<u8>]) -> Grib2File {
    Grib2File::from_bytes("x.grib2", parts.concat(), Limits::default()).unwrap()
}

fn names(file: &Grib2File) -> Vec<(String, usize)> {
    file.variables()
        .into_iter()
        .map(|v| (v.name, v.steps.len()))
        .collect()
}

/// Vortex 1.1.0 and 0.13.3 expose these as separate variables.
#[test]
fn an_error_process_is_its_own_variable() {
    let file = open(&[with_process(2, 1), with_process(6, 2)]);
    assert_eq!(
        names(&file),
        [
            ("Temperature_error_surface".to_string(), 1),
            ("Temperature_surface".to_string(), 1)
        ]
    );
    let frame = file.frame("Temperature_error_surface", 0).unwrap();
    assert!(frame
        .parts()
        .description
        .as_deref()
        .unwrap()
        .contains(" error @ "));
}

#[test]
fn process_types_6_and_7_under_one_name_are_refused() {
    let file = open(&[with_process(6, 1), with_process(7, 2)]);
    assert!(names(&file).is_empty());
    assert_eq!(file.unlisted().len(), 2);
    assert!(file.frame("Temperature_error_surface", 0).is_err());
}

#[test]
fn mixed_ordinary_process_types_are_refused() {
    let file = open(&[with_process(0, 1), with_process(2, 2)]);
    assert!(names(&file).is_empty());
    assert_eq!(file.unlisted().len(), 2);
}

#[test]
fn several_grids_are_refused() {
    // A later hour, so only the grid distinguishes them from one variable's two steps.
    let mut other = with_process(0, 2);
    let grid = section(&other, 3);
    other[grid + 46] ^= 1; // first latitude
    let file = open(&[with_process(0, 1), other]);
    assert!(names(&file).is_empty());
    assert_eq!(file.unlisted().len(), 2);
    assert!(file.decode_record(1).is_ok());
}

#[test]
fn the_same_process_over_time_stays_one_variable() {
    let file = open(&[with_process(2, 1), with_process(2, 2)]);
    assert_eq!(names(&file), [("Temperature_surface".to_string(), 2)]);
}

/// A probability or climatological record would otherwise read as the parameter itself.
#[test]
fn a_process_type_with_no_sampled_product_is_refused() {
    for process in (0..=255).filter(|p| ![0, 2, 6, 7, 8].contains(p)) {
        let file = open(&[with_process(process, 1)]);
        assert!(names(&file).is_empty(), "process {process}");
        assert_eq!(file.unlisted().len(), 1, "process {process}");
        assert!(file.frame("Temperature_surface", 0).is_err());
        assert!(file.decode_record(0).is_ok());
    }
    for process in [0, 2, 8] {
        let file = open(&[with_process(process, 1)]);
        assert_eq!(names(&file), [("Temperature_surface".to_string(), 1)]);
    }
}

/// Template 4.8 with two time ranges of the given statistics, an hour each, ending in 2024.
fn statistics(first: u8, second: u8) -> Vec<u8> {
    let mut section = vec![0u8; 70];
    section[7..9].copy_from_slice(&8u16.to_be_bytes());
    section[17] = 1; // hours
    section[34..41].copy_from_slice(&[0x07, 0xe8, 6, 1, 2, 0, 0]);
    section[41] = 2;
    for (base, statistic) in [(46, first), (58, second)] {
        section[base] = statistic;
        section[base + 2] = 1;
        section[base + 3..base + 7].copy_from_slice(&1u32.to_be_bytes());
        section[base + 7] = 255;
    }
    section
}

#[test]
fn time_ranges_with_different_statistics_are_refused() {
    let mut identification = vec![0u8; 21];
    identification[12..19].copy_from_slice(&[0x07, 0xe8, 6, 1, 0, 0, 0]);
    let same = read_product(0, &identification, &statistics(1, 1)).unwrap();
    assert_eq!(same.statistic, Some(1));
    assert!(matches!(
        read_product(0, &identification, &statistics(1, 0)),
        Err(VortexError::Refused { .. })
    ));
}
