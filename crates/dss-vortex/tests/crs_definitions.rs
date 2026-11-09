//! WKT, datum refusals, GRIB earth shapes and envelopes. `crs_wkt_oracle.json` holds what GDAL
//! 3.2.1 inside Vortex 1.1.0 produced from the `SpatialReference` calls `WktFactory` makes.

use dss_vortex::crs::wkt::{SHG_ESRI_WKT, SHG_GDAL_WKT};
use dss_vortex::crs::{
    grib2_earth, reproject_envelope, vortex_grib_crs, CrsDef, Datum, DatumPolicy, Earth, Envelope,
    GribGrid, Projection, Transformer,
};
use dss_vortex::{Crs, NamedCrs, VortexError};
use std::fs;
use std::path::Path;

fn oracle() -> serde_json::Value {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/crs_wkt_oracle.json");
    serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
}

// Vortex's input WKT for the four P41 samples, from its meta.json.
const HRRR: &str = "PROJCS[\"Lambert Conformal Conic 2SP\",\n    GEOGCS[\"unknown\",\n        DATUM[\"unknown\",\n            SPHEROID[\"sphere\",6371229,0]],\n        PRIMEM[\"Greenwich\",0],\n        UNIT[\"degree\",0.0174532925199433,\n            AUTHORITY[\"EPSG\",\"9122\"]]],\n    PROJECTION[\"Lambert_Conformal_Conic_2SP\"],\n    PARAMETER[\"latitude_of_origin\",38.5],\n    PARAMETER[\"central_meridian\",262.5],\n    PARAMETER[\"standard_parallel_1\",38.5],\n    PARAMETER[\"standard_parallel_2\",38.5],\n    PARAMETER[\"false_easting\",0],\n    PARAMETER[\"false_northing\",0],\n    UNIT[\"Meter\",1],\n    AXIS[\"Easting\",EAST],\n    AXIS[\"Northing\",NORTH]]";
const WPC: &str = "PROJCS[\"Lambert Conformal Conic 2SP\",\n    GEOGCS[\"unknown\",\n        DATUM[\"unknown\",\n            SPHEROID[\"sphere\",6371200,0]],\n        PRIMEM[\"Greenwich\",0],\n        UNIT[\"degree\",0.0174532925199433,\n            AUTHORITY[\"EPSG\",\"9122\"]]],\n    PROJECTION[\"Lambert_Conformal_Conic_2SP\"],\n    PARAMETER[\"latitude_of_origin\",25],\n    PARAMETER[\"central_meridian\",265],\n    PARAMETER[\"standard_parallel_1\",25],\n    PARAMETER[\"standard_parallel_2\",25],\n    PARAMETER[\"false_easting\",0],\n    PARAMETER[\"false_northing\",0],\n    UNIT[\"Meter\",1],\n    AXIS[\"Easting\",EAST],\n    AXIS[\"Northing\",NORTH]]";
const STAGE4: &str = "PROJCS[\"unnamed\",\n    GEOGCS[\"unknown\",\n        DATUM[\"unknown\",\n            SPHEROID[\"sphere\",6371229,0]],\n        PRIMEM[\"Greenwich\",0],\n        UNIT[\"degree\",0.0174532925199433,\n            AUTHORITY[\"EPSG\",\"9122\"]]],\n    PROJECTION[\"Stereographic\"],\n    PARAMETER[\"latitude_of_origin\",90],\n    PARAMETER[\"central_meridian\",255],\n    PARAMETER[\"scale_factor\",0.933012701892219],\n    PARAMETER[\"false_easting\",0],\n    PARAMETER[\"false_northing\",0],\n    UNIT[\"Meter\",1],\n    AXIS[\"Easting\",EAST],\n    AXIS[\"Northing\",NORTH]]";
const MRMS: &str = "GEOGCS[\"unknown\",DATUM[\"unknown\",SPHEROID[\"sphere\",6371229,0]],PRIMEM[\"Greenwich\",0],UNIT[\"degree\",0.0174532925199433,AUTHORITY[\"EPSG\",\"9122\"]],AXIS[\"Latitude\",NORTH],AXIS[\"Longitude\",EAST]]";

