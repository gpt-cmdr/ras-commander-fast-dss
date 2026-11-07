//! Location metadata (packet P15; decision D37 extends D29): DSS7 location records
//! (type 20) and the coordinates DSS6 stores in time-series internal headers.
//!
//! Format rules: `docs/format/dss7.md` §11 and `docs/format/dss6.md` §13. Sources:
//!
//! | Item | HEC source |
//! |---|---|
//! | Field list and code tables | `heclib_c/src/headers/zStructLocation.h`; `Public/zlocationRetrieve.c` header comment |
//! | Vertical datum codes and names | `headers/verticalDatum.h` (`IVERTICAL_DATUM_*`, `CVERTICAL_DATUM_*`) |
//! | Location pathname `/A/B/Location Info////` | `Public/zlocationPath.c:zlocationPath()` |
//! | Supplemental `Keyword:Item;` convention | `Public/zlocationRetrieve.c` header comment; `Utilities/verticalDatum.c:extractFromDelimitedString()` |
//! | DSS6 coordinates in TS headers | `heclib_f/src/zrrtsi.f` lines 588–601, `heclib_f/src/zritsi6.f` lines 737–749; `Dss6/ztsRetrieveReg6.c` lines 239–248 |
//! | DSS6 supplemental text from the TS user header (P27) | Monolith `supplementalInfo` (`javaHeclib/src/Hec_ztsRetrieve.c`, "User header (supplemental)"); HEC's C library leaves it empty (`Dss6/ztsRetrieveReg6.c`) |
//!
//! Codes are kept exactly as stored. The typed accessors decode them; a code outside
//! HEC's table decodes to an `Unknown(code)` variant, never to a guessed value. An unset
//! vertical datum (code 0) is [`VerticalDatum::Unset`]; it is never treated as NAVD88,
//! and a datum named in the supplemental text is reported separately
//! ([`Location::supplemental_vertical_datum`]), never merged into the typed field.

/// `DATA_TYPE_LOCATION` (`headers/zdataTypeDescriptions.h`).
pub const LOCATION: i32 = 20;

/// `LOCATION_PATH` (`Public/zlocationPath.c`): what replaces the C..F parts.
pub const LOCATION_PATH_SUFFIX: &[u8] = b"Location Info////";

/// `zlocationPath()`: the location pathname for any pathname of a dataset,
/// `/A/B/` of `pathname` followed by `Location Info////`.
///
/// `zpathnamePartPositions()` gives the start of the C part (after the third `/`).
/// Returns `None` when `pathname` does not start with `/` or has fewer than three `/`
/// (HEC's positions are undefined there).
pub fn location_pathname(pathname: &[u8]) -> Option<Vec<u8>> {
    if pathname.first() != Some(&b'/') {
        return None;
    }
    let third = pathname
        .iter()
        .enumerate()
        .filter(|(_, &b)| b == b'/')
        .nth(2)
        .map(|(i, _)| i)?;
    let mut out = pathname[..=third].to_vec();
    out.extend_from_slice(LOCATION_PATH_SUFFIX);
    Some(out)
}

/// True when the C, D, E and F parts of `pathname` are those of a location record
/// (`Location Info`, then three empty parts). The C part compares case-insensitively,
/// as DSS7 lookups do (`zpathnameCompare.c`).
pub fn is_location_pathname(pathname: &[u8]) -> bool {
    location_pathname(pathname).is_some_and(|p| p.eq_ignore_ascii_case(pathname))
}

/// `coordinateSystem` (`zStructLocation.h`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CoordinateSystem {
    /// 0: no coordinates set.
    Unset,
    /// 1: latitude/longitude.
    LatLong,
    /// 2: State Plane, FIPS zone numbers.
    StatePlaneFips,
    /// 3: State Plane, ADS zone numbers.
    StatePlaneAds,
    /// 4: UTM.
    Utm,
    /// 5: local (other).
    Local,
    /// Any other stored code.
    Unknown(i32),
}

