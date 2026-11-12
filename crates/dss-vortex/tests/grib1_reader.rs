//! P73: the GRIB1 reader on synthetic messages built here, byte by byte, in Stage IV's layout.
//! Parity on real Stage IV files against Vortex and ecCodes was checked during development; these
//! tests pin the arithmetic, the refusals and hostile input without it.

use dss_vortex::import::{open_source, OpenOptions};
use dss_vortex::products::WindowRule;
use dss_vortex::source::grib::Limits;
use dss_vortex::source::grib1::Grib1File;
use dss_vortex::{DataType, FrameTime, RowOrder, VortexError};

/// Knobs for one synthetic message.
#[derive(Clone)]
struct Spec {
    nx: u16,
    ny: u16,
    p1: u8,
    p2: u8,
    time_range: u8,
    time_unit: u8,
    parameter: u8,
    center: u8,
    level_type: u8,
    /// (yy, mm, dd, hh) with century 21.
    reference: (u8, u8, u8, u8),
    decimal_scale: i16,
    binary_scale: i16,
    /// IBM float bytes.
    reference_value: [u8; 4],
    bits: u8,
    /// Packed values for the present points, in order.
    packed: Vec<u32>,
    /// Present points; `None` for no BMS.
    bitmap: Option<Vec<bool>>,
    bitmap_table: u16,
    bds_flag: u8,
    resolution: u8,
    scan_mode: u8,
    representation: u8,
    /// Lo1 in thousandths of a degree, sign and magnitude as stored.
    lo1: i32,
    /// BMS octet 4, when not the true count of padding bits.
    bms_unused: Option<u8>,
    /// BDS octet 4 low nibble, when not the true count of padding bits.
    bds_unused: Option<u8>,
    /// Extra octets appended to the BDS payload, as given.
    bds_extra: Vec<u8>,
    /// Dx and Dy octets, as stored.
    dx: [u8; 3],
    dy: [u8; 3],
}

impl Default for Spec {
    fn default() -> Self {
        Spec {
            nx: 4,
            ny: 3,
            p1: 0,
            p2: 1,
            time_range: 4,
            time_unit: 1,
            parameter: 61,
            center: 7,
            level_type: 1,
            reference: (8, 1, 8, 11),
            decimal_scale: 1,
            binary_scale: -2,
            // 100.0 as IBM: 0x42 0x64 0x00 0x00.
            reference_value: [0x42, 0x64, 0, 0],
            bits: 7,
            packed: (0..12).map(|i| i * 9).collect(),
            bitmap: None,
            bitmap_table: 0,
            bds_flag: 0,
            resolution: 0x08,
            scan_mode: 64,
            representation: 5,
            lo1: -119023,
            bms_unused: None,
            bds_unused: None,
            bds_extra: Vec::new(),
            dx: [0x00, 0x12, 0x9b],
            dy: [0x00, 0x12, 0x9b],
        }
    }
}

fn u24(n: usize) -> [u8; 3] {
    [(n >> 16) as u8, (n >> 8) as u8, n as u8]
}

fn sm2(v: i16) -> [u8; 2] {
    let m = v.unsigned_abs();
    let mut b = m.to_be_bytes();
    if v < 0 {
        b[0] |= 0x80;
    }
    b
}

fn sm3(v: i32) -> [u8; 3] {
    let m = v.unsigned_abs();
    let mut b = [(m >> 16) as u8, (m >> 8) as u8, m as u8];
    if v < 0 {
        b[0] |= 0x80;
    }
    b
}