#[test]
fn vortex_sample_wkt_parses_and_writes_back_byte_for_byte() {
    for wkt in [HRRR, WPC, STAGE4, MRMS] {
        let def = CrsDef::parse_wkt(wkt).unwrap();
        assert_eq!(def.datum, Datum::Sphere);
        assert_eq!(def.to_wkt(), wkt);
    }
}

#[test]
fn grib_rules_produce_vortex_wkt_for_each_sample() {
    let lambert = |latin, r| {
        vortex_grib_crs(
            GribGrid::Lambert {
                latin1: latin,
                latin2: latin,
                lov: if latin == 25.0 { 265.0 } else { 262.5 },
            },
            Earth::Sphere { radius: r },
        )
        .unwrap()
    };
    assert_eq!(
        lambert(38.5, 6371229.0).to_wkt(),
        HRRR,
        "HRRR, shape 6, file radius"
    );
    assert_eq!(
        lambert(25.0, 6371200.0).to_wkt(),
        WPC,
        "WPC, shape 1, file radius"
    );
    // Stage IV: shape 1, 6,371,200 m in the file; Stereographic's default radius in the WKT.
    let polar = GribGrid::Polar {
        lov: 255.0,
        lad: 60.0,
        north_pole: true,
    };
    let stage4 = vortex_grib_crs(polar, grib2_earth(1, 6371200.0, 0.0, 0.0)).unwrap();
    assert_eq!(stage4.to_wkt(), STAGE4);
    // MRMS: shape 2 in the file; a default-earth sphere in the WKT.
    let mrms = vortex_grib_crs(GribGrid::LatLon, grib2_earth(2, 0.0, 0.0, 0.0)).unwrap();
    assert_eq!(mrms.to_wkt(), MRMS);
}

#[test]
fn grib_earth_table_follows_netcdf_java() {
    let sphere = |radius| Earth::Sphere { radius };
    assert_eq!(grib2_earth(0, 0.0, 0.0, 0.0), sphere(6367470.0));
    assert_eq!(grib2_earth(1, 6371.2, 0.0, 0.0), sphere(6371200.0));
    assert_eq!(grib2_earth(6, 0.0, 0.0, 0.0), sphere(6371229.0));
    assert_eq!(grib2_earth(8, 0.0, 0.0, 0.0), sphere(6371200.0));
    assert_eq!(grib2_earth(99, 0.0, 0.0, 0.0), sphere(6371229.0));
    assert_eq!(
        grib2_earth(5, 0.0, 0.0, 0.0),
        Earth::Flattened {
            a: 6378137.0,
            rf: 298.257223563
        }
    );
    assert_eq!(
        grib2_earth(7, 0.0, 6378.137, 6356.752),
        Earth::Axes {
            major: 6378137.0,
            minor: 6356752.0
        }
    );
}

#[test]
fn ellipsoidal_grib_projections_are_refused_until_a_sample_exercises_them() {
    let e = grib2_earth(5, 0.0, 0.0, 0.0);
    for grid in [
        GribGrid::Polar {
            lov: 255.0,
            lad: 60.0,
            north_pole: true,
        },
        GribGrid::Lambert {
            latin1: 25.0,
            latin2: 25.0,
            lov: 265.0,
        },
    ] {
        assert!(matches!(
            vortex_grib_crs(grid, e),
            Err(VortexError::Unsupported { .. })
        ));
    }
}