impl CoordinateSystem {
    pub fn from_code(c: i32) -> Self {
        match c {
            0 => Self::Unset,
            1 => Self::LatLong,
            2 => Self::StatePlaneFips,
            3 => Self::StatePlaneAds,
            4 => Self::Utm,
            5 => Self::Local,
            other => Self::Unknown(other),
        }
    }

    /// Name used by the Python layer; `None` for unset and unknown codes.
    pub fn name(self) -> Option<&'static str> {
        Some(match self {
            Self::LatLong => "LAT_LONG",
            Self::StatePlaneFips => "STATE_PLANE_FIPS",
            Self::StatePlaneAds => "STATE_PLANE_ADS",
            Self::Utm => "UTM",
            Self::Local => "LOCAL",
            Self::Unset | Self::Unknown(_) => return None,
        })
    }
}

/// `horizontalUnits` (`zStructLocation.h`). The `zlocationRetrieve()` comment also lists
/// 5 "Local (other)", which the struct header does not; 5 decodes to `Unknown(5)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HorizontalUnits {
    /// 0: unspecified.
    Unset,
    /// 1.
    Feet,
    /// 2.
    Meters,
    /// 3.
    DecimalDegrees,
    /// 4.
    DegreesMinutesSeconds,
    Unknown(i32),
}

impl HorizontalUnits {
    pub fn from_code(c: i32) -> Self {
        match c {
            0 => Self::Unset,
            1 => Self::Feet,
            2 => Self::Meters,
            3 => Self::DecimalDegrees,
            4 => Self::DegreesMinutesSeconds,
            other => Self::Unknown(other),
        }
    }

    pub fn name(self) -> Option<&'static str> {
        Some(match self {
            Self::Feet => "FEET",
            Self::Meters => "METERS",
            Self::DecimalDegrees => "DECIMAL_DEGREES",
            Self::DegreesMinutesSeconds => "DEGREES_MINUTES_SECONDS",
            Self::Unset | Self::Unknown(_) => return None,
        })
    }
}

/// `horizontalDatum` (`zStructLocation.h`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HorizontalDatum {
    /// 0: unset.
    Unset,
    /// 1.
    Nad83,
    /// 2.
    Nad27,
    /// 3.
    Wgs84,
    /// 4.
    Wgs72,
    /// 5: local (other).
    Local,
    Unknown(i32),
}

impl HorizontalDatum {
    pub fn from_code(c: i32) -> Self {
        match c {
            0 => Self::Unset,
            1 => Self::Nad83,
            2 => Self::Nad27,
            3 => Self::Wgs84,
            4 => Self::Wgs72,
            5 => Self::Local,
            other => Self::Unknown(other),
        }
    }

    pub fn name(self) -> Option<&'static str> {
        Some(match self {
            Self::Nad83 => "NAD83",
            Self::Nad27 => "NAD27",
            Self::Wgs84 => "WGS84",
            Self::Wgs72 => "WGS72",
            Self::Local => "LOCAL",
            Self::Unset | Self::Unknown(_) => return None,
        })
    }
}

/// `verticalUnits` (`zStructLocation.h`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerticalUnits {
    /// 0: unspecified.
    Unset,
    /// 1.
    Feet,
    /// 2.
    Meters,
    Unknown(i32),
}

impl VerticalUnits {
    pub fn from_code(c: i32) -> Self {
        match c {
            0 => Self::Unset,
            1 => Self::Feet,
            2 => Self::Meters,
            other => Self::Unknown(other),
        }
    }

    pub fn name(self) -> Option<&'static str> {
        Some(match self {
            Self::Feet => "FEET",
            Self::Meters => "METERS",
            Self::Unset | Self::Unknown(_) => return None,
        })
    }
}

/// `verticalDatum` (`zStructLocation.h`; `IVERTICAL_DATUM_*` in `verticalDatum.h`).
/// Names are HEC's `CVERTICAL_DATUM_*` strings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerticalDatum {
    /// 0 (`IVERTICAL_DATUM_UNSET`). Not NAVD88: the datum is unknown.
    Unset,
    /// 1 (`IVERTICAL_DATUM_NAVD88`, `"NAVD-88"`).
    Navd88,
    /// 2 (`IVERTICAL_DATUM_NGVD29`, `"NGVD-29"`).
    Ngvd29,
    /// 3 (`IVERTICAL_DATUM_OTHER` = `IVERTICAL_DATUM_LOCAL`, `"OTHER"`).
    Other,
    Unknown(i32),
}

