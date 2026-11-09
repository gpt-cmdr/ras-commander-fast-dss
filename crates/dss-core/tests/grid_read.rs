//! Public/synthetic grid coverage. No client data or client-derived constants.
//! Layouts follow heclib zStructSpatialGrid.h/zspatialGridStore and Java heclib
//! GridInfoFlat.flatten, projection-specific Flat classes and DataCompression.
mod dss7_common;
#[path = "dss6_ts_support/mod.rs"]
mod v6;

use dss_core::{dss6::Dss6File, dss7::Dss7File, GridError, GridReadOptions, GRID_UNDEFINED};
const PATH: &str = "/PUBLIC/GRID/PRECIP/01JAN2020:2300/01JAN2020:2400/TEST/";

fn bits(v: f32) -> i32 {
    v.to_bits() as i32
}
fn raw(words: &[i32]) -> Vec<u8> {
    words.iter().flat_map(|v| v.to_le_bytes()).collect()
}
fn pack(bytes: &[u8]) -> Vec<i32> {
    let mut b = bytes.to_vec();
    b.resize(b.len().div_ceil(4) * 4, 0);
    b.chunks_exact(4)
        .map(|v| i32::from_le_bytes([v[0], v[1], v[2], v[3]]))
        .collect()
}
fn legacy(rt: i32, method: i32, nbytes: usize) -> Vec<i32> {
    let len = match rt {
        410 | 411 => 66,
        420 | 421 => 75,
        430 | 431 => 83,
        _ => 63,
    };
    let mut h = vec![0; len];
    h[0] = (len * 4) as i32;
    h[1] = rt;
    h[2] = 124;
    h[3] = 124;
    h[4] = 63_115_140;
    h[5] = 63_115_200;
    h[6] = i32::from_le_bytes(*b"MM  ");
    h[9] = 1;
    h[10] = -2;
    h[11] = 3;
    h[12] = 3;
    h[13] = 1;
    h[14] = bits(2.0);
    h[15] = method;
    h[16] = nbytes as i32;
    h[17] = bits(100.0);
    h[19] = bits(1.25);
    h[20] = bits(0.0);
    h[21] = bits(0.5);
    h[22] = 2;
    h[23] = bits(0.0);
    h[24] = bits(1.0);
    h[43] = 2;
    h[44] = 1;
    if matches!(rt, 420 | 421) {
        h[63] = 2;
        h[64] = i32::from_le_bytes(*b"Mete");
        h[65] = i32::from_le_bytes(*b"rs  ");
        h[67] = bits(29.5);
        h[68] = bits(45.5);
        h[69] = bits(-96.0);
        h[70] = bits(23.0);
    }
    if matches!(rt, 430 | 431) {
        let tail = [
            2,
            1,
            i32::from_le_bytes(*b"WKT "),
            0,
            2,
            i32::from_le_bytes(*b"GEOG"),
            i32::from_le_bytes(*b"CS[]"),
            bits(10.0),
            bits(20.0),
            bits(-9999.0),
            1,
            i32::from_le_bytes(*b"UTC "),
            0,
            1,
            1,
        ];
        h.truncate(63);
        h.extend(tail);
        h[0] = (h.len() * 4) as i32;
    }
    h
}
fn file6(rt: i32, h: Vec<i32>, data: Vec<u8>) -> Vec<u8> {
    let mut f = v6::ts_file(&[v6::Rec::live(PATH, rt)], *b"6-YN");
    f.attach(
        PATH,
        &v6::Payload {
            internal_header: h,
            data: pack(&data),
            ..Default::default()
        },
    );
    f.finish()
}
fn file7(rt: i32, h: &[i32], data: &[u8], modern: bool) -> Vec<u8> {
    use dss7_common::{build, set_word, Rec, Spec};
    let b = build(&Spec {
        recs: vec![Rec::live(PATH, rt)],
        ..Default::default()
    });
    let mut bytes = b.bytes;
    let info = b.info_addr[0];
    let mut next = bytes.len().div_ceil(8) as i64;
    let mut attach = |key: i64, area: &[u8]| {
        let addr = next;
        next += area.len().div_ceil(8) as i64;
        bytes.resize((next * 8) as usize, 0);
        bytes[(addr * 8) as usize..(addr * 8) as usize + area.len()].copy_from_slice(area);
        set_word(&mut bytes, (info + key) as usize, addr);
        set_word(
            &mut bytes,
            (info + key + 1) as usize,
            area.len().div_ceil(4) as i64,
        );
    };
    attach(13, &raw(h));
    if modern {
        attach(15, b"MM\0PUBLIC\0WKT\0GEOGCS[]\0UTC\0");
        attach(
            17,
            &raw(&[bits(2.0), bits(10.0), bits(20.0), bits(-9999.0)]),
        );
        attach(
            19,
            &raw(&[bits(0.0), bits(1.25), bits(0.5), bits(0.0), bits(1.0), 2, 1]),
        );
        attach(23, data);
    } else {
        attach(19, data);
    }
    bytes.resize((next as usize + 1) * 8, 0);
    set_word(&mut bytes, 5, next);
    set_word(&mut bytes, next as usize, -97532);
    bytes
}
fn modern(method: i32, nbytes: usize) -> Vec<i32> {
    let mut h = vec![0; 24];
    h[0] = -100;
    h[1] = 430;
    h[2] = 2;
    h[3] = 1;
    h[4] = -2;
    h[5] = 3;
    h[6] = 3;
    h[7] = 1;
    h[8] = method;
    h[9] = nbytes as i32;
    h[10] = 2;
    h[13] = 1;
    h[14] = 1;
    h
}