fn message(s: &Spec) -> Vec<u8> {
    let mut pds = vec![0u8; 28];
    pds[..3].copy_from_slice(&u24(28));
    pds[3] = 2;
    pds[4] = s.center;
    pds[5] = 182;
    pds[6] = 255;
    pds[7] = 0x80 | if s.bitmap.is_some() { 0x40 } else { 0 };
    pds[8] = s.parameter;
    pds[9] = s.level_type;
    let (yy, mm, dd, hh) = s.reference;
    pds[12] = yy;
    pds[13] = mm;
    pds[14] = dd;
    pds[15] = hh;
    pds[17] = s.time_unit;
    pds[18] = s.p1;
    pds[19] = s.p2;
    pds[20] = s.time_range;
    pds[24] = 21;
    pds[25] = 4;
    pds[26..28].copy_from_slice(&sm2(s.decimal_scale));

    let mut gds = vec![0u8; 32];
    gds[..3].copy_from_slice(&u24(32));
    gds[4] = 255;
    gds[5] = s.representation;
    gds[6..8].copy_from_slice(&s.nx.to_be_bytes());
    gds[8..10].copy_from_slice(&s.ny.to_be_bytes());
    gds[10..13].copy_from_slice(&sm3(23117));
    gds[13..16].copy_from_slice(&sm3(s.lo1));
    gds[16] = s.resolution;
    gds[17..20].copy_from_slice(&sm3(-105000));
    gds[20..23].copy_from_slice(&s.dx);
    gds[23..26].copy_from_slice(&s.dy);
    gds[26] = 0;
    gds[27] = s.scan_mode;

    let mut out = Vec::new();
    out.extend_from_slice(b"GRIB");
    out.extend_from_slice(&[0, 0, 0, 1]);
    out.extend_from_slice(&pds);
    out.extend_from_slice(&gds);
    if let Some(present) = &s.bitmap {
        let mut bits = vec![0u8; present.len().div_ceil(8)];
        for (i, &p) in present.iter().enumerate() {
            if p {
                bits[i / 8] |= 128 >> (i % 8);
            }
        }
        let len = 6 + bits.len();
        out.extend_from_slice(&u24(len));
        out.push(
            s.bms_unused
                .unwrap_or((bits.len() * 8 - present.len()) as u8),
        );
        out.extend_from_slice(&s.bitmap_table.to_be_bytes());
        out.extend_from_slice(&bits);
    }
    let mut payload = Vec::new();
    let mut acc: u64 = 0;
    let mut nbits = 0u32;
    for &v in &s.packed {
        acc = (acc << s.bits) | u64::from(v);
        nbits += u32::from(s.bits);
        while nbits >= 8 {
            payload.push((acc >> (nbits - 8)) as u8);
            nbits -= 8;
        }
    }
    let unused = if nbits > 0 {
        payload.push((acc << (8 - nbits)) as u8);
        8 - nbits
    } else {
        0
    };
    payload.extend_from_slice(&s.bds_extra);
    let unused = s.bds_unused.unwrap_or(unused as u8);
    let mut bds = Vec::new();
    bds.extend_from_slice(&u24(11 + payload.len()));
    bds.push(s.bds_flag | unused);
    bds.extend_from_slice(&sm2(s.binary_scale));
    bds.extend_from_slice(&s.reference_value);
    bds.push(s.bits);
    bds.extend_from_slice(&payload);
    out.extend_from_slice(&bds);
    out.extend_from_slice(b"7777");
    let total = out.len();
    out[4..7].copy_from_slice(&u24(total));
    out
}

fn file(name: &str, bytes: Vec<u8>) -> Result<Grib1File, VortexError> {
    Grib1File::from_bytes(name, bytes, Limits::default())
}

fn only_variable(f: &Grib1File) -> String {
    let vars = f.variables();
    assert_eq!(vars.len(), 1, "{:?}", f.unlisted());
    vars[0].name.clone()
}

#[test]
fn simple_packing_is_javas_float_arithmetic() {
    let s = Spec::default();
    let f = file("ST4.2008010812.01h", message(&s)).unwrap();
    let cells = f.decode_record(0).unwrap();
    // ref = (float)(0.1 * 100f), scale = (float)(0.1 * 2^-2), value = ref + scale * X in float.
    let reference = (0.1_f64 * 100.0_f64) as f32;
    let scale = (0.1_f64 * 0.25) as f32;
    let expected: Vec<f32> = s
        .packed
        .iter()
        .map(|&x| reference + scale * x as f32)
        .collect();
    assert_eq!(cells.len(), 12);
    for (a, b) in cells.iter().zip(&expected) {
        assert_eq!(a.to_bits(), b.to_bits());
    }
}

#[test]
fn bitmap_points_left_out_are_nan_and_values_follow_set_bits() {
    let present = vec![
        true, false, true, true, false, false, true, true, true, false, true, true,
    ];
    let count = present.iter().filter(|p| **p).count() as u32;
    let s = Spec {
        bitmap: Some(present.clone()),
        packed: (1..=count).collect(),
        ..Spec::default()
    };
    let f = file("ST4.2008010812.01h", message(&s)).unwrap();
    let cells = f.decode_record(0).unwrap();
    let mut k = 0;
    for (cell, p) in cells.iter().zip(&present) {
        if *p {
            k += 1;
            assert!(!cell.is_nan());
            assert_eq!(*cell, 10.0_f32 + 0.025_f32 * k as f32);
        } else {
            assert!(cell.is_nan());
        }
    }
}

