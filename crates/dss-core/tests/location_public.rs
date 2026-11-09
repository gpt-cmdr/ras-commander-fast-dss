//! Location reads on public fixtures. Expected values: `hecdss` 0.1.33
//! (`hec_dss_locationRetrieve`, HEC's `zlocationRetrieve`) and HEC Monolith 3.3.27
//! (`HecTimeSeries.read` location fields) reads of the same files.
//!
//! `synthetic_location7.dss` was written by hecdss 0.1.33 (HEC's Python package) from
//! synthetic values; `sample6.dss` is HEC's DSSVue sample.

use std::path::PathBuf;

use dss_core::dss6::Dss6File;
use dss_core::dss7::{Dss7File, LocationError};
use dss_core::location::{is_location_pathname, LOCATION};
use dss_core::{
    CoordinateSystem, HorizontalDatum, HorizontalUnits, Location, LocationSource, VerticalDatum,
    VerticalUnits,
};

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/public")
        .join(name)
}

fn open7() -> Dss7File<std::fs::File> {
    Dss7File::open(fixture("synthetic_location7.dss")).unwrap()
}

#[test]
fn catalog_lists_four_location_records() {
    let mut f = open7();
    let cat = f.catalog().unwrap();
    let locs: Vec<_> = cat
        .catalog
        .live()
        .filter(|e| e.record_type == LOCATION)
        .collect();
    assert_eq!(locs.len(), 4);
    assert!(locs.iter().all(|e| is_location_pathname(&e.pathname)));
}

#[test]
fn unknown_typed_datum_kept_beside_supplemental_datum() {
    let mut f = open7();
    let r = f
        .read_location(b"/P15 SYNTH/GAGE A/Location Info////")
        .unwrap();
    let l = &r.location;
    assert_eq!(l.source, LocationSource::Dss7Record);
    assert_eq!((l.x, l.y, l.z), (-97.25, 30.5, 612.5));
    assert!(l.descriptors_stored);
    assert_eq!(l.coordinate_system(), CoordinateSystem::LatLong);
    assert_eq!(l.coordinate_id, 0);
    assert_eq!(l.horizontal_units(), HorizontalUnits::DecimalDegrees);
    assert_eq!(l.horizontal_datum(), HorizontalDatum::Nad83);
    assert_eq!(l.vertical_units(), VerticalUnits::Feet);
    // Typed datum 0 is unset, never NAVD88; the text's NGVD29 is reported separately.
    assert_eq!(l.vertical_datum_code, 0);
    assert_eq!(l.vertical_datum(), VerticalDatum::Unset);
    assert_eq!(l.supplemental_vertical_datum().as_deref(), Some("NGVD29"));
    assert_eq!(l.time_zone_name.as_deref(), Some("America/Chicago"));
    assert_eq!(
        l.supplemental_text().as_deref(),
        Some("VerticalDatum:NGVD29;Operator:Synthetic;Note:unknown typed datum;")
    );
    assert_eq!(r.values1.len(), 16);
}

#[test]
fn other_codes_units_and_time_zones() {
    let mut f = open7();
    let b = f
        .read_location(b"/P15 SYNTH/GAGE B/Location Info////")
        .unwrap()
        .location;
    assert_eq!(b.coordinate_system(), CoordinateSystem::Utm);
    assert_eq!(b.coordinate_id, 15);
    assert_eq!(b.horizontal_units(), HorizontalUnits::Meters);
    assert_eq!(b.horizontal_datum(), HorizontalDatum::Wgs84);
    assert_eq!(b.vertical_units(), VerticalUnits::Meters);
    assert_eq!(b.vertical_datum(), VerticalDatum::Navd88);
    assert_eq!(b.time_zone_name.as_deref(), Some("UTC"));
    // Stored empty (one zero int in values2), not absent.
    assert_eq!(b.supplemental.as_deref(), Some(&b""[..]));

    let c = f
        .read_location(b"/p15 synth/gage c/location info////")
        .unwrap()
        .location;
    assert_eq!(c.coordinate_system(), CoordinateSystem::StatePlaneFips);
    assert_eq!(c.coordinate_id, 4203);
    assert_eq!(c.horizontal_datum(), HorizontalDatum::Nad27);
    assert_eq!(c.vertical_datum(), VerticalDatum::Ngvd29);
    // Empty time zone: zlocationStore() stores no words for it.
    assert_eq!(c.time_zone_name, None);
    assert_eq!(c.supplemental_text().as_deref(), Some("line one\nline two"));
    assert!(c.supplemental_items().is_empty());

    let d = f
        .read_location(b"/P15 SYNTH/GAGE D/Location Info////")
        .unwrap()
        .location;
    assert_eq!(d.coordinate_system(), CoordinateSystem::Local);
    assert_eq!(d.horizontal_units(), HorizontalUnits::Unset);
    assert_eq!(d.horizontal_datum(), HorizontalDatum::Local);
    assert_eq!(d.vertical_units(), VerticalUnits::Unset);
    assert_eq!(d.vertical_datum(), VerticalDatum::Unknown(9));
    assert!(d.vertical_datum().is_unknown());
    assert_eq!(d.time_zone_name.as_deref(), Some("PST"));
    assert_eq!(
        d.supplemental_items(),
        vec![
            ("verticalDatum".to_string(), "OTHER".to_string()),
            (
                "Key With Space".to_string(),
                "value: with colon".to_string()
            ),
        ]
    );
}