#[test]
fn shg_forms_all_mean_shg() {
    assert_eq!(oracle()["shg_esri"].as_str().unwrap(), SHG_ESRI_WKT);
    assert_eq!(oracle()["shg_gdal"].as_str().unwrap(), SHG_GDAL_WKT);
    assert_eq!(CrsDef::parse_wkt(SHG_ESRI_WKT).unwrap(), CrsDef::shg());
    assert_eq!(CrsDef::parse_wkt(SHG_GDAL_WKT).unwrap(), CrsDef::shg());
    assert_eq!(
        CrsDef::from_frame(&Crs::Named(NamedCrs::Shg)).unwrap(),
        CrsDef::shg()
    );
    assert_eq!(
        CrsDef::from_frame(&Crs::Named(NamedCrs::Hrap)).unwrap(),
        CrsDef::hrap()
    );
}

#[test]
fn writer_matches_gdal_for_each_generated_form() {
    let sphere = |projection| CrsDef {
        datum: Datum::Sphere,
        a: 6371229.0,
        rf: 0.0,
        projection,
    };
    let spheroid = |projection| CrsDef {
        datum: Datum::Other("unknown".into()),
        a: 6378160.0,
        rf: 297.0,
        projection,
    };
    let lcc = Projection::Lambert {
        lat_1: 33.0,
        lat_2: 45.0,
        lat_0: 40.0,
        lon_0: 262.5,
        k_0: 1.0,
        x_0: 0.0,
        y_0: 0.0,
    };
    let aea = Projection::Albers {
        lat_1: 29.5,
        lat_2: 45.5,
        lat_0: 23.0,
        lon_0: -96.0,
        x_0: 0.0,
        y_0: 0.0,
    };
    let st = Projection::Stereographic {
        lat_0: 90.0,
        lon_0: 255.0,
        k_0: 0.933012701892219,
        lat_ts: None,
        x_0: 0.0,
        y_0: 0.0,
    };
    let ps = Projection::Stereographic {
        lat_0: 90.0,
        lon_0: -105.0,
        k_0: 1.0,
        lat_ts: Some(60.0),
        x_0: 0.0,
        y_0: 0.0,
    };
    let south = Projection::Stereographic {
        lat_0: -90.0,
        lon_0: -105.0,
        k_0: 1.0,
        lat_ts: Some(-60.0),
        x_0: 0.0,
        y_0: 0.0,
    };
    let o = oracle();
    for (key, p) in [
        ("polarsouth", south),
        ("latlon", Projection::LongLat),
        ("lambert", lcc),
        ("albers", aea),
        ("stereographic", st),
        ("polar", ps),
    ] {
        for (suffix, def) in [("sphere", sphere(p)), ("spheroid", spheroid(p))] {
            let name = format!("{key}_{suffix}");
            let want = o[name.as_str()].as_str().unwrap();
            assert_eq!(def.to_wkt(), want, "{name}");
            assert_eq!(CrsDef::parse_wkt(want).unwrap(), def, "{name} parse");
        }
    }
}

fn refused(result: Result<Transformer, VortexError>) -> bool {
    matches!(result, Err(VortexError::Refused { .. }))
}

const NAD27: &str = "GEOGCS[\"NAD27\",DATUM[\"North_American_Datum_1927\",SPHEROID[\"Clarke 1866\",6378206.4,294.978698213898]],PRIMEM[\"Greenwich\",0],UNIT[\"degree\",0.0174532925199433]]";

