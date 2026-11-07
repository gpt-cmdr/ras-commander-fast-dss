//! Zones from GeoJSON. A small parser, written here so that nesting is bounded and every allocation
//! is fallible: neither a deep file nor a large one can end the process.
//!
//! Sources: RFC 8259, RFC 7946, and GDAL 3.2.1 `ogr/ogrsf_frmts/geojson/ogrgeojsonreader.cpp`, whose
//! rules for a CRS are: a `crs` member names it; with none the layer is WGS84 in longitude, latitude.

use super::geometry::{try_vec, Part, Pt};
use super::{refused, ZoneLimits, ZoneRecord};
use crate::error::VortexError;

// GeoJSON's own deepest legal nesting is well under this; collections add a few levels each.
const MAX_DEPTH: usize = 32;

#[derive(Debug)]
enum J {
    Null,
    Bool,
    /// A number written with no fraction or exponent, that fits.
    Int(i64),
    Num(f64),
    Str(String),
    /// An array holding only numbers: a coordinate. Each one is three words, not a tree.
    Nums(Vec<f64>),
    Arr(Vec<J>),
    Obj(Vec<(String, J)>),
}

impl J {
    fn get(&self, key: &str) -> Option<&J> {
        match self {
            // json-c keeps the last of a repeated key.
            J::Obj(members) => members.iter().rev().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    fn string(&self) -> Option<&str> {
        match self {
            J::Str(s) => Some(s),
            _ => None,
        }
    }
}

struct Parser<'a> {
    b: &'a [u8],
    i: usize,
    what: &'a str,
}

impl Parser<'_> {
    fn bad(&self, reason: &str) -> VortexError {
        VortexError::Decode {
            format: "GeoJSON",
            message: format!("{}: {reason} at byte {}", self.what, self.i),
        }
    }

    fn ws(&mut self) {
        while matches!(self.b.get(self.i), Some(b' ' | b'\t' | b'\r' | b'\n')) {
            self.i += 1;
        }
    }

    fn value(&mut self, depth: usize) -> Result<J, VortexError> {
        if depth > MAX_DEPTH {
            return Err(self.bad("nested too deeply"));
        }
        self.ws();
        match self.b.get(self.i) {
            Some(b'{') => self.object(depth),
            Some(b'[') => self.array(depth),
            Some(b'"') => Ok(J::Str(self.string()?)),
            Some(b't') => self.word(b"true", J::Bool),
            Some(b'f') => self.word(b"false", J::Bool),
            Some(b'n') => self.word(b"null", J::Null),
            Some(b'-' | b'0'..=b'9') => self.number(),
            _ => Err(self.bad("expected a value")),
        }
    }

    fn word(&mut self, text: &[u8], value: J) -> Result<J, VortexError> {
        if self.b[self.i..].starts_with(text) {
            self.i += text.len();
            Ok(value)
        } else {
            Err(self.bad("expected a value"))
        }
    }

    fn number(&mut self) -> Result<J, VortexError> {
        let start = self.i;
        if self.b.get(self.i) == Some(&b'-') {
            self.i += 1;
        }
        let digits = |p: &mut Self| {
            let s = p.i;
            while matches!(p.b.get(p.i), Some(b'0'..=b'9')) {
                p.i += 1;
            }
            p.i - s
        };
        let first = self.i;
        let n = digits(self);
        if n == 0 || (n > 1 && self.b[first] == b'0') {
            return Err(self.bad("malformed number"));
        }
        let mut integer = true;
        if self.b.get(self.i) == Some(&b'.') {
            integer = false;
            self.i += 1;
            if digits(self) == 0 {
                return Err(self.bad("malformed number"));
            }
        }
        if matches!(self.b.get(self.i), Some(b'e' | b'E')) {
            integer = false;
            self.i += 1;
            if matches!(self.b.get(self.i), Some(b'+' | b'-')) {
                self.i += 1;
            }
            if digits(self) == 0 {
                return Err(self.bad("malformed number"));
            }
        }
        let text = std::str::from_utf8(&self.b[start..self.i])
            .map_err(|_| self.bad("malformed number"))?;
        if integer {
            if let Ok(v) = text.parse::<i64>() {
                return Ok(J::Int(v));
            }
        }
        let v: f64 = text.parse().map_err(|_| self.bad("malformed number"))?;
        if !v.is_finite() {
            return Err(self.bad("a number is out of range"));
        }
        Ok(J::Num(v))
    }