impl VerticalDatum {
    pub fn from_code(c: i32) -> Self {
        match c {
            0 => Self::Unset,
            1 => Self::Navd88,
            2 => Self::Ngvd29,
            3 => Self::Other,
            other => Self::Unknown(other),
        }
    }

    pub fn name(self) -> Option<&'static str> {
        Some(match self {
            Self::Navd88 => "NAVD-88",
            Self::Ngvd29 => "NGVD-29",
            Self::Other => "OTHER",
            Self::Unset | Self::Unknown(_) => return None,
        })
    }

    /// True for code 0 and for codes outside HEC's table.
    pub fn is_unknown(self) -> bool {
        matches!(self, Self::Unset | Self::Unknown(_))
    }

    /// True only for codes naming a datum (NAVD-88, NGVD-29). Code 3 (`OTHER`, other or
    /// local) is typed but unnamed: neither unknown nor usable as a named datum.
    pub fn is_named(self) -> bool {
        matches!(self, Self::Navd88 | Self::Ngvd29)
    }
}

/// Where a [`Location`] was read from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocationSource {
    /// A DSS7 location record (type 20), `zlocationRetrieve()`.
    Dss7Record,
    /// Words 13–24 (regular) or 17–28 (irregular) of a DSS6 time-series internal header
    /// (`zrrtsi.f`, `zritsi6.f`). No time zone is stored there; supplemental text, when
    /// present, comes from the record's user header ([`SupplementalSource::Dss6UserHeader`]).
    Dss6TsHeader,
}

/// Where [`Location::supplemental`] came from (P27).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SupplementalSource {
    /// values2 of a DSS7 location record (`zlocationRetrieve()`).
    Dss7LocationRecord,
    /// The user header of the DSS6 time-series record the coordinates come from
    /// (docs/format/dss6.md §14). This is Monolith's behavior: its `supplementalInfo` for a
    /// DSS6 time series is the user-header text (`javaHeclib/src/Hec_ztsRetrieve.c`).
    /// HEC's C library leaves the location's supplemental text empty for DSS6
    /// (`Dss6/ztsRetrieveReg6.c`). Informational only (decision D37).
    Dss6UserHeader,
}

impl SupplementalSource {
    /// Name used by the Python layer.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Dss7LocationRecord => "dss7_location_record",
            Self::Dss6UserHeader => "dss6_user_header",
        }
    }
}

/// Location metadata with every stored code kept.
#[derive(Debug, Clone, PartialEq)]
pub struct Location {
    /// Pathname of the record the location was read from: the location record for DSS7,
    /// the time-series record for DSS6.
    pub pathname: Vec<u8>,
    pub source: LocationSource,
    /// `xOrdinate`, `yOrdinate`, `zOrdinate` as stored (bits kept).
    pub x: f64,
    pub y: f64,
    pub z: f64,
    /// The six descriptor codes were stored (`values1Number >= 12` in DSS7). When false
    /// they are 0, as HEC leaves the struct.
    pub descriptors_stored: bool,
    pub coordinate_system_code: i32,
    /// UTM zone, FIPS or ADS State Plane zone number.
    pub coordinate_id: i32,
    pub horizontal_units_code: i32,
    pub horizontal_datum_code: i32,
    pub vertical_units_code: i32,
    pub vertical_datum_code: i32,
    /// Location time-zone name (DSS7). `None` when no words are stored; may be empty.
    pub time_zone_name: Option<String>,
    /// Supplemental text bytes up to the first NUL. DSS7: values2, `None` when values2 is
    /// empty. DSS6: the time-series user-header text (P27,
    /// [`Location::with_dss6_user_header`]), `None` when that text is empty. See
    /// [`Location::supplemental_source`].
    pub supplemental: Option<Vec<u8>>,
}