#[test]
fn datums_outside_v9_are_refused_unless_the_caller_opts_in() {
    let nad27 = CrsDef::parse_wkt(NAD27).unwrap();
    assert!(matches!(nad27.datum, Datum::Other(_)));
    assert!(refused(Transformer::new(
        &nad27,
        &CrsDef::shg(),
        DatumPolicy::Refuse
    )));
    assert!(refused(Transformer::new(
        &CrsDef::shg(),
        &nad27,
        DatumPolicy::Refuse
    )));
    assert!(Transformer::new(&nad27, &CrsDef::shg(), DatumPolicy::AssumeNoShift).is_ok());

    let shifted = NAD27.replace(
        "294.978698213898]]",
        "294.978698213898],TOWGS84[-8,160,176,0,0,0,0]]",
    );
    let shifted = CrsDef::parse_wkt(&shifted).unwrap();
    assert!(matches!(shifted.datum, Datum::Shifted(_)));
    assert!(refused(Transformer::new(
        &shifted,
        &CrsDef::shg(),
        DatumPolicy::Refuse
    )));

    let unnamed_iau = "GEOGCS[\"g\",DATUM[\"unknown\",SPHEROID[\"s\",6378160,297]],PRIMEM[\"Greenwich\",0],UNIT[\"degree\",0.0174532925199433]]";
    let d = CrsDef::parse_wkt(unnamed_iau).unwrap();
    assert!(refused(Transformer::new(
        &d,
        &CrsDef::shg(),
        DatumPolicy::Refuse
    )));

    for (wkt, datum) in [
        (
            "GEOGCS[\"g\",DATUM[\"D_WGS_1984\",SPHEROID[\"s\",6378137,298.257223563]],PRIMEM[\"Greenwich\",0],UNIT[\"degree\",0.0174532925199433]]",
            Datum::Wgs84,
        ),
        (
            "GEOGCS[\"g\",DATUM[\"unknown\",SPHEROID[\"s\",6378137,298.257222101]],PRIMEM[\"Greenwich\",0],UNIT[\"degree\",0.0174532925199433]]",
            Datum::Nad83,
        ),
        (
            "GEOGCS[\"g\",DATUM[\"NAD83\",SPHEROID[\"s\",6378137,298.257222101],TOWGS84[0,0,0,0,0,0,0]],PRIMEM[\"Greenwich\",0],UNIT[\"degree\",0.0174532925199433]]",
            Datum::Nad83,
        ),
        (
            "GEOGCS[\"g\",DATUM[\"unknown\",SPHEROID[\"s\",6370000,0]],PRIMEM[\"Greenwich\",0],UNIT[\"degree\",0.0174532925199433]]",
            Datum::Sphere,
        ),
    ] {
        assert_eq!(CrsDef::parse_wkt(wkt).unwrap().datum, datum, "{wkt}");
    }
}

#[test]
fn unsupported_definitions_are_typed_refusals() {
    let unsupported = [
        // WKT2.
        "GEOGCRS[\"x\",DATUM[\"y\",ELLIPSOID[\"z\",6378137,298.25]]]",
        "PROJCS[\"p\",GEOGCS[\"g\",DATUM[\"NAD83\",SPHEROID[\"s\",6378137,298.257222101]],PRIMEM[\"Greenwich\",0],UNIT[\"degree\",0.0174532925199433]],PROJECTION[\"Mercator_1SP\"],UNIT[\"Meter\",1]]",
        "PROJCS[\"p\",GEOGCS[\"g\",DATUM[\"NAD83\",SPHEROID[\"s\",6378137,298.257222101]],PRIMEM[\"Greenwich\",0],UNIT[\"degree\",0.0174532925199433]],PROJECTION[\"Albers\"],PARAMETER[\"standard_parallel_1\",29.5],PARAMETER[\"standard_parallel_2\",45.5],PARAMETER[\"latitude_of_origin\",23],PARAMETER[\"central_meridian\",-96],PARAMETER[\"false_easting\",0],PARAMETER[\"false_northing\",0],UNIT[\"foot\",0.3048]]",
        "GEOGCS[\"g\",DATUM[\"NAD83\",SPHEROID[\"s\",6378137,298.257222101]],PRIMEM[\"Paris\",2.33],UNIT[\"degree\",0.0174532925199433]]",
        "GEOGCS[\"g\",DATUM[\"NAD83\",SPHEROID[\"s\",6378137,298.257222101]],PRIMEM[\"Greenwich\",0],UNIT[\"grad\",0.015707963267949]]",
        "PROJCS[\"p\",GEOGCS[\"g\",DATUM[\"NAD83\",SPHEROID[\"s\",6378137,298.257222101]],PRIMEM[\"Greenwich\",0],UNIT[\"degree\",0.0174532925199433]],PROJECTION[\"Polar_Stereographic\"],PARAMETER[\"latitude_of_origin\",60],PARAMETER[\"central_meridian\",0],PARAMETER[\"scale_factor\",0.9],PARAMETER[\"false_easting\",0],PARAMETER[\"false_northing\",0],UNIT[\"Meter\",1]]",
    ];
    for wkt in unsupported {
        assert!(
            matches!(CrsDef::parse_wkt(wkt), Err(VortexError::Unsupported { .. })),
            "{wkt}"
        );
    }
    assert!(matches!(
        CrsDef::from_frame(&Crs::Undefined),
        Err(VortexError::Refused { .. })
    ));
    assert!(matches!(
        CrsDef::from_frame(&Crs::Proj("+proj=aea".into())),
        Err(VortexError::Unsupported { .. })
    ));
}