#[test]
fn associated_location_uses_a_and_b_parts() {
    let mut f = open7();
    let ts = f
        .read_associated_location(b"/P15 SYNTH/GAGE A/STAGE/01Jan2020/1Hour/P15/")
        .unwrap()
        .unwrap();
    assert_eq!(ts.location.pathname, b"/P15 SYNTH/GAGE A/Location Info////");
    // Any D part, and a paired-data record, share it.
    let any_d = f
        .read_associated_location(b"/P15 SYNTH/GAGE A/STAGE//1Hour/P15/")
        .unwrap()
        .unwrap();
    let pd = f
        .read_associated_location(b"/P15 SYNTH/GAGE A/STAGE-FLOW///P15/")
        .unwrap()
        .unwrap();
    assert_eq!(any_d.location, ts.location);
    assert_eq!(pd.location, ts.location);
    assert!(f
        .read_associated_location(b"/P15 SYNTH/NO LOCATION/FLOW/01Jan2020/1Hour/P15/")
        .unwrap()
        .is_none());
    assert!(matches!(
        f.read_associated_location(b"no slashes"),
        Err(LocationError::BadPathname { .. })
    ));
}

#[test]
fn typed_errors() {
    let mut f = open7();
    assert!(matches!(
        f.read_location(b"/P15 SYNTH/GAGE A/STAGE/01Jan2020/1Hour/P15/"),
        Err(LocationError::UnsupportedRecordType {
            record_type: 105,
            ..
        }) | Err(LocationError::UnsupportedRecordType {
            record_type: 100,
            ..
        })
    ));
    assert!(matches!(
        f.read_location(b"/P15 SYNTH/NOWHERE/Location Info////"),
        Err(LocationError::NotFound { .. })
    ));
}

/// DSS6: coordinates in regular TS headers. 149 of sample6's TS records store them,
/// matching Monolith on all six codes and the coordinate bits.
#[test]
fn dss6_ts_header_coordinates() {
    let f = Dss6File::open(fixture("sample6.dss")).unwrap();
    let rec = f
        .read_ts_record(b"/MISSISSIPPI/ST. LOUIS/FLOW/01JAN1861/1DAY/OBS/")
        .unwrap();
    let l =
        Location::from_dss6_ts(&rec.pathname, rec.coordinates, rec.coordinate_descriptors).unwrap();
    assert_eq!(l.source, LocationSource::Dss6TsHeader);
    assert_eq!((l.x, l.y, l.z), (-901047.2, 383744.4, 379.94));
    assert_eq!(l.coordinate_system(), CoordinateSystem::LatLong);
    assert_eq!(l.horizontal_units(), HorizontalUnits::DegreesMinutesSeconds);
    assert_eq!(l.horizontal_datum(), HorizontalDatum::Nad83);
    assert_eq!(l.vertical_units(), VerticalUnits::Feet);
    assert_eq!(l.vertical_datum(), VerticalDatum::Unset);
    assert!(l.time_zone_name.is_none() && l.supplemental.is_none());

    let cat = f.catalog().unwrap();
    let mut with = 0;
    for e in cat.live() {
        if (100..200).contains(&e.record_type) {
            if let Ok(r) = f.read_ts_record(&e.pathname) {
                with += usize::from(
                    Location::from_dss6_ts(&r.pathname, r.coordinates, r.coordinate_descriptors)
                        .is_some(),
                );
            }
        }
    }
    assert_eq!(with, 149);
}