#[test]
fn precip_tokens_double_precision_scaling_and_missing() {
    // Tokens [125, zero-run 1, missing-run 1]; high half-word first, plus padding.
    let data = raw(&[((125u32 << 16) | 0x8001) as i32, ((0xc001u32) << 16) as i32]);
    for rt in [400, 401, 410, 411, 420, 421, 430, 431] {
        let h = legacy(rt, 101001, 6);
        for version in [6, 7] {
            let g = if version == 6 {
                Dss6File::from_bytes(&file6(rt, h.clone(), data.clone()))
                    .unwrap()
                    .read_grid(PATH.as_bytes())
                    .unwrap()
            } else {
                Dss7File::from_bytes(&file7(rt, &h, &data, false))
                    .unwrap()
                    .read_grid(PATH.as_bytes())
                    .unwrap()
            };
            assert_eq!(g.data, [1.25, 0.0, GRID_UNDEFINED]);
            assert_eq!(g.range_limit_table, [0.0, 1.0]);
            assert_eq!(g.number_equal_or_exceeding_range_limit, [2, 1]);
            assert!(g.is_missing(g.data[2]));
            if rt == 430 {
                assert_eq!(g.srs_definition, "GEOGCS[]");
                assert_eq!(g.srs_name, "WKT");
                assert_eq!(g.time_zone_id, "UTC");
                assert_eq!(g.extent(), [6.0, 26.0, 12.0, 28.0]);
            }
        }
    }
    // Values that distinguish f64 divide/add from f32 divide/add.
    let mut h = legacy(410, 101001, 6);
    h[17] = bits(3.0);
    h[18] = bits(0.1);
    let data = raw(&[((32767u32 << 16) | 7) as i32, (1u32 << 16) as i32]);
    let g = Dss6File::from_bytes(&file6(410, h, data))
        .unwrap()
        .read_grid(PATH.as_bytes())
        .unwrap();
    assert_eq!(
        g.data.iter().map(|v| v.to_bits()).collect::<Vec<_>>(),
        [32767, 7, 1]
            .map(|v| (f64::from(v) / 3.0 + f64::from(0.1f32)) as f32)
            .map(f32::to_bits)
    );
}