#[test]
fn a_constant_field_needs_no_payload() {
    let s = Spec {
        bits: 0,
        packed: vec![],
        ..Spec::default()
    };
    let cells = file("x", message(&s)).unwrap().decode_record(0).unwrap();
    assert!(cells.iter().all(|c| *c == 10.0));
}

#[test]
fn names_windows_and_units_are_vortexs() {
    for (p2, hour, interval) in [(1u8, 11u8, "1"), (6, 6, "6"), (24, 12, "24")] {
        let s = Spec {
            p2,
            reference: (8, 1, 8, hour),
            ..Spec::default()
        };
        let f = file("ST4.2008010812.xxh", message(&s)).unwrap();
        let name = only_variable(&f);
        assert_eq!(
            name,
            format!("Total_precipitation_surface_{interval}_Hour_Accumulation")
        );
        let frame = f.frame(&name, 0).unwrap();
        let p = frame.parts();
        assert_eq!(p.units, "kg m^-2");
        assert_eq!(p.data_type, DataType::PerCum);
        assert_eq!(
            p.description.as_deref(),
            Some(
                format!(
                    "Total precipitation ({interval}_Hour Accumulation) @ Ground or water surface"
                )
                .as_str()
            )
        );
        assert!(p.nodata.unwrap().is_nan());
        assert_eq!(p.row_order, RowOrder::SouthToNorth);
        // 2008-01-08 is HEC julian day 39454.
        let start = 39454 * 86_400 + i64::from(hour) * 3600;
        let end = start + i64::from(p2) * 3600;
        let split = |t: i64| (t.div_euclid(86_400), t.rem_euclid(86_400));
        assert_eq!(
            p.time,
            FrameTime::Period {
                start: split(start),
                end: split(end)
            }
        );
    }
}

#[test]
fn the_georeference_is_the_hrap_stereographic() {
    let f = file("x", message(&Spec::default())).unwrap();
    let g = f.georeference(0).unwrap();
    assert_eq!((g.nx, g.ny), (4, 3));
    assert!(g
        .wkt
        .contains("PARAMETER[\"central_meridian\",-105.000007629395]"));
    assert!(g
        .wkt
        .contains("PARAMETER[\"scale_factor\",0.933012701892219]"));
    assert!(g.wkt.contains("SPHEROID[\"sphere\",6371229,0]"));
    // The first point of Stage IV's grid; the f32 axis gives Vortex's origin and spacing.
    assert_eq!(g.origin_x, -1904920.0439453125);
    // The mean of f32 km steps: 4762.97 m over 4 points, 4762.999998 m over 1121 (Vortex).
    assert!((g.dx - 4763.0).abs() < 0.1, "{}", g.dx);
}

#[test]
fn an_east_longitude_above_180_is_the_same_point_as_its_negative() {
    // The 2002 Stage IV GDS states Lo1 = 240.983; lonNormal makes it -119.017.
    let east = file(
        "x",
        message(&Spec {
            lo1: 240983,
            ..Spec::default()
        }),
    )
    .unwrap()
    .georeference(0)
    .unwrap();
    let west = file(
        "x",
        message(&Spec {
            lo1: -119017,
            ..Spec::default()
        }),
    )
    .unwrap()
    .georeference(0)
    .unwrap();
    // The two f32 inputs differ by 2.3e-5 degrees (240.98302 - 360 against -119.01701), a few metres
    // here; exact parity on the real 2002 file was checked during development.
    assert!(
        (east.origin_x - west.origin_x).abs() < 5.0,
        "{east:?} {west:?}"
    );
    assert!(
        (east.origin_y - west.origin_y).abs() < 5.0,
        "{east:?} {west:?}"
    );
}

#[test]
fn concatenated_hours_are_steps_of_one_variable_in_time_order() {
    let mut bytes = Vec::new();
    for hour in [2u8, 0, 1] {
        bytes.extend(message(&Spec {
            reference: (17, 8, 27, hour),
            ..Spec::default()
        }));
    }
    let f = file("x", bytes).unwrap();
    let vars = f.variables();
    assert_eq!(vars.len(), 1);
    let records: Vec<usize> = vars[0].steps.iter().map(|s| s.record).collect();
    assert_eq!(records, vec![1, 2, 0]);
}

