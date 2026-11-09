//! Cross-platform and reference checks for `dss_vortex::maths`.
//!
//! `same_bits_on_every_platform` digests the results over a fixed corpus and compares against a
//! committed value, so CI on any other OS or target fails if one bit differs. NaN results are
//! digested as one value, because the sign of a NaN produced by an invalid operation differs
//! between x86 and ARM and NaN is the missing mask, not a value.
//!
//! `reference_bits` holds outputs recorded from `ucrtbase.dll` 10.0.26100.9711 with FMA3 enabled,
//! by black-box calls from a scratch harness. `known_residuals_have_not_moved` holds inputs where
//! the module is known to differ, so a change there shows up too.

use dss_vortex::maths;

fn fnv(mut h: u64, v: u64) -> u64 {
    for b in v.to_le_bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        // xorshift64*: integer only, so the corpus is the same everywhere.
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x9e37_79b9_7f4a_7c15)
    }

    /// In `[lo, hi)`, from `+`, `*` and an exact integer conversion only.
    fn range(&mut self, lo: f64, hi: f64) -> f64 {
        let unit = (self.next() >> 11) as f64 * f64::from_bits(0x3ca0_0000_0000_0000);
        lo + (hi - lo) * unit
    }

    /// Any bit pattern with a finite exponent.
    fn any_finite(&mut self) -> f64 {
        let b = self.next();
        if (b >> 52) & 0x7ff == 0x7ff {
            f64::from_bits(b & 0xbfff_ffff_ffff_ffff)
        } else {
            f64::from_bits(b)
        }
    }
}

const EDGES: &[u64] = &[
    0x0000_0000_0000_0000,
    0x8000_0000_0000_0000,
    0x3ff0_0000_0000_0000,
    0xbff0_0000_0000_0000,
    0x7ff0_0000_0000_0000,
    0xfff0_0000_0000_0000,
    0x7ff8_0000_0000_0000,
    0x0000_0000_0000_0001,
    0x000f_ffff_ffff_ffff,
    0x7fef_ffff_ffff_ffff,
    0x3fe9_21fb_5444_2d18,
    0x3ff9_21fb_5444_2d18,
    0x4009_21fb_5444_2d18,
    0x4153_12d0_0000_0000,
    0x3fe5_80a0_fb4b_6529,
];

fn canon(v: f64) -> u64 {
    if v.is_nan() {
        0x7ff8_0000_0000_0000
    } else {
        v.to_bits()
    }
}

fn digest_one(f: fn(f64) -> f64, seed: u64, lo: f64, hi: f64) -> u64 {
    let mut rng = Rng(seed);
    let mut h = 0xcbf2_9ce4_8422_2325;
    for &e in EDGES {
        h = fnv(h, canon(f(f64::from_bits(e))));
    }
    for _ in 0..200_000 {
        h = fnv(h, canon(f(rng.range(lo, hi))));
    }
    for _ in 0..50_000 {
        h = fnv(h, canon(f(rng.any_finite())));
    }
    h
}

fn digest_two(f: fn(f64, f64) -> f64, seed: u64, a: (f64, f64), b: (f64, f64)) -> u64 {
    let mut rng = Rng(seed);
    let mut h = 0xcbf2_9ce4_8422_2325;
    for &x in EDGES {
        for &y in EDGES {
            h = fnv(h, canon(f(f64::from_bits(x), f64::from_bits(y))));
        }
    }
    for _ in 0..200_000 {
        let x = rng.range(a.0, a.1);
        let y = rng.range(b.0, b.1);
        h = fnv(h, canon(f(x, y)));
    }
    for _ in 0..50_000 {
        let x = rng.any_finite();
        let y = rng.any_finite();
        h = fnv(h, canon(f(x, y)));
    }
    h
}

/// Exact fused multiply-add cases: a rounding here would change every fused step above.
fn digest_fma(seed: u64) -> u64 {
    let mut rng = Rng(seed);
    let mut h = 0xcbf2_9ce4_8422_2325;
    for _ in 0..200_000 {
        let a = rng.range(-2.0, 2.0);
        let b = rng.range(-2.0, 2.0);
        // Near cancellation: the exact product's rounded value, negated.
        h = fnv(h, canon(a.mul_add(b, -(a * b))));
        h = fnv(h, canon(a.mul_add(b, rng.range(-1.0, 1.0))));
    }
    h
}