#[test]
fn parameters_proj_rejects_are_refused() {
    let conic = CrsDef {
        datum: Datum::Sphere,
        a: 6371229.0,
        rf: 0.0,
        projection: Projection::Lambert {
            lat_1: 30.0,
            lat_2: -30.0,
            lat_0: 0.0,
            lon_0: 0.0,
            k_0: 1.0,
            x_0: 0.0,
            y_0: 0.0,
        },
    };
    assert!(refused(Transformer::new(
        &conic,
        &CrsDef::shg(),
        DatumPolicy::Refuse
    )));
    let negative_axis = CrsDef {
        a: -1.0,
        ..CrsDef::sphere_lonlat(1.0)
    };
    assert!(refused(Transformer::new(
        &negative_axis,
        &CrsDef::shg(),
        DatumPolicy::Refuse
    )));
}

#[test]
fn hostile_wkt_never_panics() {
    let deep = "A[".repeat(5000);
    let long = format!("GEOGCS[\"{}\"]", "x".repeat(100_000));
    let cases = [
        "",
        "[",
        "]",
        "GEOGCS",
        "GEOGCS[",
        "GEOGCS[\"",
        "GEOGCS[]",
        "PROJCS[\"p\"]",
        "GEOGCS[\"g\",DATUM[\"d\",SPHEROID[\"s\",nan,inf]],UNIT[\"degree\",0.01]]",
        "GEOGCS[\"g\",DATUM[\"d\",SPHEROID[\"s\",1e999,0]],UNIT[\"degree\",0.01]]",
        "GEOGCS[\"\u{e9}\u{1F600}\",DATUM]",
        "GEOGCS[\"g\"] trailing",
        deep.as_str(),
        long.as_str(),
    ];
    for wkt in cases {
        assert!(CrsDef::parse_wkt(wkt).is_err(), "{wkt:.60}");
    }
    let base = SHG_GDAL_WKT.as_bytes();
    for cut in 0..base.len() {
        let _ = CrsDef::parse_wkt(&String::from_utf8_lossy(&base[..cut]));
    }
}

#[test]
fn envelope_is_the_bounding_box_of_the_corners_only() {
    let ll = CrsDef::sphere_lonlat(6371229.0);
    let env = Envelope {
        min_x: -110.0,
        max_x: -90.0,
        min_y: 30.0,
        max_y: 45.0,
    };
    let got = reproject_envelope(env, &ll, &CrsDef::shg(), DatumPolicy::Refuse).unwrap();
    let t = Transformer::new(&ll, &CrsDef::shg(), DatumPolicy::Refuse).unwrap();
    let c: Vec<(f64, f64)> = [(-110.0, 30.0), (-110.0, 45.0), (-90.0, 45.0), (-90.0, 30.0)]
        .iter()
        .map(|p| t.transform(p.0, p.1).unwrap())
        .collect();
    assert_eq!(got.max_y, c.iter().map(|p| p.1).fold(f64::MIN, f64::max));
    assert_eq!(got.min_x, c.iter().map(|p| p.0).fold(f64::MAX, f64::min));
    // The southern edge sags below its corners at the central meridian; a corner box misses it.
    assert!(t.transform(-96.0, 30.0).unwrap().1 < got.min_y);

    let bad = Envelope {
        min_x: -100.0,
        max_x: -90.0,
        min_y: 80.0,
        max_y: 100.0,
    };
    assert!(matches!(
        reproject_envelope(bad, &ll, &CrsDef::shg(), DatumPolicy::Refuse),
        Err(VortexError::Refused { .. })
    ));
}