#[test]
fn zlib_keeps_nan_payloads_and_handles_modern_odd_padding() {
    let values = [bits(1.25), 0x7fc12345, bits(-9999.0)];
    let compressed = miniz_oxide::deflate::compress_to_vec_zlib(&raw(&values), 6);
    let mut h = legacy(430, 26, compressed.len());
    // SpecifiedGridInfoFlat can retain its constructor's 296-byte size after
    // variable strings grow. The actual transfer length remains authoritative.
    h[0] = 296;
    let raw_opts = GridReadOptions {
        preserve_legacy_missing: true,
        ..Default::default()
    };
    for version in [6, 7] {
        let g = if version == 6 {
            Dss6File::from_bytes(&file6(430, h.clone(), compressed.clone()))
                .unwrap()
                .read_grid_with(PATH.as_bytes(), &raw_opts)
                .unwrap()
        } else {
            Dss7File::from_bytes(&file7(430, &h, &compressed, false))
                .unwrap()
                .read_grid_with(PATH.as_bytes(), &raw_opts)
                .unwrap()
        };
        assert_eq!(
            g.data
                .iter()
                .map(|v| v.to_bits() as i32)
                .collect::<Vec<_>>(),
            values
        );
        assert!(g.is_missing(g.data[1]));
        assert!(g.is_missing(g.data[2]));
    }
    let g = Dss6File::from_bytes(&file6(430, h.clone(), compressed.clone()))
        .unwrap()
        .read_grid(PATH.as_bytes())
        .unwrap();
    assert_eq!(g.data[2], GRID_UNDEFINED);
    assert_eq!(g.data[1].to_bits(), values[1] as u32);
    let mut padded = values.to_vec();
    padded.push(0);
    let z = miniz_oxide::deflate::compress_to_vec_zlib(&raw(&padded), 6);
    let bytes = file7(430, &modern(26, z.len()), &z, true);
    let g = Dss7File::from_bytes(&bytes)
        .unwrap()
        .read_grid(PATH.as_bytes())
        .unwrap();
    assert_eq!(
        g.data
            .iter()
            .map(|v| v.to_bits() as i32)
            .collect::<Vec<_>>(),
        values
    );
    assert_eq!(g.end_time.unwrap() - g.start_time.unwrap(), 3600);
    assert_eq!(g.srs_definition, "GEOGCS[]");
}

#[test]
fn raw_cells_and_typed_failures() {
    let values = raw(&[bits(1.25), 0, bits(-9999.0)]);
    let bytes = file7(430, &modern(1, 0), &values, true);
    let mut f = Dss7File::from_bytes(&bytes).unwrap();
    assert_eq!(
        f.read_grid(PATH.as_bytes()).unwrap().data,
        [1.25, 0.0, -9999.0]
    );
    let err = f
        .read_grid_with(
            PATH.as_bytes(),
            &GridReadOptions {
                max_cells: 2,
                ..Default::default()
            },
        )
        .unwrap_err();
    assert!(matches!(
        err,
        GridError::LimitExceeded { field: "cells", .. }
    ));
    let bytes = file7(200, &[0; 24], &[], false);
    assert!(matches!(
        Dss7File::from_bytes(&bytes)
            .unwrap()
            .read_grid(PATH.as_bytes()),
        Err(GridError::UnsupportedRecordType { record_type: 200 })
    ));
    let h = modern(99, 0);
    let bytes = file7(430, &h, &values, true);
    assert!(matches!(
        Dss7File::from_bytes(&bytes)
            .unwrap()
            .read_grid(PATH.as_bytes()),
        Err(GridError::UnsupportedCompression { method: 99 })
    ));
}

#[test]
fn corrupt_streams_runs_truncations_and_bombs_are_errors() {
    let bomb = miniz_oxide::deflate::compress_to_vec_zlib(&vec![0; 1_000_000], 6);
    let bytes = file7(430, &modern(26, bomb.len()), &bomb, true);
    assert!(Dss7File::from_bytes(&bytes)
        .unwrap()
        .read_grid(PATH.as_bytes())
        .is_err());
    for token in [0x8000, 0x8004, 0xc004] {
        let data = raw(&[(token as u32).wrapping_shl(16) as i32]);
        let bytes = file6(410, legacy(410, 101001, 2), data);
        assert!(Dss6File::from_bytes(&bytes)
            .unwrap()
            .read_grid(PATH.as_bytes())
            .is_err());
    }
    let z = miniz_oxide::deflate::compress_to_vec_zlib(&raw(&[0; 4]), 6);
    for end in 0..z.len() {
        let bytes = file7(430, &modern(26, end), &z[..end], true);
        assert!(Dss7File::from_bytes(&bytes)
            .unwrap()
            .read_grid(PATH.as_bytes())
            .is_err());
    }
    let mut corrupt = z.clone();
    let last = corrupt.len() - 1;
    corrupt[last] ^= 1;
    let bytes = file7(430, &modern(26, corrupt.len()), &corrupt, true);
    assert!(Dss7File::from_bytes(&bytes)
        .unwrap()
        .read_grid(PATH.as_bytes())
        .is_err());
}

