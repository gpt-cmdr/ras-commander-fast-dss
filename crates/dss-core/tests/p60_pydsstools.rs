//! Packet P60: the three engine findings of the P58 pydsstools validation.
//!
//! - E1: DSS6 time-series units and type are C strings (cut at the first NUL), as heclib's
//!   `stringFortToC` returns them and Monolith reads them; pydsstools leaves `CFS`, NUL and
//!   stray bytes in the units word when a container is stored twice.
//! - E2: a legacy grid header whose byte count (int 0) exceeds the stored header reads,
//!   as in Monolith (`GridInfoFlat.unflatten` ignores int 0); pydsstools `put_grid0`
//!   writes 764 into a shorter header.
//! - E3: time-invariant grids are written as 410/420/430 (HEC's DSS6 grid store writes
//!   no other type, and Monolith cannot read DSS7 411/421); DSS7 specified ones keep 431.
//!
//! `fixtures/public/p60_pydsstools6.dss` is written by pydsstools 3.1.0 from synthetic
//! values (`evidence/P60/scripts/pyd_fixture.py`); its SHA-256 is
//! pinned in `fixtures/public/README.md` and checked by `tests/python/test_p60_pydsstools.py`.

use std::path::{Path, PathBuf};

use dss_core::dss6::write::{create_dss6, write_grid6};
use dss_core::dss6::Dss6File;
use dss_core::dss7::write::{
    create_dss7, write_grid, AlbersParameters, GridCompression, GridInput, GridProjection,
    SpecifiedReference, WriteOptions,
};
use dss_core::dss7::Dss7File;
use dss_core::{Grid, GridType, GRID_UNDEFINED};

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/public")
        .join(name)
}

const RTS: &[u8] = b"/P60/RTS/FLOW/01JAN2021/1HOUR/PYD/";
const ITS: &[u8] = b"/P60/ITS/STAGE/01JAN2021/IR-MONTH/PYD/";
const GRID: &[u8] = b"/UTM17N/P60/PRECIP/01JUN2021:0100/01JUN2021:0200/PYD/";

#[test]
fn e1_pydsstools_units_cut_at_first_nul() {
    let path = fixture("p60_pydsstools6.dss");
    // The fixture still carries the pattern: a units word `CFS`, NUL, then non-blank bytes.
    let bytes = std::fs::read(&path).unwrap();
    let at = bytes
        .windows(4)
        .position(|w| w == b"CFS\0")
        .expect("units word with a NUL");
    assert!(
        bytes[at + 4..at + 8].iter().any(|&b| b != 0 && b != b' '),
        "fixture no longer has stray bytes after the NUL"
    );
    let f = Dss6File::open(&path).unwrap();
    let r = f.read_ts_record(RTS).unwrap();
    assert_eq!(
        (r.units.as_str(), r.data_type.as_str()),
        ("CFS", "INST-VAL")
    );
    assert_eq!(r.values.len(), 744);
    let i = f.read_ts_record(ITS).unwrap();
    assert_eq!(
        (i.units.as_str(), i.data_type.as_str()),
        ("FEET", "INST-VAL")
    );
    let ts = f.read_ts(RTS).unwrap();
    assert_eq!(ts.units, "CFS");
}

#[test]
fn e2_pydsstools_grid_header_count_beyond_stored_header() {
    let f = Dss6File::open(fixture("p60_pydsstools6.dss")).unwrap();
    let rec = f.lookup(GRID).unwrap().expect("grid record");
    let stored_ints = rec.info.internal_header.1;
    let g = f.read_grid(GRID).unwrap();
    assert_eq!(g.record_type, 430);
    // pydsstools' count (744 here, 764 in P58: it depends on the WKT length) exceeds
    // the stored header.
    assert!(
        g.struct_version / 4 > stored_ints,
        "byte count {} no longer exceeds the stored header ({stored_ints} ints)",
        g.struct_version
    );
    assert_eq!((g.nx, g.ny, g.cell_size), (3, 2, 1000.0));
    assert_eq!(g.data_units, "MM");
    assert_eq!(g.srs_name, "UTM17N");
    // pydsstools flips rows (row 0 of its input is north); HEC stores the south row first.
    // The -9999 null cell reads as HEC's undefined marker (Java heclib legacy rule, P11).
    assert_eq!(g.null_value, -9999.0);
    assert_eq!(g.data[..3], [4.0, GRID_UNDEFINED, 6.0]);
    assert_eq!(g.data[3..], [1.0, 2.0, 3.0]);
}