/// Extreme exponents, subnormal results, ties and overflow edges.
fn fma_hard_cases(seed: u64, mut check: impl FnMut(f64, f64, f64)) {
    let mut rng = Rng(seed);
    for _ in 0..150_000 {
        let any = |r: &mut Rng| f64::from_bits(r.next());
        let (a, b, c) = (any(&mut rng), any(&mut rng), any(&mut rng));
        check(a, b, c);
        // Subnormal results.
        let small = |r: &mut Rng| {
            let exp = 480 + r.next() % 60;
            f64::from_bits((r.next() & 0x800f_ffff_ffff_ffff) | (exp << 52))
        };
        let (a, b) = (small(&mut rng), small(&mut rng));
        let c = f64::from_bits(rng.next() & 0x801f_ffff_ffff_ffff);
        check(a, b, c);
        // Ties: few significant bits.
        let short = |r: &mut Rng| f64::from_bits((r.next() & 0xbfff_fc00_0000_0000) | (1023 << 52));
        let (a, b) = (short(&mut rng), short(&mut rng));
        let c = f64::from_bits((rng.next() & 0xbfff_0000_0000_0000) | 0x3ca0_0000_0000_0000);
        check(a, b, c);
        // Overflow edge.
        let big =
            |r: &mut Rng, e: u64| f64::from_bits((r.next() & 0x800f_ffff_ffff_ffff) | (e << 52));
        let (a, b, c) = (
            big(&mut rng, 1500),
            big(&mut rng, 1570),
            big(&mut rng, 2046),
        );
        check(a, b, c);
        // Exact and near cancellation.
        let a = rng.range(-2.0, 2.0);
        let b = rng.range(-2.0, 2.0);
        check(a, b, -(a * b));
        let nudge = 1.0 + rng.range(-1e-15, 1e-15);
        check(a, b, -(a * b) * nudge);
    }
}

/// A CONUS LCC domain: bases `tan(pi/4 + lat/2)`, exponents `-sin(lat)`.
fn digest_conus_lcc(seed: u64) -> u64 {
    let mut rng = Rng(seed);
    let mut h = 0xcbf2_9ce4_8422_2325;
    let deg = f64::from_bits(0x3f91_df46_a252_9d39);
    let quarter_pi = f64::from_bits(0x3fe9_21fb_5444_2d18);
    for _ in 0..500_000 {
        let lat = rng.range(20.0, 55.0) * deg;
        let parallel = rng.range(20.0, 55.0) * deg;
        let base = maths::tan(quarter_pi + 0.5 * lat);
        h = fnv(h, canon(maths::pow(base, -maths::sin(parallel))));
        h = fnv(h, canon(maths::log(base)));
    }
    h
}

fn digest_software_fma(seed: u64) -> u64 {
    let mut h = 0xcbf2_9ce4_8422_2325;
    fma_hard_cases(seed, |a, b, c| {
        h = fnv(h, canon(maths::software_fma(a, b, c)))
    });
    h
}

fn digests() -> Vec<(&'static str, u64)> {
    let piby = (-7.0, 7.0);
    vec![
        ("sin", digest_one(maths::sin, 11, piby.0, piby.1)),
        ("cos", digest_one(maths::cos, 12, piby.0, piby.1)),
        ("tan", digest_one(maths::tan, 13, piby.0, piby.1)),
        ("asin", digest_one(maths::asin, 14, -1.0, 1.0)),
        ("atan", digest_one(maths::atan, 15, -5.0, 5.0)),
        (
            "atan2",
            digest_two(maths::atan2, 16, (-5e6, 5e6), (-5e6, 5e6)),
        ),
        ("log", digest_one(maths::log, 17, 1e-3, 30.0)),
        ("exp", digest_one(maths::uncertified::exp, 18, -40.0, 40.0)),
        ("pow", digest_two(maths::pow, 19, (0.2, 8.0), (-1.5, 1.5))),
        (
            "hypot",
            digest_two(maths::hypot, 20, (-5e6, 5e6), (-5e6, 5e6)),
        ),
        ("mul_add", digest_fma(21)),
        ("conus_lcc", digest_conus_lcc(22)),
        ("software_fma", digest_software_fma(23)),
    ]
}

const EXPECTED: &[(&str, u64)] = &[
    ("sin", 0x94da_e5a7_5d25_e39b),
    ("cos", 0x5c4f_0d5e_5759_a72d),
    ("tan", 0x1868_e9eb_17f7_4dd9),
    ("asin", 0xdc84_21dc_9af2_6f41),
    ("atan", 0x1631_6563_38c4_8e71),
    ("atan2", 0xc71f_4642_78ee_1ff5),
    ("log", 0x3d1c_d9ef_9e9f_e3a8),
    ("exp", 0x6956_7032_0f57_e731),
    ("pow", 0xdbc5_83c2_3dab_5f3f),
    ("hypot", 0x9fc0_fdbb_4351_5c40),
    ("mul_add", 0x714b_e292_dce8_cf3d),
    ("conus_lcc", 0xcf34_eb46_8952_d948),
    ("software_fma", 0x9cc5_b6e6_f2ca_75fd),
];

