//! Hostile warp inputs: no panic, and no allocation beyond the stated limits. Deterministic.
//! `P44_FUZZ_ITERS` sets a longer run; debug builds also catch integer overflow.

use dss_vortex::crs::{CrsDef, Datum, DatumPolicy, Envelope, Projection};
use dss_vortex::frame::GeoTransform;
use dss_vortex::warp::{Align, Method, SourceGrid, WarpEnvelope, WarpLimits, WarpPlan, WarpSpec};

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn pick<T: Copy>(&mut self, from: &[T]) -> T {
        from[(self.next() % from.len() as u64) as usize]
    }
    fn wild(&mut self) -> f64 {
        match self.next() % 5 {
            0 => f64::from_bits(self.next()),
            1 => self.pick(&[
                0.0,
                -0.0,
                f64::NAN,
                f64::INFINITY,
                f64::NEG_INFINITY,
                f64::MAX,
                f64::MIN_POSITIVE,
                5e-324,
                1e300,
                -1e300,
                1e-300,
            ]),
            _ => (self.next() % 2_000_000) as f64 / 1000.0 - 1000.0,
        }
    }
    fn angle(&mut self) -> f64 {
        if self.next().is_multiple_of(8) {
            self.wild()
        } else {
            (self.next() % 18000) as f64 / 100.0 - 90.0
        }
    }
}

fn crs(r: &mut Rng) -> CrsDef {
    let projection = match r.next() % 5 {
        0 => Projection::LongLat,
        1 => Projection::Albers {
            lat_1: r.angle(),
            lat_2: r.angle(),
            lat_0: r.angle(),
            lon_0: r.angle(),
            x_0: 0.0,
            y_0: 0.0,
        },
        2 => Projection::Lambert {
            lat_1: r.angle(),
            lat_2: r.angle(),
            lat_0: r.angle(),
            lon_0: r.angle(),
            k_0: 1.0,
            x_0: 0.0,
            y_0: 0.0,
        },
        3 => Projection::Stereographic {
            lat_0: r.pick(&[90.0, -90.0, 45.0, 0.0]),
            lon_0: r.angle(),
            k_0: 0.933,
            lat_ts: None,
            x_0: 0.0,
            y_0: 0.0,
        },
        _ => return CrsDef::shg(),
    };
    CrsDef {
        datum: Datum::Sphere,
        a: if r.next().is_multiple_of(16) {
            r.wild()
        } else {
            6371229.0
        },
        rf: 0.0,
        projection,
    }
}

fn one_case(r: &mut Rng) -> bool {
    let (nx, ny) = ((r.next() % 40 + 1) as usize, (r.next() % 40 + 1) as usize);
    let scale = r.pick(&[0.01, 0.25, 1000.0, 4762.5, 1e-9, 1e7]);
    let transform = if r.next().is_multiple_of(4) {
        GeoTransform {
            origin_x: r.wild(),
            dx: r.wild(),
            row_rotation: if r.next().is_multiple_of(2) {
                0.0
            } else {
                r.wild()
            },
            origin_y: r.wild(),
            column_rotation: if r.next().is_multiple_of(2) {
                0.0
            } else {
                r.wild()
            },
            dy: r.wild(),
        }
    } else {
        GeoTransform::axis_aligned(
            (r.next() % 400) as f64 - 200.0,
            scale,
            (r.next() % 200) as f64 - 100.0,
            if r.next().is_multiple_of(2) {
                -scale
            } else {
                scale
            },
        )
    };
    let source = SourceGrid {
        nx,
        ny,
        transform,
        crs: crs(r),
    };
    let mut spec = WarpSpec::new(r.pick(&[Method::Near, Method::Bilinear, Method::Average]));
    spec.target = (!r.next().is_multiple_of(8)).then(|| crs(r));
    spec.cell_size = match r.next() % 4 {
        0 => None,
        1 => Some(r.wild()),
        _ => Some(r.pick(&[2000.0, 10000.0, 500.0, 0.5, 1e6])),
    };
    if !r.next().is_multiple_of(4) {
        let (cx, cy) = (
            (r.next() % 8_000_000) as f64 - 4_000_000.0,
            (r.next() % 8_000_000) as f64 - 4_000_000.0,
        );
        let size = r.pick(&[1000.0, 50_000.0, 500_000.0, 5e7]);
        spec.envelope = Some(WarpEnvelope {
            envelope: if r.next().is_multiple_of(8) {
                Envelope {
                    min_x: r.wild(),
                    max_x: r.wild(),
                    min_y: r.wild(),
                    max_y: r.wild(),
                }
            } else {
                Envelope {
                    min_x: cx,
                    max_x: cx + size,
                    min_y: cy,
                    max_y: cy + size,
                }
            },
            crs: (r.next().is_multiple_of(2)).then(CrsDef::shg),
        });
    }
    spec.align = r.pick(&[Align::Refuse, Align::Vortex]);
    spec.datum_policy = r.pick(&[DatumPolicy::default(), DatumPolicy::AssumeNoShift]);
    spec.threads = r.pick(&[1, 1, 2]);
    spec.limits = WarpLimits {
        max_cells: 4096,
        max_chunks: 64,
        ..WarpLimits::default()
    };
    let rotated = source.transform.row_rotation != 0.0 || source.transform.column_rotation != 0.0;
    let Ok(plan) = WarpPlan::new(&source, &spec) else {
        return false;
    };
    assert!(!rotated, "a rotated source must be refused");
    assert!((plan.nx() * plan.ny()) as u64 <= spec.limits.max_cells);
    let cells: Vec<f32> = (0..nx * ny)
        .map(|_| match r.next() % 8 {
            0 => f32::NAN,
            1 => -1.0,
            2 => f32::from_bits(r.next() as u32),
            _ => (r.next() % 1000) as f32 / 7.0,
        })
        .collect();
    let nodata = r.pick(&[-1.0, f32::NAN, 0.0, f32::MAX, f32::INFINITY]);
    let out = plan.warp_cells(&cells, nodata).unwrap();
    assert_eq!(out.len(), plan.nx() * plan.ny());
    assert!(plan.warp_cells(&cells[1..], nodata).is_err());
    true
}

#[test]
fn hostile_grids_and_options_never_panic() {
    let iters = std::env::var("P44_FUZZ_ITERS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(4000u64);
    let mut r = Rng(0x9e37_79b9_7f4a_7c15);
    let mut warped = 0u64;
    for _ in 0..iters {
        warped += u64::from(one_case(&mut r));
    }
    // A fuzz run that mostly stops at the first refusal would prove little about the kernels.
    assert!(
        warped * 10 >= iters,
        "only {warped} of {iters} reached a kernel"
    );
}

#[test]
fn an_unrepresentable_source_is_refused() {
    let source = SourceGrid {
        nx: usize::MAX,
        ny: 3,
        transform: GeoTransform::axis_aligned(0.0, 1.0, 0.0, -1.0),
        crs: CrsDef::shg(),
    };
    assert!(WarpPlan::new(&source, &WarpSpec::new(Method::Near)).is_err());
}