fn refused(spec: Spec) -> String {
    let f = match file("x", message(&spec)) {
        Ok(f) => f,
        Err(e) => return e.to_string(),
    };
    if let Some((_, e)) = f.unlisted().into_iter().next() {
        return e.to_string();
    }
    match f.decode_record(0) {
        Err(e) => e.to_string(),
        Ok(_) => panic!("decoded"),
    }
}

#[test]
fn variants_outside_stage_iv_are_typed_refusals() {
    let cases: Vec<(Spec, &str)> = vec![
        (
            Spec {
                time_range: 0,
                ..Spec::default()
            },
            "time range indicator 0",
        ),
        (
            Spec {
                time_unit: 0,
                ..Spec::default()
            },
            "time unit 0",
        ),
        (
            Spec {
                parameter: 11,
                ..Spec::default()
            },
            "no product table entry",
        ),
        (
            Spec {
                center: 98,
                ..Spec::default()
            },
            "no product table entry",
        ),
        (
            Spec {
                level_type: 105,
                ..Spec::default()
            },
            "level type 105",
        ),
        (
            Spec {
                bds_flag: 0x40,
                ..Spec::default()
            },
            "complex or second-order packing",
        ),
        (
            Spec {
                bds_flag: 0x80,
                ..Spec::default()
            },
            "spherical harmonic",
        ),
        (
            Spec {
                scan_mode: 0,
                ..Spec::default()
            },
            "scan mode 0",
        ),
        (
            Spec {
                resolution: 0x48,
                ..Spec::default()
            },
            "oblate spheroid",
        ),
        (
            Spec {
                representation: 3,
                ..Spec::default()
            },
            "representation type 3",
        ),
        (
            Spec {
                bitmap: Some(vec![true; 12]),
                bitmap_table: 7,
                ..Spec::default()
            },
            "predefined bitmap 7",
        ),
    ];
    for (spec, text) in cases {
        let error = refused(spec);
        assert!(error.contains(text), "expected {text:?} in {error}");
    }
}

#[test]
fn a_file_mixing_editions_is_refused() {
    let mut bytes = message(&Spec::default());
    bytes.extend_from_slice(b"GRIB\0\0\0\x02");
    bytes.extend_from_slice(&[0; 32]);
    let e = file("x", bytes).unwrap_err().to_string();
    assert!(e.contains("not edition 1"), "{e}");
}

#[test]
fn short_payload_and_bad_lengths_are_errors() {
    let mut s = Spec::default();
    s.packed.truncate(6);
    let e = refused(s);
    assert!(e.contains("12 values of 7 bits need 84 bits"), "{e}");
    let mut bytes = message(&Spec::default());
    let n = bytes.len();
    bytes[n - 1] = b'8';
    assert!(file("x", bytes).is_err());
}