// Each form below was once accepted silently. `sub` makes it from a valid WKT,
// so a failure to refuse cannot be a typo in a whole hand-written string.
fn sub(wkt: &str, from: &str, to: &str) -> String {
    assert!(wkt.contains(from), "{from} not in {wkt}");
    wkt.replacen(from, to, 1)
}

fn assert_refused(wkt: &str) {
    assert!(
        matches!(
            CrsDef::parse_wkt(wkt),
            Err(VortexError::Unsupported { .. } | VortexError::Decode { .. })
        ),
        "accepted: {wkt}"
    );
}

#[test]
fn geographic_axes_other_than_north_and_east_are_refused() {
    let west = sub(MRMS, "AXIS[\"Longitude\",EAST]", "AXIS[\"Longitude\",WEST]");
    assert_refused(&west);
    assert_refused(&sub(
        MRMS,
        "AXIS[\"Latitude\",NORTH]",
        "AXIS[\"Latitude\",SOUTH]",
    ));
    assert_refused(&sub(MRMS, ",AXIS[\"Longitude\",EAST]", ""));
    assert_refused(&sub(
        MRMS,
        "AXIS[\"Longitude\",EAST]",
        "AXIS[\"Longitude\",\"EAST\"]",
    ));
    // Inside a projected CRS too.
    let nested = sub(
        SHG_GDAL_WKT,
        "UNIT[\"Degree\",0.0174532925199433]]",
        "UNIT[\"Degree\",0.0174532925199433],AXIS[\"Lat\",NORTH],AXIS[\"Lon\",WEST]]",
    );
    assert_refused(&nested);
    // Both supported orders still mean x is longitude.
    assert!(CrsDef::parse_wkt(MRMS).is_ok());
    let swapped = sub(
        MRMS,
        "AXIS[\"Latitude\",NORTH],AXIS[\"Longitude\",EAST]",
        "AXIS[\"Longitude\",EAST],AXIS[\"Latitude\",NORTH]",
    );
    assert_eq!(
        CrsDef::parse_wkt(&swapped).unwrap(),
        CrsDef::parse_wkt(MRMS).unwrap()
    );
}

#[test]
fn invalid_duplicate_or_conflicting_parameters_are_refused() {
    let fe = "PARAMETER[\"false_easting\",0]";
    for bad in ["\"oops\"", "\"0\"", "nan", "inf", "1e999", "oops"] {
        assert_refused(&sub(
            SHG_GDAL_WKT,
            fe,
            &format!("PARAMETER[\"false_easting\",{bad}]"),
        ));
    }
    assert_refused(&sub(SHG_GDAL_WKT, fe, &format!("{fe},{fe}")));
    assert_refused(&sub(SHG_GDAL_WKT, fe, ""));
    assert_refused(&sub(
        SHG_GDAL_WKT,
        fe,
        &format!("{fe},PARAMETER[\"central_meridian\",0]"),
    ));
    assert_refused(&sub(
        SHG_GDAL_WKT,
        fe,
        &format!("{fe},PARAMETER[\"latitude_of_origin\",23]"),
    ));
    assert_refused(&sub(
        SHG_GDAL_WKT,
        fe,
        &format!("{fe},PARAMETER[\"azimuth\",1]"),
    ));
    assert_refused(&sub(SHG_GDAL_WKT, fe, "PARAMETER[\"false_easting\",0,1]"));
    assert_refused(&sub(SHG_GDAL_WKT, fe, "PARAMETER[\"false_easting\"]"));
    // An unknown alias on another method.
    assert_refused(&sub(
        HRRR,
        "\"central_meridian\"",
        "\"longitude_of_center\"",
    ));
    // Lambert 2SP missing its second parallel is not the same as a tangent cone stated outright.
    assert_refused(&sub(
        HRRR,
        "    PARAMETER[\"standard_parallel_2\",38.5],\n",
        "",
    ));
}