    fn hex4(&mut self) -> Result<u32, VortexError> {
        let s = self
            .b
            .get(self.i..self.i + 4)
            .ok_or_else(|| self.bad("short escape"))?;
        let t = std::str::from_utf8(s).map_err(|_| self.bad("bad escape"))?;
        let v = u32::from_str_radix(t, 16).map_err(|_| self.bad("bad escape"))?;
        self.i += 4;
        Ok(v)
    }

    fn string(&mut self) -> Result<String, VortexError> {
        self.i += 1;
        let mut out: Vec<u8> = Vec::new();
        let push = |out: &mut Vec<u8>, bytes: &[u8], p: &Self| -> Result<(), VortexError> {
            out.try_reserve(bytes.len())
                .map_err(|_| p.bad("a string could not be allocated"))?;
            out.extend_from_slice(bytes);
            Ok(())
        };
        loop {
            let run = self.i;
            while !matches!(self.b.get(self.i), None | Some(b'"' | b'\\' | 0..=0x1F)) {
                self.i += 1;
            }
            push(&mut out, &self.b[run..self.i], self)?;
            match self.b.get(self.i) {
                Some(b'"') => {
                    self.i += 1;
                    return String::from_utf8(out).map_err(|_| self.bad("a string is not UTF-8"));
                }
                Some(b'\\') => {
                    self.i += 1;
                    let c = *self.b.get(self.i).ok_or_else(|| self.bad("short escape"))?;
                    self.i += 1;
                    let simple = match c {
                        b'"' => Some(b'"'),
                        b'\\' => Some(b'\\'),
                        b'/' => Some(b'/'),
                        b'b' => Some(8),
                        b'f' => Some(12),
                        b'n' => Some(b'\n'),
                        b'r' => Some(b'\r'),
                        b't' => Some(b'\t'),
                        b'u' => None,
                        _ => return Err(self.bad("bad escape")),
                    };
                    if let Some(s) = simple {
                        push(&mut out, &[s], self)?;
                        continue;
                    }
                    let mut code = self.hex4()?;
                    if (0xD800..0xDC00).contains(&code) {
                        if self.b.get(self.i..self.i + 2) != Some(b"\\u") {
                            return Err(self.bad("a lone surrogate"));
                        }
                        self.i += 2;
                        let low = self.hex4()?;
                        if !(0xDC00..0xE000).contains(&low) {
                            return Err(self.bad("a lone surrogate"));
                        }
                        code = 0x10000 + ((code - 0xD800) << 10) + (low - 0xDC00);
                    }
                    let ch = char::from_u32(code).ok_or_else(|| self.bad("a lone surrogate"))?;
                    let mut buffer = [0u8; 4];
                    push(&mut out, ch.encode_utf8(&mut buffer).as_bytes(), self)?;
                }
                _ => return Err(self.bad("an unterminated string")),
            }
        }
    }

    fn array(&mut self, depth: usize) -> Result<J, VortexError> {
        self.i += 1;
        self.ws();
        if self.b.get(self.i) == Some(&b']') {
            self.i += 1;
            return Ok(J::Arr(Vec::new()));
        }
        let mut nums: Vec<f64> = Vec::new();
        let mut items: Vec<J> = Vec::new();
        loop {
            let v = self.value(depth + 1)?;
            let number = match v {
                J::Int(n) => Some(n as f64),
                J::Num(n) => Some(n),
                _ => None,
            };
            match number {
                Some(n) if items.is_empty() => {
                    nums.try_reserve(1)
                        .map_err(|_| self.bad("an array could not be allocated"))?;
                    nums.push(n);
                }
                _ => {
                    items
                        .try_reserve(1 + nums.len())
                        .map_err(|_| self.bad("an array could not be allocated"))?;
                    items.extend(nums.drain(..).map(J::Num));
                    items.push(v);
                }
            }
            self.ws();
            match self.b.get(self.i) {
                Some(b',') => self.i += 1,
                Some(b']') => {
                    self.i += 1;
                    return Ok(if items.is_empty() {
                        J::Nums(nums)
                    } else {
                        J::Arr(items)
                    });
                }
                _ => return Err(self.bad("expected , or ]")),
            }
        }
    }