#[test]
fn same_bits_on_every_platform() {
    let got = digests();
    if std::env::var_os("P44A_WRITE_DIGESTS").is_some() {
        for (name, value) in &got {
            println!("    (\"{name}\", 0x{value:016x}),");
        }
    }
    assert_eq!(got, EXPECTED);
}

#[test]
fn reference_bits() {
    let text = concat!(
        include_str!("data/maths_reference.txt"),
        "
",
        include_str!("data/maths_pow_edges.txt"),
    );
    let (mut checked, mut bad) = (0u32, Vec::new());
    for line in text
        .lines()
        .filter(|l| !l.starts_with('#') && !l.is_empty())
    {
        let parts: Vec<&str> = line.split_whitespace().collect();
        let bits = |s: &str| u64::from_str_radix(s, 16).unwrap();
        let (x, y, want) = (
            f64::from_bits(bits(parts[1])),
            f64::from_bits(bits(parts[2])),
            bits(parts[3]),
        );
        let got = match parts[0] {
            "sin" => maths::sin(x),
            "cos" => maths::cos(x),
            "tan" => maths::tan(x),
            "asin" => maths::asin(x),
            "atan" => maths::atan(x),
            "atan2" => maths::atan2(x, y),
            "log" => maths::log(x),
            "exp" => maths::uncertified::exp(x),
            "pow" => maths::pow(x, y),
            "hypot" => maths::hypot(x, y),
            other => panic!("unknown function {other}"),
        };
        checked += 1;
        let both_nan = got.is_nan() && f64::from_bits(want).is_nan();
        if got.to_bits() != want && !both_nan {
            bad.push(format!("{line} -> {:016x}", got.to_bits()));
        }
    }
    assert!(checked > 5_000, "reference file too small: {checked}");
    assert!(
        bad.is_empty(),
        "{} differ:\n{}",
        bad.len(),
        bad[..bad.len().min(20)].join("\n")
    );
}

#[test]
fn known_residuals_have_not_moved() {
    let text = include_str!("data/maths_residual.txt");
    let mut rows = 0u32;
    for line in text
        .lines()
        .filter(|l| !l.starts_with('#') && !l.is_empty())
    {
        let p: Vec<&str> = line.split_whitespace().collect();
        let bits = |s: &str| u64::from_str_radix(s, 16).unwrap();
        let (x, y, ucrt) = (
            f64::from_bits(bits(p[1])),
            f64::from_bits(bits(p[2])),
            bits(p[3]),
        );
        let got = match p[0] {
            "exp" => maths::uncertified::exp(x),
            "atan2" => maths::atan2(x, y),
            "pow" => maths::pow(x, y),
            other => panic!("{other} has no residual on the expected list"),
        }
        .to_bits();
        let distance = (got as i64).wrapping_sub(ucrt as i64).unsigned_abs();
        assert_eq!(distance, 1, "{line}: module gives {got:016x}");
        rows += 1;
    }
    assert!(rows > 100, "residual file too small: {rows}");
}

#[test]
fn software_fma_agrees_with_mul_add() {
    // With FMA in the build this is the instruction; elsewhere it is the platform's `fma`.
    let mut bad = Vec::new();
    fma_hard_cases(24, |a, b, c| {
        let (s, h) = (maths::software_fma(a, b, c), a.mul_add(b, c));
        if s.to_bits() != h.to_bits() && !(s.is_nan() && h.is_nan()) {
            bad.push(format!("{a:e} {b:e} {c:e}: software {s:e}, mul_add {h:e}"));
        }
    });
    assert!(
        bad.is_empty(),
        "{} differ:\n{}",
        bad.len(),
        bad[..bad.len().min(10)].join("\n")
    );
}