impl Location {
    pub fn coordinate_system(&self) -> CoordinateSystem {
        CoordinateSystem::from_code(self.coordinate_system_code)
    }
    pub fn horizontal_units(&self) -> HorizontalUnits {
        HorizontalUnits::from_code(self.horizontal_units_code)
    }
    pub fn horizontal_datum(&self) -> HorizontalDatum {
        HorizontalDatum::from_code(self.horizontal_datum_code)
    }
    pub fn vertical_units(&self) -> VerticalUnits {
        VerticalUnits::from_code(self.vertical_units_code)
    }
    pub fn vertical_datum(&self) -> VerticalDatum {
        VerticalDatum::from_code(self.vertical_datum_code)
    }

    /// Where [`Location::supplemental`] came from; `None` exactly when it is `None`.
    pub fn supplemental_source(&self) -> Option<SupplementalSource> {
        self.supplemental.as_ref()?;
        Some(match self.source {
            LocationSource::Dss7Record => SupplementalSource::Dss7LocationRecord,
            LocationSource::Dss6TsHeader => SupplementalSource::Dss6UserHeader,
        })
    }

    /// DSS6 (P27): set the supplemental text from the user-header words of the same
    /// time-series record, as Monolith fills `supplementalInfo`. The text is
    /// [`crate::user_header::user_header_text_bytes`]: up to the first NUL, trailing
    /// blanks removed. An empty text leaves `supplemental` as `None`. Coordinates, codes
    /// and the presence rule are untouched; a datum named in the text stays informational
    /// (D37) and is never merged into the typed fields.
    pub fn with_dss6_user_header(mut self, user_header: &[i32]) -> Self {
        let text = crate::user_header::user_header_text_bytes(user_header);
        self.supplemental = (!text.is_empty()).then_some(text);
        self
    }

    /// Supplemental text. `source == Dss7Record`: Latin-1 decoded (lossless for bytes).
    /// `source == Dss6TsHeader`: [`Location::supplemental`] holds user-header text bytes
    /// (already cut at the NUL and trimmed by [`Location::with_dss6_user_header`]) and is
    /// decoded as the user-header text ([`crate::user_header::decode_user_header_text`],
    /// P28, step 3 from P31): UTF-8, else Java modified UTF-8, else UTF-8 with each
    /// malformed sequence replaced by one U+FFFD, as Java's decoder does.
    pub fn supplemental_text(&self) -> Option<String> {
        self.supplemental_decoded().map(|(s, _)| s)
    }

    /// How [`Location::supplemental_text`] decodes the bytes (P28, P31): `"utf-8"`,
    /// `"modified-utf-8"` or `"utf-8-replaced"` when `source == Dss6TsHeader`, always
    /// `"latin-1"` when `source == Dss7Record`; `None` exactly when
    /// [`Location::supplemental`] is `None`.
    pub fn supplemental_encoding(&self) -> Option<&'static str> {
        self.supplemental_decoded().map(|(_, e)| e)
    }

    fn supplemental_decoded(&self) -> Option<(String, &'static str)> {
        let b = self.supplemental.as_ref()?;
        Some(match self.source {
            LocationSource::Dss6TsHeader => {
                let (s, e) = crate::user_header::decode_user_header_text(b);
                (s, e.as_str())
            }
            LocationSource::Dss7Record => (b.iter().map(|&c| c as char).collect(), "latin-1"),
        })
    }

    /// `Keyword:Item;` pairs of the supplemental text ([`parse_supplemental`]).
    pub fn supplemental_items(&self) -> Vec<(String, String)> {
        self.supplemental_text()
            .map(|s| parse_supplemental(&s))
            .unwrap_or_default()
    }

    /// The item of the first `verticalDatum:` keyword anywhere in the supplemental text
    /// ([`find_supplemental_keyword`]), exactly as stored. Informational only: it is not
    /// validated and never replaces [`Location::vertical_datum`].
    pub fn supplemental_vertical_datum(&self) -> Option<String> {
        self.supplemental_text()
            .and_then(|s| find_supplemental_keyword(&s, "verticalDatum"))
    }

    /// Location from a DSS6 time-series record's coordinates (`zrrtsi.f` lines 588–601):
    /// `None` when the header is too short or all three coordinates are 0 (HEC's
    /// `LCOORDS` is false and `ztsRetrieveReg6()` leaves the location unset).
    pub fn from_dss6_ts(
        pathname: &[u8],
        coordinates: Option<[f64; 3]>,
        descriptors: Option<[i32; 6]>,
    ) -> Option<Self> {
        let c = coordinates?;
        if c.iter().all(|&v| v == 0.0) {
            return None;
        }
        let d = descriptors.unwrap_or([0; 6]);
        Some(Location {
            pathname: pathname.to_vec(),
            source: LocationSource::Dss6TsHeader,
            x: c[0],
            y: c[1],
            z: c[2],
            descriptors_stored: descriptors.is_some(),
            coordinate_system_code: d[0],
            coordinate_id: d[1],
            horizontal_units_code: d[2],
            horizontal_datum_code: d[3],
            vertical_units_code: d[4],
            vertical_datum_code: d[5],
            time_zone_name: None,
            supplemental: None,
        })
    }
}