fn tmp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("p60_{}_{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn base<'a>(path: &'a [u8], data: &'a [f32], projection: GridProjection) -> GridInput<'a> {
    GridInput {
        pathname: path,
        projection,
        time_stamped: false,
        data,
        nx: 3,
        ny: 2,
        lower_left_cell_x: 10,
        lower_left_cell_y: 20,
        cell_size: 100.0,
        x_coord_of_grid_cell_zero: 0.0,
        y_coord_of_grid_cell_zero: 0.0,
        units: b"FT",
        data_type: 2,
        compression: GridCompression::Zlib,
    }
}

fn read(path: &Path, v: u8, p: &[u8]) -> Grid {
    if v == 6 {
        Dss6File::open(path).unwrap().read_grid(p).unwrap()
    } else {
        Dss7File::open(path).unwrap().read_grid(p).unwrap()
    }
}

/// E3: HEC's DSS6 grid store (Monolith `GriddedData.storeGriddedData`) writes only
/// 410/420/430 and refuses 4x1 (P60 probe: status -1); its own time-invariant DSS6 grid
/// is 420 with empty D and E. DSS7 stores the type it is given, but Monolith reads back
/// only 410/420/430/431 (`GridStructContainer.returnGridInfo`), so HRAP and Albers use
/// 410/420 there too and specified grids keep 431.
#[test]
fn e3_time_invariant_grid_types_follow_hec() {
    let d = tmp("e3");
    let cells = [1.0f32, 2.0, 3.0, 4.0, 5.0, 6.0];
    let kinds: [(&[u8], GridProjection, GridType, i32); 3] = [
        (
            b"/HRAP/P60 HRAP/ELEVATION///SYNTH/",
            GridProjection::Hrap {
                data_source: b"SYNTH".to_vec(),
            },
            GridType::Hrap,
            410,
        ),
        (
            b"/SHG/P60 ALBERS/ELEVATION///SYNTH/",
            GridProjection::Albers(AlbersParameters::shg()),
            GridType::Albers,
            420,
        ),
        (
            b"/SYN/P60 SPECIFIED/ELEVATION///SYNTH/",
            GridProjection::Specified(SpecifiedReference {
                srs_name: b"SYNTH".to_vec(),
                srs_definition: Vec::new(),
                srs_definition_type: 0,
                null_value: -9999.0,
                time_zone_id: b"UTC".to_vec(),
                time_zone_raw_offset: 0,
                is_interval: false,
            }),
            GridType::Specified,
            430,
        ),
    ];
    let o = WriteOptions {
        sync: false,
        ..WriteOptions::default()
    };
    for v in [6u8, 7] {
        let f = d.join(format!("t{v}.dss"));
        if v == 6 {
            create_dss6(&f).unwrap();
        } else {
            create_dss7(&f).unwrap();
        }
        for (p, proj, kind, even) in &kinds {
            let mut input = base(p, &cells, proj.clone());
            if *kind == GridType::Hrap {
                input.cell_size = 4762.5;
            }
            if v == 6 {
                write_grid6(&f, &input, &o).unwrap();
            } else {
                write_grid(&f, &input, &o).unwrap();
            }
            let g = read(&f, v, p);
            let want = if v == 7 && *kind == GridType::Specified {
                even + 1
            } else {
                *even
            };
            assert_eq!(g.record_type, want, "v{v} {kind:?}");
            assert_eq!(g.grid_type, *kind);
            assert_eq!((g.start_time, g.end_time), (None, None), "v{v} {kind:?}");
            assert_eq!(g.data, cells);
            if *kind == GridType::Specified || v == 7 {
                assert!(!g.is_time_stamped, "v{v} {kind:?}");
            }
            if v == 6 {
                // Undefined DSS6 grid times, as Monolith stores them (UNDEFINED_TIME).
                assert_eq!(g.stored_start_time, Some(-2_147_483_647));
            }
            // An equal rewrite of the same input is accepted (no type change).
            if v == 6 {
                write_grid6(&f, &input, &o).unwrap();
            } else {
                write_grid(&f, &input, &o).unwrap();
            }
        }
    }
    let _ = std::fs::remove_dir_all(&d);
}