    fn object(&mut self, depth: usize) -> Result<J, VortexError> {
        self.i += 1;
        let mut members: Vec<(String, J)> = Vec::new();
        self.ws();
        if self.b.get(self.i) == Some(&b'}') {
            self.i += 1;
            return Ok(J::Obj(members));
        }
        loop {
            self.ws();
            if self.b.get(self.i) != Some(&b'"') {
                return Err(self.bad("expected a member name"));
            }
            let key = self.string()?;
            self.ws();
            if self.b.get(self.i) != Some(&b':') {
                return Err(self.bad("expected :"));
            }
            self.i += 1;
            let v = self.value(depth + 1)?;
            members
                .try_reserve(1)
                .map_err(|_| self.bad("an object could not be allocated"))?;
            members.push((key, v));
            self.ws();
            match self.b.get(self.i) {
                Some(b',') => self.i += 1,
                Some(b'}') => {
                    self.i += 1;
                    return Ok(J::Obj(members));
                }
                _ => return Err(self.bad("expected , or }")),
            }
        }
    }
}

fn point_of(v: &J, what: &str) -> Result<Option<Pt>, VortexError> {
    match v {
        J::Nums(n) if n.len() >= 2 => Ok(Some(Pt { x: n[0], y: n[1] })),
        J::Arr(a) if a.is_empty() => Ok(None),
        J::Nums(n) if n.is_empty() => Ok(None),
        _ => Err(refused(
            what,
            "a GeoJSON position is not an array of at least 2 numbers",
        )),
    }
}

fn line_of(
    v: &J,
    what: &str,
    points: &mut usize,
    limits: &ZoneLimits,
) -> Result<Vec<Pt>, VortexError> {
    match v {
        J::Arr(items) => {
            *points += items.len();
            if *points > limits.max_points {
                return Err(refused(
                    what,
                    format!("it holds over {} points, the limit", limits.max_points),
                ));
            }
            let mut out = try_vec(items.len(), what)?;
            for item in items {
                if let Some(p) = point_of(item, what)? {
                    out.push(p);
                }
            }
            Ok(out)
        }
        J::Nums(n) if n.is_empty() => Ok(Vec::new()),
        _ => Err(refused(what, "a GeoJSON line is not an array of positions")),
    }
}

fn list_of<'a>(v: &'a J, what: &str, name: &str) -> Result<&'a [J], VortexError> {
    match v {
        J::Arr(items) => Ok(items),
        J::Nums(n) if n.is_empty() => Ok(&[]),
        _ => Err(refused(what, format!("a GeoJSON {name} is not an array"))),
    }
}

fn parts_of(
    g: &J,
    out: &mut Vec<Part>,
    what: &str,
    points: &mut usize,
    limits: &ZoneLimits,
) -> Result<(), VortexError> {
    if matches!(g, J::Null) {
        return Ok(());
    }
    let kind = g
        .get("type")
        .and_then(J::string)
        .ok_or_else(|| refused(what, "a GeoJSON geometry has no type"))?
        .to_ascii_lowercase();
    if kind == "geometrycollection" {
        let list = g
            .get("geometries")
            .ok_or_else(|| refused(what, "a GeometryCollection has no geometries"))?;
        for item in list_of(list, what, "geometries")? {
            parts_of(item, out, what, points, limits)?;
        }
        return Ok(());
    }
    let coordinates = g
        .get("coordinates")
        .ok_or_else(|| refused(what, "a GeoJSON geometry has no coordinates"))?;
    let mut push = |p: Part| -> Result<(), VortexError> {
        out.try_reserve(1)
            .map_err(|_| refused(what, "its parts could not be allocated"))?;
        out.push(p);
        Ok(())
    };
    let polygon = |c: &J, points: &mut usize| -> Result<Vec<Vec<Pt>>, VortexError> {
        let rings = list_of(c, what, "polygon")?;
        let mut poly = try_vec(rings.len(), what)?;
        for ring in rings {
            poly.push(line_of(ring, what, points, limits)?);
        }
        Ok(poly)
    };
    match kind.as_str() {
        "point" => {
            if let Some(p) = point_of(coordinates, what)? {
                *points += 1;
                push(Part::Point(p))?;
            }
        }
        "multipoint" => {
            for p in line_of(coordinates, what, points, limits)? {
                push(Part::Point(p))?;
            }
        }
        "linestring" => {
            let l = line_of(coordinates, what, points, limits)?;
            if !l.is_empty() {
                push(Part::Line(l))?;
            }
        }
        "multilinestring" => {
            for item in list_of(coordinates, what, "multilinestring")? {
                let l = line_of(item, what, points, limits)?;
                if !l.is_empty() {
                    push(Part::Line(l))?;
                }
            }
        }
        "polygon" => {
            let p = polygon(coordinates, points)?;
            if !p.is_empty() {
                push(Part::Polygon(p))?;
            }
        }
        "multipolygon" => {
            for item in list_of(coordinates, what, "multipolygon")? {
                let p = polygon(item, points)?;
                if !p.is_empty() {
                    push(Part::Polygon(p))?;
                }
            }
        }
        other => {
            return Err(refused(
                what,
                format!("GeoJSON geometry type {other:?} is not known"),
            ))
        }
    }
    Ok(())
}