#[test]
fn the_importer_chooses_the_reader_by_edition() {
    let dir = std::env::temp_dir().join(format!("p73-grib1-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("ST4.2008010812.01h");
    std::fs::write(&path, message(&Spec::default())).unwrap();
    let source = open_source(&path, &OpenOptions::default()).unwrap();
    assert_eq!(
        source.variables(),
        vec!["Total_precipitation_surface_1_Hour_Accumulation".to_string()]
    );
    let frame = source
        .frame("Total_precipitation_surface_1_Hour_Accumulation", 0)
        .unwrap();
    assert_eq!(frame.parts().nx, 4);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn window_rule_vortex_gives_the_same_window_for_stage_iv_names() {
    let f = file("ST4.2008010812.01h", message(&Spec::default())).unwrap();
    let name = only_variable(&f);
    let a = f.frame_with(&name, 0, WindowRule::Refuse).unwrap();
    let b = f.frame_with(&name, 0, WindowRule::Vortex).unwrap();
    assert_eq!(a.parts().time, b.parts().time);
}

/// Hostile input: deterministic mutations of a valid message never panic.
#[test]
fn mutations_never_panic() {
    let base = message(&Spec {
        bitmap: Some(vec![true; 12]),
        ..Spec::default()
    });
    let mut state: u64 = 0x9e37_79b9_7f4a_7c15;
    let mut next = || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    for round in 0..3000 {
        let mut bytes = base.clone();
        match round % 4 {
            0 => {
                let at = (next() as usize) % bytes.len();
                bytes[at] = next() as u8;
            }
            1 => {
                let at = (next() as usize) % bytes.len();
                bytes[at] ^= 1 << (next() % 8);
            }
            2 => {
                let keep = (next() as usize) % bytes.len();
                bytes.truncate(keep);
            }
            _ => {
                for _ in 0..4 {
                    let at = (next() as usize) % bytes.len();
                    bytes[at] = 0xff;
                }
            }
        }
        if let Ok(f) = file("ST4.2008010812.01h", bytes) {
            for i in 0..f.record_count() {
                let _ = f.decode_record(i);
                let _ = f.georeference(i);
            }
            for v in f.variables() {
                let _ = f.frame_with(&v.name, 0, WindowRule::Vortex);
            }
        }
    }
}

#[test]
fn a_huge_declared_grid_is_refused_by_the_cell_limit() {
    let s = Spec {
        nx: 0xfffe,
        ny: 0xfffe,
        bits: 0,
        packed: vec![],
        ..Spec::default()
    };
    let limits = Limits {
        max_cells: 1 << 20,
        ..Limits::default()
    };
    let f = Grib1File::from_bytes("x", message(&s), limits).unwrap();
    let e = f.decode_record(0).unwrap_err().to_string();
    assert!(e.contains("over the limit"), "{e}");
}

// ---- Crafted headers ----

fn decode(spec: &Spec) -> Result<Vec<f32>, VortexError> {
    file("ST4.2008010812.01h", message(spec))?.decode_record(0)
}

fn twelve_present() -> Vec<bool> {
    vec![true; 12]
}

/// Grow the BMS by `extra` zero octets and declare `unused` padding bits, fixing the lengths.
fn pad_bitmap(mut bytes: Vec<u8>, extra: usize, unused: u8) -> Vec<u8> {
    // GRIB(8) + PDS(28) + GDS(32): the BMS starts at 68.
    let bms = 68;
    let len = (usize::from(bytes[bms]) << 16)
        | (usize::from(bytes[bms + 1]) << 8)
        | usize::from(bytes[bms + 2]);
    let new_len = len + extra;
    bytes[bms..bms + 3].copy_from_slice(&u24(new_len));
    bytes[bms + 3] = unused;
    for _ in 0..extra {
        bytes.insert(bms + len, 0);
    }
    let total = bytes.len();
    bytes[4..7].copy_from_slice(&u24(total));
    bytes
}

#[test]
fn bitmap_declaring_too_few_usable_bits_is_refused() {
    // 12 cells, 2 octets, 8 declared unused: only 8 usable bits for 12 points.
    let spec = Spec {
        bitmap: Some(twelve_present()),
        bms_unused: Some(8),
        ..Spec::default()
    };
    let e = decode(&spec).unwrap_err().to_string();
    assert!(e.contains("declares 8 unused"), "{e}");
}

#[test]
fn bitmap_declaring_too_many_usable_bits_is_refused() {
    let spec = Spec {
        bitmap: Some(twelve_present()),
        bms_unused: Some(0),
        ..Spec::default()
    };
    let e = decode(&spec).unwrap_err().to_string();
    assert!(e.contains("declares 0 unused"), "{e}");
}

#[test]
fn bitmap_unused_count_over_the_section_is_refused() {
    let spec = Spec {
        bitmap: Some(twelve_present()),
        bms_unused: Some(200),
        ..Spec::default()
    };
    assert!(decode(&spec).is_err());
}

#[test]
fn bitmap_with_even_octet_padding_over_seven_bits_decodes() {
    // Stage IV declares 15 unused bits; here one zero octet more, 12 unused.
    let spec = Spec {
        bitmap: Some(twelve_present()),
        ..Spec::default()
    };
    let plain = decode(&spec).unwrap();
    let padded = file("x", pad_bitmap(message(&spec), 1, 12))
        .unwrap()
        .decode_record(0)
        .unwrap();
    assert_eq!(
        plain.iter().map(|v| v.to_bits()).collect::<Vec<_>>(),
        padded.iter().map(|v| v.to_bits()).collect::<Vec<_>>()
    );
}

#[test]
fn bitmap_padding_that_is_not_zero_is_refused() {
    let spec = Spec {
        bitmap: Some(twelve_present()),
        ..Spec::default()
    };
    let mut bytes = pad_bitmap(message(&spec), 1, 12);
    // Set a bit in the padding octet (BMS data starts at 68 + 6; octets 2 are the bitmap).
    bytes[68 + 6 + 2] = 0x01;
    let e = file("x", bytes)
        .unwrap()
        .decode_record(0)
        .unwrap_err()
        .to_string();
    assert!(e.contains("not zero"), "{e}");
}

#[test]
fn data_section_declaring_too_few_usable_bits_is_refused() {
    // 12 values of 7 bits, 88 physical bits, 15 declared unused: 73 usable for 84 needed.
    let spec = Spec {
        bds_unused: Some(15),
        ..Spec::default()
    };
    let e = decode(&spec).unwrap_err().to_string();
    assert!(e.contains("need 84 bits"), "{e}");
}

#[test]
fn data_section_with_undeclared_extra_octets_is_refused() {
    let spec = Spec {
        bds_extra: vec![0, 0],
        ..Spec::default()
    };
    let e = decode(&spec).unwrap_err().to_string();
    assert!(e.contains("need 84 bits"), "{e}");
}

#[test]
fn data_section_with_declared_even_octet_padding_decodes() {
    // 84 bits in 11 octets leaves 4; one zero octet more and 12 declared unused.
    let plain = decode(&Spec::default()).unwrap();
    let padded = decode(&Spec {
        bds_extra: vec![0],
        bds_unused: Some(12),
        ..Spec::default()
    })
    .unwrap();
    assert_eq!(
        plain.iter().map(|v| v.to_bits()).collect::<Vec<_>>(),
        padded.iter().map(|v| v.to_bits()).collect::<Vec<_>>()
    );
}

#[test]
fn data_section_padding_that_is_not_zero_is_refused() {
    let e = decode(&Spec {
        bds_extra: vec![0x80],
        bds_unused: Some(12),
        ..Spec::default()
    })
    .unwrap_err()
    .to_string();
    assert!(e.contains("not zero"), "{e}");
}

#[test]
fn additional_flags_are_refused() {
    let e = decode(&Spec {
        bds_flag: 0x10,
        ..Spec::default()
    })
    .unwrap_err();
    assert!(
        matches!(e, VortexError::Unsupported { .. }) && e.to_string().contains("additional flags"),
        "{e}"
    );
}

#[test]
fn the_original_integers_flag_is_accepted() {
    let plain = decode(&Spec::default()).unwrap();
    let integers = decode(&Spec {
        bds_flag: 0x20,
        ..Spec::default()
    })
    .unwrap();
    assert_eq!(plain, integers);
}

fn georef(spec: &Spec) -> Result<dss_vortex::source::grib::Georeference, VortexError> {
    file("x", message(spec))?.georeference(0)
}

#[test]
fn dx_with_the_top_bit_set_is_refused_not_read_as_negative() {
    // 80 12 9b: unsigned 8,393,371 m; a sign-magnitude reading gives -4,763 m.
    for (dx, dy) in [
        ([0x80, 0x12, 0x9b], [0x00, 0x12, 0x9b]),
        ([0x00, 0x12, 0x9b], [0x80, 0x12, 0x9b]),
    ] {
        let e = georef(&Spec {
            dx,
            dy,
            ..Spec::default()
        })
        .unwrap_err()
        .to_string();
        assert!(e.contains("8393371 m"), "{e}");
        let e = decode(&Spec {
            dx,
            dy,
            ..Spec::default()
        })
        .unwrap_err()
        .to_string();
        assert!(e.contains("8393371 m"), "{e}");
    }
}

#[test]
fn a_zero_grid_length_is_refused() {
    let e = georef(&Spec {
        dy: [0, 0, 0],
        ..Spec::default()
    })
    .unwrap_err()
    .to_string();
    assert!(e.contains("Dy is 0 m"), "{e}");
}

#[test]
fn the_largest_unsigned_length_below_the_top_bit_reads_as_positive() {
    let g = georef(&Spec {
        dx: [0x7f, 0xff, 0xff],
        ..Spec::default()
    })
    .unwrap();
    assert!(g.dx > 8.0e6, "{}", g.dx);
}