#[test]
fn pow_exact_powers_of_two_including_subnormal_and_overflow() {
    for e in -1074..=1023 {
        let want = if e >= -1022 {
            ((e + 1023) as u64) << 52
        } else {
            1u64 << (e + 1074)
        };
        assert_eq!(maths::pow(2.0, f64::from(e)).to_bits(), want, "2^{e}");
        if e % 2 == 0 {
            assert_eq!(
                maths::pow(4.0, f64::from(e / 2)).to_bits(),
                want,
                "4^{}",
                e / 2
            );
        }
        let negative = if e % 2 == 0 { want } else { want | (1 << 63) };
        assert_eq!(
            maths::pow(-2.0, f64::from(e)).to_bits(),
            negative,
            "(-2)^{e}"
        );
    }
    assert_eq!(
        maths::pow(2.0, -1075.0).to_bits(),
        0,
        "tie rounds to even zero"
    );
    assert_eq!(maths::pow(2.0, -1076.0).to_bits(), 0);
    assert_eq!(
        maths::pow(-2.0, -1075.0).to_bits(),
        1 << 63,
        "the sign survives underflow"
    );
    assert_eq!(maths::pow(2.0, 1024.0), f64::INFINITY);
    assert_eq!(maths::pow(-2.0, 1025.0), f64::NEG_INFINITY);
    assert_eq!(maths::pow(10.0, 308.0).to_bits(), 0x7fe1_ccf3_85eb_c8a0);
}

#[test]
fn fma_nonfinite_and_overflow_cases() {
    let (inf, nan, max) = (f64::INFINITY, f64::NAN, f64::MAX);
    let f = maths::software_fma;
    // A finite product that alone would overflow does not meet the infinity.
    assert_eq!(f(max, 2.0, -inf), -inf);
    assert_eq!(f(-max, 2.0, inf), inf);
    assert_eq!(f(max, max, -inf), -inf);
    // The exact product cancels, where rounding it first would not.
    assert_eq!(f(max, 2.0, -max), max);
    assert_eq!(f(max, 2.0, 0.0), inf);
    assert_eq!(f(-max, 2.0, 0.0), -inf);
    assert!(f(inf, 1.0, -inf).is_nan());
    assert!(f(inf, 0.0, 1.0).is_nan());
    assert!(f(0.0, inf, 1.0).is_nan());
    assert!(f(nan, 1.0, 1.0).is_nan());
    assert!(f(1.0, nan, 1.0).is_nan());
    assert!(f(1.0, 1.0, nan).is_nan());
    assert_eq!(f(inf, 1.0, inf), inf);
    assert_eq!(f(inf, -1.0, 1.0), -inf);
    // Every pair of specials, against `mul_add`.
    let values = [
        0.0, -0.0, 1.0, -1.0, 2.0, 0.5, 5e-324, -5e-324, max, -max, inf, -inf, nan,
    ];
    for a in values {
        for b in values {
            for c in values {
                let (s, h) = (f(a, b, c), a.mul_add(b, c));
                let same = s.to_bits() == h.to_bits() || (s.is_nan() && h.is_nan());
                assert!(
                    same,
                    "fma({a:e}, {b:e}, {c:e}): software {s:e}, mul_add {h:e}"
                );
            }
        }
    }
}

/// Known pow differences near even powers of two: results here sit next to a rounding midpoint.
#[test]
fn pow_near_tie_family_is_held() {
    let text = include_str!("data/maths_pow_near_tie.txt");
    let mut rows = 0u32;
    for line in text
        .lines()
        .filter(|l| !l.starts_with('#') && !l.is_empty())
    {
        let p: Vec<&str> = line.split_whitespace().collect();
        let bits = |s: &str| u64::from_str_radix(s, 16).unwrap();
        let (x, y) = (f64::from_bits(bits(p[0])), f64::from_bits(bits(p[1])));
        let (ucrt, module) = (bits(p[2]), bits(p[3]));
        assert!(p[4] == "module" || p[4] == "ucrt", "{line}");
        assert_eq!(maths::pow(x, y).to_bits(), module, "{line}");
        assert_eq!(
            (ucrt as i64).wrapping_sub(module as i64).unsigned_abs(),
            1,
            "{line}"
        );
        rows += 1;
    }
    assert_eq!(rows, 400);

    // Every platform must give the same bits across the family, not only on the witnesses.
    let mut rng = Rng(0x6e41_f2a9_7c03_d581);
    let mut h = 0xcbf2_9ce4_8422_2325;
    for e in [52u64, 50, 48, 40, 30, 20, 10, 2, 0] {
        let base = (e + 1023) << 52;
        for i in 0..50_000 {
            let x = f64::from_bits(base + rng.next() % 2_000_001 - 1_000_000);
            let y = if i % 2 == 0 { 0.5 } else { 1.5 };
            h = fnv(h, canon(maths::pow(x, y)));
        }
    }
    assert_eq!(h, 0x8660_1252_9cff_bc08);
}

#[test]
fn traced_sine_input_matches_reference_bits() {
    let x = f64::from_bits(0x3fe5_80a0_fb4b_6529);
    assert_eq!(maths::sin(x).to_bits(), 0x3fe3_eba3_d0b4_7a0f);
}