#[test]
fn duplicate_or_unknown_elements_are_refused() {
    for (from, to) in [
        (
            "PROJECTION[\"Albers_Conic_Equal_Area\"]",
            "PROJECTION[\"Albers_Conic_Equal_Area\"],PROJECTION[\"Mercator\"]",
        ),
        (
            "UNIT[\"metre\",1,AUTHORITY[\"EPSG\",\"9001\"]]",
            "UNIT[\"metre\",1],UNIT[\"foot\",0.3048]",
        ),
        ("UNIT[\"metre\",1,", "UNIT[\"foot\",1,"),
        (
            "PRIMEM[\"Greenwich\",0]",
            "PRIMEM[\"Greenwich\",0],PRIMEM[\"Paris\",2.33]",
        ),
        ("PRIMEM[\"Greenwich\",0]", "PRIMEM[\"Paris\",0]"),
        ("PRIMEM[\"Greenwich\",0],", ""),
        (
            "UNIT[\"Degree\",0.0174532925199433]",
            "UNIT[\"grad\",0.0174532925199433]",
        ),
        (
            "PROJECTION[\"Albers_Conic_Equal_Area\"]",
            "VERTCS[\"v\"],PROJECTION[\"Albers_Conic_Equal_Area\"]",
        ),
        (
            "PROJECTION[\"Albers_Conic_Equal_Area\"]",
            "EXTENSION[\"PROJ4\",\"+proj=longlat\"],PROJECTION[\"Albers_Conic_Equal_Area\"]",
        ),
        ("6378137,298.257222101,", "6378137,298.257222101,1,"),
        ("6378137,298.257222101,", "6378137,,298.257222101,"),
        ("[\"NAD83\",DATUM", "[\"NAD83\",\"second name\",DATUM"),
    ] {
        assert_refused(&sub(SHG_GDAL_WKT, from, to));
    }
    assert_refused("GEOGCS[\"g\",DATUM[\"NAD83\",SPHEROID[\"s\",6378137,298.257222101]),PRIMEM[\"Greenwich\",0],UNIT[\"degree\",0.0174532925199433]]");
    assert_refused("GEOGCS[\"g\",DATUM[\"d\",SPHEROID[\"s\",6378137,0.5]],PRIMEM[\"Greenwich\",0],UNIT[\"degree\",0.0174532925199433]]");
}

fn nad83(datum_extra: &str) -> String {
    format!("GEOGCS[\"g\",DATUM[\"NAD83\",SPHEROID[\"s\",6378137,298.257222101]{datum_extra}],PRIMEM[\"Greenwich\",0],UNIT[\"degree\",0.0174532925199433]]")
}