/// Split supplemental text by HEC's `Keyword:Item;` convention (`zlocationRetrieve()`
/// header comment): segments end at `;`; a segment with a `:` is a pair, keyword before
/// the first `:` and item after it. Nothing is trimmed (as
/// `extractFromDelimitedString()`); empty segments and segments without `:` are skipped.
/// Duplicate keywords are all returned in order (HEC's lookups take the first).
pub fn parse_supplemental(text: &str) -> Vec<(String, String)> {
    text.split(';')
        .filter_map(|seg| {
            let (k, v) = seg.split_once(':')?;
            Some((k.to_string(), v.to_string()))
        })
        .collect()
}

/// Find `keyword:` anywhere in `text`, as `Utilities/verticalDatum.c:extractFromDelimitedString()`
/// does with `strstr` / `dss_strcasestr` (here: ASCII case-insensitive, not bound to
/// segment starts, so it also works in newline-separated text). The item runs to the next
/// `;` (HEC's delimiter) and, unlike HEC, also stops at a line break so newline-separated
/// text gives one line. Returns `None` when the keyword is absent.
pub fn find_supplemental_keyword(text: &str, keyword: &str) -> Option<String> {
    let needle: Vec<u8> = keyword
        .bytes()
        .chain(std::iter::once(b':'))
        .map(|b| b.to_ascii_lowercase())
        .collect();
    let hay = text.as_bytes();
    let start = hay
        .windows(needle.len())
        .position(|w| w.eq_ignore_ascii_case(&needle))?
        + needle.len();
    let rest = &text[start..];
    let end = rest.find([';', '\n', '\r']).unwrap_or(rest.len());
    Some(rest[..end].to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn location_path_follows_zlocation_path() {
        assert_eq!(
            location_pathname(b"/A/B/FLOW/01JAN2000/1Hour/OBS/").unwrap(),
            b"/A/B/Location Info////".to_vec()
        );
        assert_eq!(
            location_pathname(b"//B/STAGE///X/").unwrap(),
            b"//B/Location Info////".to_vec()
        );
        assert_eq!(
            location_pathname(b"/A/B/").unwrap(),
            b"/A/B/Location Info////".to_vec()
        );
        assert!(location_pathname(b"A/B/C/D/E/F/").is_none());
        assert!(location_pathname(b"/A/B").is_none());
        assert!(location_pathname(b"").is_none());
        assert!(is_location_pathname(b"/A/B/location info////"));
        assert!(!is_location_pathname(b"/A/B/FLOW////"));
    }

    #[test]
    fn codes_decode_with_unknown_variants() {
        assert_eq!(VerticalDatum::from_code(0), VerticalDatum::Unset);
        assert!(VerticalDatum::from_code(0).is_unknown());
        assert_eq!(VerticalDatum::from_code(0).name(), None);
        assert_eq!(VerticalDatum::from_code(1).name(), Some("NAVD-88"));
        assert_eq!(VerticalDatum::from_code(2).name(), Some("NGVD-29"));
        assert_eq!(VerticalDatum::from_code(3).name(), Some("OTHER"));
        assert_eq!(VerticalDatum::from_code(9), VerticalDatum::Unknown(9));
        assert!(VerticalDatum::from_code(9).is_unknown());
        assert!(!VerticalDatum::from_code(2).is_unknown());
        assert_eq!(HorizontalUnits::from_code(5), HorizontalUnits::Unknown(5));
        assert_eq!(HorizontalDatum::from_code(5).name(), Some("LOCAL"));
        assert_eq!(
            CoordinateSystem::from_code(-1),
            CoordinateSystem::Unknown(-1)
        );
        assert_eq!(CoordinateSystem::from_code(1).name(), Some("LAT_LONG"));
        assert_eq!(VerticalUnits::from_code(2).name(), Some("METERS"));
    }

    #[test]
    fn supplemental_pairs_and_datum_hint() {
        let items = parse_supplemental("Offset:1.2;Shift:3.4;note;Datum:5.6;Transform:Log:Log;");
        assert_eq!(
            items,
            vec![
                ("Offset".into(), "1.2".into()),
                ("Shift".into(), "3.4".into()),
                ("Datum".into(), "5.6".into()),
                ("Transform".into(), "Log:Log".into()),
            ]
        );
        let loc = Location {
            pathname: b"/A/B/Location Info////".to_vec(),
            source: LocationSource::Dss7Record,
            x: 0.0,
            y: 0.0,
            z: 0.0,
            descriptors_stored: true,
            coordinate_system_code: 0,
            coordinate_id: 0,
            horizontal_units_code: 0,
            horizontal_datum_code: 0,
            vertical_units_code: 1,
            vertical_datum_code: 0,
            time_zone_name: None,
            supplemental: Some(b"Gage:x;VerticalDatum:NGVD29;verticalDatum:other;".to_vec()),
        };
        // Typed datum stays unset; the text names NGVD29 separately.
        assert_eq!(loc.vertical_datum(), VerticalDatum::Unset);
        assert_eq!(loc.supplemental_vertical_datum().as_deref(), Some("NGVD29"));
        // Anywhere in the text, including newline-separated notes; not verticalDatumInfo.
        let f = |t: &str| find_supplemental_keyword(t, "verticalDatum");
        assert_eq!(
            f("Gage 12\nVERTICALDATUM:NAVD88\nother").as_deref(),
            Some("NAVD88")
        );
        assert_eq!(f("verticalDatumInfo:abc;").as_deref(), None);
        assert_eq!(f("a;x verticalDatum:OTHER").as_deref(), Some("OTHER"));
        assert_eq!(f("verticalDatum:").as_deref(), Some(""));
        assert_eq!(f("nothing here"), None);
        assert!(VerticalDatum::from_code(1).is_named() && VerticalDatum::from_code(2).is_named());
        assert!(
            !VerticalDatum::from_code(3).is_named() && !VerticalDatum::from_code(3).is_unknown()
        );
        assert!(!VerticalDatum::from_code(0).is_named() && !VerticalDatum::from_code(9).is_named());
    }

    #[test]
    fn dss6_coordinates_need_a_nonzero_value() {
        assert!(Location::from_dss6_ts(b"/A/B/C/D/E/F/", Some([0.0; 3]), Some([1; 6])).is_none());
        assert!(Location::from_dss6_ts(b"/A/B/C/D/E/F/", None, None).is_none());
        let l = Location::from_dss6_ts(
            b"/A/B/C/D/E/F/",
            Some([1.0, 2.0, 0.0]),
            Some([1, 0, 3, 1, 1, 2]),
        )
        .unwrap();
        assert_eq!(l.source, LocationSource::Dss6TsHeader);
        assert_eq!(l.vertical_datum(), VerticalDatum::Ngvd29);
        assert_eq!(l.horizontal_units(), HorizontalUnits::DecimalDegrees);
        assert!(l.time_zone_name.is_none() && l.supplemental.is_none());
        assert_eq!(l.supplemental_source(), None);
    }

    fn words(b: &[u8]) -> Vec<i32> {
        let mut b = b.to_vec();
        while b.len() % 4 != 0 {
            b.push(b' ');
        }
        b.chunks(4)
            .map(|c| i32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .collect()
    }

    #[test]
    fn dss6_user_header_fills_supplemental_only() {
        let base = Location::from_dss6_ts(
            b"/A/B/C/D/E/F/",
            Some([1.0, 2.0, 3.0]),
            Some([1, 0, 3, 1, 1, 0]),
        )
        .unwrap();
        let l = base
            .clone()
            .with_dss6_user_header(&words(b"Gage: X;VerticalDatum: NAVD88;   "));
        assert_eq!(
            l.supplemental_text().as_deref(),
            Some("Gage: X;VerticalDatum: NAVD88;")
        );
        assert_eq!(
            l.supplemental_source(),
            Some(SupplementalSource::Dss6UserHeader)
        );
        assert_eq!(
            l.supplemental_source().unwrap().as_str(),
            "dss6_user_header"
        );
        // Informational only: the typed datum stays unset (D37).
        assert_eq!(l.vertical_datum(), VerticalDatum::Unset);
        assert_eq!(l.supplemental_vertical_datum().as_deref(), Some(" NAVD88"));
        assert_eq!((l.x, l.y, l.z), (base.x, base.y, base.z));
        assert_eq!(l.vertical_datum_code, base.vertical_datum_code);
        // The text ends at the first NUL; an empty text gives None.
        let mut nul = b"K: v".to_vec();
        nul.extend_from_slice(&[0, b'j', b'u', b'n', b'k']);
        let n = base.clone().with_dss6_user_header(&words(&nul));
        assert_eq!(n.supplemental.as_deref(), Some(&b"K: v"[..]));
        let e = base.clone().with_dss6_user_header(&words(b"    "));
        assert!(e.supplemental.is_none() && e.supplemental_source().is_none());
        assert!(base.with_dss6_user_header(&[]).supplemental.is_none());
    }

    #[test]
    fn dss6_supplemental_uses_the_user_header_decode() {
        let base = Location::from_dss6_ts(b"/A/B/C/D/E/F/", Some([1.0, 2.0, 3.0]), None).unwrap();
        // Valid UTF-8 (P28): one character, as the user-header text.
        let u = base
            .clone()
            .with_dss6_user_header(&words(b"Gage: Y\xc3\xbd;VerticalDatum: NAVD88"));
        assert_eq!(
            u.supplemental_text().as_deref(),
            Some("Gage: Y\u{fd};VerticalDatum: NAVD88")
        );
        assert_eq!(u.supplemental_encoding(), Some("utf-8"));
        assert_eq!(
            u.supplemental_items(),
            vec![
                ("Gage".to_string(), " Y\u{fd}".to_string()),
                ("VerticalDatum".to_string(), " NAVD88".to_string()),
            ]
        );
        assert_eq!(
            u.supplemental.as_deref(),
            Some(&b"Gage: Y\xc3\xbd;VerticalDatum: NAVD88"[..])
        );
        // Invalid UTF-8 (P31): the bad byte is one U+FFFD, the rest is UTF-8.
        let l = base
            .clone()
            .with_dss6_user_header(&words(b"Gage: \xc3\xbd\xe9"));
        assert_eq!(
            l.supplemental_text().as_deref(),
            Some("Gage: \u{fd}\u{fffd}")
        );
        assert_eq!(l.supplemental.as_deref(), Some(&b"Gage: \xc3\xbd\xe9"[..]));
        assert_eq!(l.supplemental_encoding(), Some("utf-8-replaced"));
        assert_eq!(base.supplemental_encoding(), None);
        // DSS7 location text stays Latin-1, even when it is valid UTF-8.
        let mut d7 = base.clone();
        d7.source = LocationSource::Dss7Record;
        d7.supplemental = Some(b"K:\xc3\xbd".to_vec());
        assert_eq!(d7.supplemental_text().as_deref(), Some("K:\u{c3}\u{bd}"));
        assert_eq!(d7.supplemental_encoding(), Some("latin-1"));
    }
}
