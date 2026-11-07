//! Reference checks for the functions Transverse Mercator added to `dss_vortex::maths` (P53b):
//! `sinh`, `asinh` and `tmerc_exp`.
//!
//! `data/maths_tm_reference.txt` holds results recorded from `ucrtbase.dll` 10.0.26100.9711 with
//! FMA3 enabled, by black-box calls from a scratch harness, as `maths_reference.txt` does for
//! P44a's functions. `sinh`, `asinh` and `tmerc_exp` rows lie in the ranges the module is
//! certified for and must match. `exp` rows are the reference's `exp` where the module is not
//! certified: the number that differ is held, so a change there shows up either way.

use dss_vortex::maths;

struct Row {
    name: String,
    x: f64,
    want: u64,
}

fn rows() -> Vec<Row> {
    include_str!("data/maths_tm_reference.txt")
        .lines()
        .filter(|l| !l.starts_with('#') && !l.trim().is_empty())
        .map(|l| {
            let p: Vec<&str> = l.split_whitespace().collect();
            assert_eq!(p.len(), 3, "{l}");
            Row {
                name: p[0].to_string(),
                x: f64::from_bits(u64::from_str_radix(p[1], 16).unwrap()),
                want: u64::from_str_radix(p[2], 16).unwrap(),
            }
        })
        .collect()
}

fn same(got: f64, want: u64) -> bool {
    got.to_bits() == want || (got.is_nan() && f64::from_bits(want).is_nan())
}

/// Rows that differ from the reference, by function, under `disturb`.
fn differing(disturb: impl Fn(f64) -> f64) -> (usize, usize, usize) {
    let (mut certified, mut wrong, mut exp_wrong) = (0, 0, 0);
    for r in rows() {
        let got = match r.name.as_str() {
            "sinh" => maths::sinh(r.x),
            "asinh" => maths::asinh(r.x),
            "tmerc_exp" | "exp" => maths::tmerc_exp(r.x),
            other => panic!("unknown function {other}"),
        };
        let got = disturb(got);
        if r.name == "exp" {
            exp_wrong += usize::from(!same(got, r.want));
        } else {
            certified += 1;
            wrong += usize::from(!same(got, r.want));
        }
    }
    (certified, wrong, exp_wrong)
}

#[test]
fn certified_ranges_match_the_reference_and_the_exp_residual_has_not_moved() {
    let (certified, wrong, exp_wrong) = differing(|v| v);
    assert_eq!(certified, 4520, "rows in the certified ranges");
    assert_eq!(wrong, 0);
    // Of 2,000 `exp` rows between 2^-26 and 5.3. One ulp each, checked below.
    assert_eq!(exp_wrong, EXP_ROWS_THAT_DIFFER);
}

const EXP_ROWS_THAT_DIFFER: usize = 5;

#[test]
fn every_exp_difference_is_one_ulp() {
    for r in rows().iter().filter(|r| r.name == "exp") {
        let got = maths::tmerc_exp(r.x).to_bits();
        assert!(got.abs_diff(r.want) <= 1, "{:e}", r.x);
    }
}

/// Controls: one low bit on every result, and on one result only.
#[test]
fn the_check_notices_one_changed_bit() {
    let (certified, wrong, _) = differing(|v| f64::from_bits(v.to_bits() ^ 1));
    assert_eq!(wrong, certified);
    let seen = std::cell::Cell::new(0);
    let (_, wrong, _) = differing(|v| {
        seen.set(seen.get() + 1);
        if seen.get() == 1000 {
            f64::from_bits(v.to_bits() ^ 1)
        } else {
            v
        }
    });
    assert_eq!(wrong, 1);
}

#[test]
fn exp_below_two_to_the_minus_26_is_one_plus_x() {
    let edge = f64::from_bits(0x3e50_0000_0000_0000);
    for x in [0.0, -0.0, 1e-300, -1e-300, 1e-9, -1e-9, edge * 0.999] {
        assert_eq!(maths::tmerc_exp(x).to_bits(), (1.0 + x).to_bits(), "{x:e}");
    }
    // On the Transverse Mercator inverse's central meridian the argument is zero.
    assert_eq!(maths::tmerc_exp(0.0), 1.0);
}

#[test]
fn special_values() {
    for f in [maths::sinh as fn(f64) -> f64, maths::asinh] {
        assert_eq!(f(0.0).to_bits(), 0.0f64.to_bits());
        assert_eq!(f(-0.0).to_bits(), (-0.0f64).to_bits());
        assert_eq!(f(f64::INFINITY), f64::INFINITY);
        assert_eq!(f(f64::NEG_INFINITY), f64::NEG_INFINITY);
        assert!(f(f64::NAN).is_nan());
        // Odd functions.
        for x in [1e-320, 1e-10, 0.3, 2.5, 30.0] {
            assert_eq!(f(-x).to_bits(), (-f(x)).to_bits(), "{x:e}");
        }
    }
    // Finite where the squared argument would overflow.
    assert!(maths::asinh(1e300).is_finite());
    assert_eq!(maths::sinh(800.0), f64::INFINITY);
}