#[test]
fn datum_refusals_cannot_be_sidestepped_by_the_text() {
    let shg = CrsDef::shg();
    let refused_datum = |wkt: &str| match CrsDef::parse_wkt(wkt) {
        Err(_) => true,
        Ok(d) => refused(Transformer::new(&d, &shg, DatumPolicy::Refuse)),
    };
    // Two shift nodes, the first of them zero.
    assert!(refused_datum(&nad83(",TOWGS84[0,0,0],TOWGS84[1,2,3]")));
    assert!(refused_datum(&nad83(
        ",TOWGS84[0,0,0,0,0,0,0],TOWGS84[1,2,3,0,0,0,0]"
    )));
    // Shifts that are not plain numbers, or the wrong number of them.
    assert!(refused_datum(&nad83(",TOWGS84[\"1\",0,0]")));
    assert!(refused_datum(&nad83(",TOWGS84[oops,0,0]")));
    assert!(refused_datum(&nad83(",TOWGS84[0,0]")));
    // A second datum, spheroid or extension.
    assert!(refused_datum(&nad83(
        ",SPHEROID[\"c\",6378206.4,294.978698213898]"
    )));
    assert!(refused_datum(&nad83(
        ",EXTENSION[\"PROJ4_GRIDS\",\"conus\"]"
    )));
    assert!(refused_datum("GEOGCS[\"g\",DATUM[\"North_American_Datum_1927\",SPHEROID[\"s\",6378137,298.257222101]],DATUM[\"NAD83\",SPHEROID[\"s\",6378137,298.257222101]],PRIMEM[\"Greenwich\",0],UNIT[\"degree\",0.0174532925199433]]"));
    // A name that disagrees with the ellipsoid.
    assert!(refused_datum("GEOGCS[\"g\",DATUM[\"NAD83\",SPHEROID[\"Clarke 1866\",6378206.4,294.978698213898]],PRIMEM[\"Greenwich\",0],UNIT[\"degree\",0.0174532925199433]]"));
    assert!(refused_datum("GEOGCS[\"g\",DATUM[\"WGS_1984\",SPHEROID[\"s\",6378137,298.257222101]],PRIMEM[\"Greenwich\",0],UNIT[\"degree\",0.0174532925199433]]"));
    // A real datum's name on a sphere.
    assert!(refused_datum("GEOGCS[\"g\",DATUM[\"North_American_Datum_1927\",SPHEROID[\"s\",6371229,0]],PRIMEM[\"Greenwich\",0],UNIT[\"degree\",0.0174532925199433]]"));
    // An authority that disagrees with an accepted name or an unnamed datum.
    assert!(refused_datum(&nad83(",AUTHORITY[\"EPSG\",\"6267\"]")));
    assert!(refused_datum("GEOGCS[\"g\",DATUM[\"unknown\",SPHEROID[\"s\",6378137,298.257222101],AUTHORITY[\"EPSG\",\"6267\"]],PRIMEM[\"Greenwich\",0],UNIT[\"degree\",0.0174532925199433]]"));
    assert!(refused_datum("GEOGCS[\"g\",DATUM[\"unknown\",SPHEROID[\"s\",6371229,0],AUTHORITY[\"EPSG\",\"6267\"]],PRIMEM[\"Greenwich\",0],UNIT[\"degree\",0.0174532925199433]]"));
    assert!(refused_datum("GEOGCS[\"g\",DATUM[\"NAD83\",SPHEROID[\"s\",6378137,298.257222101]],PRIMEM[\"Greenwich\",0],UNIT[\"degree\",0.0174532925199433],AUTHORITY[\"EPSG\",\"4267\"]]"));
    // The accepted forms stay accepted.
    assert!(!refused_datum(&nad83("")));
    assert!(!refused_datum(&nad83(",TOWGS84[0,0,0,0,0,0,0]")));
    assert!(!refused_datum(&nad83(",AUTHORITY[\"EPSG\",\"6269\"]")));
    assert!(!refused_datum(SHG_GDAL_WKT));
}

#[test]
fn writing_a_refused_datum_does_not_make_it_accepted() {
    let shg = CrsDef::shg();
    for datum in [
        Datum::Shifted("unknown".into()),
        Datum::Other("North_American_Datum_1927".into()),
    ] {
        let def = CrsDef {
            datum,
            a: 6378137.0,
            rf: 298.257222101,
            projection: Projection::LongLat,
        };
        assert!(refused(Transformer::new(&def, &shg, DatumPolicy::Refuse)));
        let again = CrsDef::parse_wkt(&def.to_wkt()).unwrap();
        assert!(
            refused(Transformer::new(&again, &shg, DatumPolicy::Refuse)),
            "{:?}",
            again.datum
        );
    }
}
