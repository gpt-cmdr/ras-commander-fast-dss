//! Time parsing never panics. Unit strings and coordinate values, ordinary and extreme, go through a
//! real classic file and the public route, under the dev profile's overflow checks. Each gives steps
//! or a typed error. `variable()` catches a panic as a decode error, so that message fails the test.

mod nc_raw;

use dss_vortex::source::netcdf::{Limits, NetcdfFile};
use dss_vortex::VortexError;

fn run(units: &[u8], calendar: Option<&[u8]>, times: &[f64]) -> Result<usize, VortexError> {
    let bytes = nc_raw::classic_time_file(units, calendar, times);
    let file = NetcdfFile::from_bytes("t.nc", bytes, Limits::default())?;
    let variable = file.variable("rain")?;
    let mut read = 0;
    for step in 0..variable.step_count() {
        variable.step(step)?;
        read += 1;
    }
    Ok(read)
}

fn check(units: &[u8], calendar: Option<&[u8]>, times: &[f64]) {
    if let Err(error) = run(units, calendar, times) {
        let text = error.to_string();
        assert!(
            !text.contains("panicked"),
            "{:?} {times:?}: {text}",
            String::from_utf8_lossy(units)
        );
    }
}

/// Parts a unit string is made from, chosen to reach each branch of the parser and its limits.
const HEADS: [&str; 12] = [
    "hours",
    "h",
    "ms",
    "s",
    "seconds",
    "days",
    "day",
    "msec",
    "minutes",
    "months",
    "calendar days",
    "fortnights",
];
const DATES: [&str; 22] = [
    "2000-01-01",
    "1970-1-1",
    "20000101",
    "2000",
    "1583-01-01",
    "1582-12-31",
    "0-1-1",
    "-1-1-1",
    "+2000-01-01",
    "999999999-12-31",
    "200000-12-31",
    "200001-01-01",
    "-200000-01-01",
    "2000-02-30",
    "2000-13-01",
    "2000-00-00",
    "2000-01",
    "--",
    "-",
    "?",
    "2000-01-01-01-01",
    "1234567890123-01-01",
];
const TIMES: [&str; 22] = [
    "",
    " 00:00",
    " 00:00:00",
    "T00:00:00",
    " 23:59:59.999",
    " 24:00",
    " 00:60",
    " 00:00:999999999999999999999999999999",
    " 00:00:1e308",
    " 00:00:9223372036854775",
    " 00:00:9223372036854776",
    " 00:00:4611686018427387",
    " 00:00:.",
    " 00:00:0.0.0",
    " ::::",
    " 000000",
    " 0000999999999999999999999",
    " 1",
    " 123",
    " 12.5",
    " ?",
    " 00:00:00.5",
];
const ZONES: [&str; 14] = [
    "",
    "Z",
    "z",
    " UTC",
    " GMT",
    "+00:00",
    "-00:00",
    "+23:59",
    "-23:59",
    "+24:00",
    " +1",
    "-",
    "+999999999:99",
    " x y",
];
const VALUES: [f64; 16] = [
    0.0,
    1.0,
    -1.0,
    0.5,
    1e15,
    -1e15,
    4.6e18,
    -4.6e18,
    9.3e18,
    -9.3e18,
    1e300,
    -1e300,
    f64::MAX,
    f64::MIN,
    f64::MIN_POSITIVE,
    -0.0,
];

#[test]
fn every_combination_gives_steps_or_a_typed_error() {
    let mut tried = 0;
    for head in HEADS {
        for date in DATES {
            for time in TIMES {
                let units = format!("{head} since {date}{time}");
                check(units.as_bytes(), None, &[0.0, 1.0]);
                tried += 1;
            }
        }
    }
    for date in DATES {
        for time in TIMES {
            for zone in ZONES {
                let units = format!("hours since {date}{time}{zone}");
                check(units.as_bytes(), None, &[1.0]);
                check(units.as_bytes(), Some(b"julian"), &[1.0]);
                tried += 2;
            }
        }
    }
    assert!(tried > 10_000);
}

#[test]
fn extreme_values_give_steps_or_a_typed_error() {
    for head in ["ms", "seconds", "hours", "days"] {
        for base in ["1970-01-01", "200000-12-31", "1583-01-01"] {
            let units = format!("{head} since {base}");
            for calendar in [None, Some(&b"julian"[..])] {
                for a in VALUES {
                    check(units.as_bytes(), calendar, &[a]);
                    check(units.as_bytes(), calendar, &[a, f64::MAX]);
                    check(units.as_bytes(), calendar, &[f64::MIN, a]);
                }
                check(units.as_bytes(), calendar, &[f64::NAN]);
                check(units.as_bytes(), calendar, &[f64::INFINITY]);
                check(units.as_bytes(), calendar, &[f64::NEG_INFINITY]);
            }
        }
    }
}

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545F4914F6CDD1D)
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }
}

#[test]
fn random_unit_strings_give_steps_or_a_typed_error() {
    const ALPHABET: &[u8] = b"0123456789-+:. tTzZ?sinceshoursdayUTCGM\xc3\xa9\xe2\x82\xac\0\xff";
    let mut rng = Rng(0x9E3779B97F4A7C15);
    for _ in 0..20_000 {
        let mut units = b"hours since ".to_vec();
        if rng.below(8) == 0 {
            units.clear();
        }
        for _ in 0..rng.below(48) {
            units.push(ALPHABET[rng.below(ALPHABET.len())]);
        }
        check(&units, None, &[VALUES[rng.below(VALUES.len())]]);
    }
}

#[test]
fn the_reviews_seconds_field_is_a_typed_error() {
    let error = run(
        b"hours since 2000-01-01 00:00:999999999999999999999999999999",
        None,
        &[0.0],
    )
    .unwrap_err();
    assert!(matches!(error, VortexError::Refused { .. }), "{error}");
    assert!(error.to_string().contains("out of range"), "{error}");
}

#[test]
fn an_ordinary_file_still_reads() {
    let units = b"hours since 2000-01-01 00:00:00";
    assert_eq!(run(units, None, &[0.0, 1.0]).unwrap(), 2);
    // Seconds past 59 are an offset, as dt.plus takes them.
    assert_eq!(
        run(b"hours since 2000-01-01 00:00:90", None, &[0.0]).unwrap(),
        1
    );
}