/// What the file says about its CRS.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum StatedCrs {
    /// A `crs` member naming longitude and latitude on WGS84, or no `crs` member (RFC 7946).
    Wgs84,
    /// A `crs` member naming something else.
    Other(String),
}

/// Every feature, and the file's CRS statement.
pub(super) fn read(
    bytes: &[u8],
    field: &str,
    what: &str,
    limits: &ZoneLimits,
) -> Result<(Vec<ZoneRecord>, StatedCrs), VortexError> {
    let mut parser = Parser {
        b: bytes,
        i: 0,
        what,
    };
    let root = parser.value(0)?;
    parser.ws();
    if parser.i != bytes.len() {
        return Err(parser.bad("text after the document"));
    }
    let stated = match root.get("crs") {
        None | Some(J::Null) => StatedCrs::Wgs84,
        Some(c) => {
            let name = c
                .get("properties")
                .and_then(|p| p.get("name"))
                .and_then(J::string)
                .ok_or_else(|| refused(what, "its crs member has no properties.name"))?;
            match name.to_ascii_uppercase().as_str() {
                "URN:OGC:DEF:CRS:OGC:1.3:CRS84"
                | "URN:OGC:DEF:CRS:OGC::CRS84"
                | "OGC:CRS84"
                | "EPSG:4326"
                | "URN:OGC:DEF:CRS:EPSG::4326" => StatedCrs::Wgs84,
                _ => StatedCrs::Other(name.to_string()),
            }
        }
    };
    let single;
    let features: &[J] = match root
        .get("type")
        .and_then(J::string)
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("featurecollection") => {
            let list = root
                .get("features")
                .ok_or_else(|| refused(what, "a FeatureCollection has no features"))?;
            list_of(list, what, "features")?
        }
        Some("feature") => {
            single = [root];
            &single
        }
        _ => {
            return Err(refused(
                what,
                format!("it is not a FeatureCollection or Feature, so it has no field {field:?}"),
            ))
        }
    };
    if features.len() > limits.max_features {
        return Err(refused(
            what,
            format!(
                "it holds {} features, over the limit of {}",
                features.len(),
                limits.max_features
            ),
        ));
    }
    let mut out = try_vec(features.len(), what)?;
    let mut points = 0usize;
    let mut any_field = false;
    for (i, feature) in features.iter().enumerate() {
        // OGRFeatureDefn::GetFieldIndex is case-insensitive.
        let value = match feature.get("properties") {
            Some(J::Obj(members)) => members
                .iter()
                .rev()
                .find(|(k, _)| k.eq_ignore_ascii_case(field))
                .map(|(_, v)| v),
            _ => None,
        };
        any_field |= value.is_some();
        let id = match value {
            None | Some(J::Null) => None,
            Some(J::Str(s)) => Some(s.clone()),
            Some(J::Int(n)) => Some(n.to_string()),
            Some(other) => {
                return Err(refused(
                    what,
                    format!(
                        "feature {i}'s {field:?} is {}; a zone name must be text or a whole number",
                        match other {
                            J::Bool => "true or false",
                            J::Num(_) => "a fractional number",
                            _ => "an array or object",
                        }
                    ),
                ))
            }
        };
        let mut parts = Vec::new();
        if let Some(g) = feature.get("geometry") {
            parts_of(g, &mut parts, what, &mut points, limits)?;
        }
        out.push(ZoneRecord { id, parts });
    }
    if !any_field && !features.is_empty() {
        return Err(refused(what, format!("no feature has a field {field:?}")));
    }
    Ok((out, stated))
}