#[test]
fn mutated_grid_headers_and_payloads_never_panic() {
    let z = miniz_oxide::deflate::compress_to_vec_zlib(&raw(&[0; 4]), 6);
    let samples = [
        file7(430, &modern(26, z.len()), &z, true),
        file7(420, &legacy(420, 26, z.len()), &z, false),
        file6(430, legacy(430, 26, z.len()), z.clone()),
    ];
    let iters = std::env::var("DSS_GRID_FUZZ_ITERS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(10_000);
    let mut seed = 0x123456789abcdefu64;
    for (which, original) in samples.iter().enumerate() {
        for i in 0..iters {
            let mut bytes = original.clone();
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            // Mutate full file, concentrating half the cases in the payload tail.
            let start = if i % 2 == 0 {
                bytes.len().saturating_sub(1500)
            } else {
                0
            };
            let pos = start + seed as usize % (bytes.len() - start);
            for j in 0..4 {
                if let Some(b) = bytes.get_mut(pos + j) {
                    *b ^= (seed >> (j * 8)) as u8;
                }
            }
            if i % 7 == 0 {
                bytes.truncate(seed as usize % bytes.len());
            }
            if which == 2 {
                if let Ok(f) = Dss6File::from_bytes(&bytes) {
                    let _ = f.read_grid(PATH.as_bytes());
                }
            } else if let Ok(mut f) = Dss7File::from_bytes(&bytes) {
                let _ = f.read_grid(PATH.as_bytes());
            }
        }
    }
}

// ---------------------------------------------------------------------------------
// Public synthetic fixtures for the Python binding:
// fixtures/public/synthetic_grid{6,7}.dss. To regenerate, set DSS_PY_FIXTURE_OUT to a
// scratch directory, run this test, and copy both files into fixtures/public/. When
// the committed files exist the test checks that they still match this generator.
// ---------------------------------------------------------------------------------

const FX_SPEC: &str = "/PUBLIC/SPECIFIED/PRECIP/01JAN2020:0000/01JAN2020:0100/SYNTH/";
const FX_ALBERS: &str = "/PUBLIC/ALBERS/PRECIP/01JAN2020:0000/01JAN2020:0100/SYNTH/";
const FX_HRAP: &str = "/PUBLIC/HRAP/PRECIP/01JAN2020:0000/01JAN2020:0100/SYNTH/";
const FX_UNTIMED: &str = "/PUBLIC/UNTIMED/ELEVATION///SYNTH/";
const FX_MODERN: &str = "/PUBLIC/MODERN/PRECIP/01JAN2020:0000/01JAN2020:0100/SYNTH/";
/// HecTime minutes of 01JAN2020 00:00 (minutes since 31DEC1899 00:00; 01JAN2020 is
/// HEC julian day 43830).
const FX_START_MINUTES: i32 = 43_830 * 1440;

/// One grid record: pathname, type, internal header, payload, and (modern -100 only)
/// header2 strings, user-header geometry and values1 statistics.
struct FxGrid {
    path: &'static str,
    rt: i32,
    header: Vec<i32>,
    data: Vec<u8>,
    modern: Option<(Vec<u8>, Vec<u8>, Vec<u8>)>,
}

fn fx_legacy(rt: i32, method: i32, data: &[u8], nx: i32, ny: i32, stats: [f32; 3]) -> Vec<i32> {
    let mut h = legacy(rt, method, data.len());
    h[4] = FX_START_MINUTES;
    h[5] = FX_START_MINUTES + 60;
    h[12] = nx;
    h[13] = ny;
    h[19] = bits(stats[0]); // max
    h[20] = bits(stats[1]); // min
    h[21] = bits(stats[2]); // mean
    h
}

fn fx_records(version: u8) -> Vec<FxGrid> {
    // Specified, legacy ZLIB, 3 x 2. Cell 4 holds the -9999 null marker.
    let spec = raw(&[1.0f32, 2.0, 3.0, 4.0, -9999.0, 6.0].map(bits));
    let spec = miniz_oxide::deflate::compress_to_vec_zlib(&spec, 6);
    let mut h_spec = fx_legacy(430, 26, &spec, 3, 2, [6.0, 1.0, 3.2]);
    h_spec[23] = bits(1.0);
    h_spec[24] = bits(4.0);
    h_spec[43] = 5;
    h_spec[44] = 2;
    // Albers, legacy ZLIB, 2 x 2. 99.0 lies outside the stored min/max, so Java
    // heclib normalizes it to UNDEFINED.
    let alb = raw(&[0.5f32, 1.0, 99.0, 2.0].map(bits));
    let alb = miniz_oxide::deflate::compress_to_vec_zlib(&alb, 6);
    let mut h_alb = fx_legacy(420, 26, &alb, 2, 2, [2.0, 0.5, 1.25]);
    h_alb[73] = bits(1000.0);
    h_alb[74] = bits(2000.0);
    // HRAP, PRECIP_2_BYTE, 3 x 1: tokens [125, zero run 1, missing run 1].
    let hrap = raw(&[((125u32 << 16) | 0x8001) as i32, ((0xc001u32) << 16) as i32]);
    let mut h_hrap = legacy(410, 101001, 6);
    h_hrap[4] = FX_START_MINUTES;
    h_hrap[5] = FX_START_MINUTES + 60;
    // Specified without time (431), legacy ZLIB, 2 x 1; time-stamped flag cleared.
    // (Monolith's legacy decoder rejects compression method 1, so ZLIB is used.)
    let untimed = raw(&[5.0f32, 6.0].map(bits));
    let untimed = miniz_oxide::deflate::compress_to_vec_zlib(&untimed, 6);
    let mut h_untimed = fx_legacy(431, 26, &untimed, 2, 1, [6.0, 5.0, 5.5]);
    let last = h_untimed.len() - 1;
    h_untimed[last] = 0;
    let mut recs = vec![
        FxGrid {
            path: FX_SPEC,
            rt: 430,
            header: h_spec,
            data: spec,
            modern: None,
        },
        FxGrid {
            path: FX_ALBERS,
            rt: 420,
            header: h_alb,
            data: alb,
            modern: None,
        },
        FxGrid {
            path: FX_HRAP,
            rt: 410,
            header: h_hrap,
            data: hrap,
            modern: None,
        },
        FxGrid {
            path: FX_UNTIMED,
            rt: 431,
            header: h_untimed,
            data: untimed,
            modern: None,
        },
    ];
    if version == 7 {
        // Modern -100, ZLIB, 3 x 3 (odd cell count: one alignment float follows the
        // cells, zspatialGridStore.c). Cell 4 is the null marker, cell 7 a NaN payload.
        let mut cells: Vec<i32> = [0.0f32, 1.0, 2.0, 3.0, -9999.0, 5.0, 6.0, 0.0, 8.0]
            .map(bits)
            .to_vec();
        cells[7] = 0x7fc1_2345;
        cells.push(0);
        let z = miniz_oxide::deflate::compress_to_vec_zlib(&raw(&cells), 6);
        let mut h = modern(26, z.len());
        h[6] = 3;
        h[7] = 3;
        recs.push(FxGrid {
            path: FX_MODERN,
            rt: 430,
            header: h,
            data: z,
            modern: Some((
                b"MM\0PUBLIC\0WKT\0GEOGCS[]\0UTC\0".to_vec(),
                raw(&[bits(2.0), bits(10.0), bits(20.0), bits(-9999.0)]),
                raw(&[bits(0.0), bits(8.0), bits(3.5), bits(0.0), bits(4.0), 7, 3]),
            )),
        });
    }
    recs
}

fn fx_file6(recs: &[FxGrid]) -> Vec<u8> {
    let cat: Vec<_> = recs.iter().map(|r| v6::Rec::live(r.path, r.rt)).collect();
    let mut f = v6::ts_file(&cat, *b"6-YN");
    for r in recs {
        f.attach(
            r.path,
            &v6::Payload {
                internal_header: r.header.clone(),
                data: pack(&r.data),
                ..Default::default()
            },
        );
    }
    f.finish()
}

fn fx_file7(recs: &[FxGrid]) -> Vec<u8> {
    use dss7_common::{build, set_word, Rec, Spec};
    let b = build(&Spec {
        recs: recs.iter().map(|r| Rec::live(r.path, r.rt)).collect(),
        ..Default::default()
    });
    let mut bytes = b.bytes;
    let mut next = bytes.len().div_ceil(8) as i64;
    for (r, &info) in recs.iter().zip(&b.info_addr) {
        // Same area layout as file7().
        let mut attach = |key: i64, area: &[u8]| {
            let addr = next;
            next += area.len().div_ceil(8) as i64;
            bytes.resize((next * 8) as usize, 0);
            bytes[(addr * 8) as usize..(addr * 8) as usize + area.len()].copy_from_slice(area);
            set_word(&mut bytes, (info + key) as usize, addr);
            set_word(
                &mut bytes,
                (info + key + 1) as usize,
                area.len().div_ceil(4) as i64,
            );
        };
        attach(13, &raw(&r.header));
        match &r.modern {
            Some((chars, geometry, stats)) => {
                attach(15, chars);
                attach(17, geometry);
                attach(19, stats);
                attach(23, &r.data);
            }
            None => attach(19, &r.data),
        }
    }
    bytes.resize((next as usize + 1) * 8, 0);
    set_word(&mut bytes, 5, next);
    set_word(&mut bytes, next as usize, -97532);
    bytes
}

#[test]
fn synthetic_grid_fixtures() {
    let b6 = fx_file6(&fx_records(6));
    let b7 = fx_file7(&fx_records(7));
    let raw_opts = GridReadOptions {
        preserve_legacy_missing: true,
        ..Default::default()
    };
    let d = GridReadOptions::default();
    for version in [6, 7] {
        let read = |p: &str, o: &GridReadOptions| {
            if version == 6 {
                Dss6File::from_bytes(&b6)
                    .unwrap()
                    .read_grid_with(p.as_bytes(), o)
                    .unwrap()
            } else {
                Dss7File::from_bytes(&b7)
                    .unwrap()
                    .read_grid_with(p.as_bytes(), o)
                    .unwrap()
            }
        };
        let g = read(FX_SPEC, &d);
        assert_eq!((g.nx, g.ny), (3, 2));
        assert_eq!(g.data, [1.0, 2.0, 3.0, 4.0, GRID_UNDEFINED, 6.0]);
        assert_eq!(read(FX_SPEC, &raw_opts).data[4], -9999.0);
        assert_eq!(g.start_time, Some(i64::from(FX_START_MINUTES) * 60));
        assert_eq!(g.end_time, Some(i64::from(FX_START_MINUTES + 60) * 60));
        let g = read(FX_ALBERS, &d);
        assert_eq!(g.data, [0.5, 1.0, GRID_UNDEFINED, 2.0]);
        assert_eq!(read(FX_ALBERS, &raw_opts).data[2], 99.0);
        assert_eq!(g.extent(), [996.0, 2006.0, 1000.0, 2010.0]);
        assert_eq!(read(FX_HRAP, &d).data, [1.25, 0.0, GRID_UNDEFINED]);
        let g = read(FX_UNTIMED, &d);
        assert_eq!(g.data, [5.0, 6.0]);
        assert_eq!((g.start_time, g.end_time), (None, None));
    }
    let g = Dss7File::from_bytes(&b7)
        .unwrap()
        .read_grid(FX_MODERN.as_bytes())
        .unwrap();
    assert_eq!((g.nx, g.ny, g.struct_version), (3, 3, -100));
    // The modern grid takes its times from the D/E parts; the legacy headers match.
    assert_eq!(g.start_time, Some(i64::from(FX_START_MINUTES) * 60));
    assert_eq!(g.end_time, Some(i64::from(FX_START_MINUTES + 60) * 60));
    assert_eq!(g.data[4], -9999.0);
    assert_eq!(g.data[7].to_bits(), 0x7fc1_2345);
    assert_eq!(g.range_limit_table, [0.0, 4.0]);
    assert_eq!(g.number_equal_or_exceeding_range_limit, [7, 3]);
    let public = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/public");
    for (name, bytes) in [("synthetic_grid6.dss", &b6), ("synthetic_grid7.dss", &b7)] {
        if let Ok(committed) = std::fs::read(public.join(name)) {
            assert!(
                committed == *bytes,
                "{name} differs from the generator; regenerate it"
            );
        }
    }
    if let Some(out) = std::env::var_os("DSS_PY_FIXTURE_OUT") {
        let out = std::path::PathBuf::from(out);
        std::fs::create_dir_all(&out).unwrap();
        std::fs::write(out.join("synthetic_grid6.dss"), &b6).unwrap();
        std::fs::write(out.join("synthetic_grid7.dss"), &b7).unwrap();
    }
}
