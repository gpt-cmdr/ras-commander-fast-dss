//! WKT1, for the subset of CRSs this crate supports. Anything else is a typed refusal: this
//! never guesses at a CRS it does not fully understand. Text comes from files, so it is hostile
//! input: depth and size are bounded and nothing panics.
//!
//! Parameter names follow what GDAL 3.2.1 reads for these methods, including the ESRI forms
//! that `MorphFromESRI` translates (`ogr/ogrspatialreference.cpp`). The writer reproduces the
//! bytes of `ExportToPrettyWkt` and `ExportToWkt` for the forms `WktFactory.createWkt`
//! generates.

use super::{CrsDef, Datum, Projection};
use crate::error::VortexError;

const MAX_LEN: usize = 64 * 1024;
const MAX_DEPTH: usize = 12;

/// `WktFactory.getShg`, byte for byte.
pub const SHG_ESRI_WKT: &str = "PROJCS[\"USA_Contiguous_Albers_Equal_Area_Conic_USGS_version\",        GEOGCS[\"GCS_North_American_1983\",        DATUM[\"D_North_American_1983\",        SPHEROID[\"GRS_1980\",6378137.0,298.257222101]],PRIMEM[\"Greenwich\",0.0],UNIT[\"Degree\",0.0174532925199433]],PROJECTION[\"Albers\"],        PARAMETER[\"False_Easting\",0.0],PARAMETER[\"False_Northing\",0.0],PARAMETER[\"Central_Meridian\",-96.0],PARAMETER[\"Standard_Parallel_1\",29.5],PARAMETER[\"Standard_Parallel_2\",45.5],PARAMETER[\"Latitude_Of_Origin\",23.0],UNIT[\"Meter\",1.0]]";

/// The SHG WKT Vortex's warp output carries, after `MorphFromESRI`, byte for byte.
pub const SHG_GDAL_WKT: &str = "PROJCS[\"USA_Contiguous_Albers_Equal_Area_Conic_USGS_version\",GEOGCS[\"NAD83\",DATUM[\"North_American_Datum_1983\",SPHEROID[\"GRS 1980\",6378137,298.257222101,AUTHORITY[\"EPSG\",\"7019\"]],AUTHORITY[\"EPSG\",\"6269\"]],PRIMEM[\"Greenwich\",0],UNIT[\"Degree\",0.0174532925199433]],PROJECTION[\"Albers_Conic_Equal_Area\"],PARAMETER[\"latitude_of_center\",23],PARAMETER[\"longitude_of_center\",-96],PARAMETER[\"standard_parallel_1\",29.5],PARAMETER[\"standard_parallel_2\",45.5],PARAMETER[\"false_easting\",0],PARAMETER[\"false_northing\",0],UNIT[\"metre\",1,AUTHORITY[\"EPSG\",\"9001\"]],AXIS[\"Easting\",EAST],AXIS[\"Northing\",NORTH]]";

#[derive(Debug)]
enum Arg {
    Text(String),
    Bare(String),
    Node(Node),
}

#[derive(Debug)]
struct Node {
    name: String,
    args: Vec<Arg>,
}

impl Node {
    fn child(&self, name: &str) -> Option<&Node> {
        self.args.iter().find_map(|a| match a {
            Arg::Node(n) if n.name.eq_ignore_ascii_case(name) => Some(n),
            _ => None,
        })
    }

    fn children<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a Node> + 'a {
        self.args.iter().filter_map(move |a| match a {
            Arg::Node(n) if n.name.eq_ignore_ascii_case(name) => Some(n),
            _ => None,
        })
    }

    fn text(&self, i: usize) -> Option<&str> {
        match self.args.get(i) {
            Some(Arg::Text(s)) => Some(s),
            _ => None,
        }
    }

    fn num(&self, i: usize) -> Option<f64> {
        let raw = match self.args.get(i) {
            Some(Arg::Bare(s)) => s,
            _ => return None,
        };
        raw.trim().parse::<f64>().ok().filter(|v| v.is_finite())
    }
}

struct Parser<'a> {
    b: &'a [u8],
    i: usize,
}

fn malformed(why: &str) -> VortexError {
    VortexError::Decode {
        format: "WKT",
        message: why.to_string(),
    }
}

impl Parser<'_> {
    fn ws(&mut self) {
        while self.b.get(self.i).is_some_and(u8::is_ascii_whitespace) {
            self.i += 1;
        }
    }

    fn word(&mut self) -> String {
        let start = self.i;
        while self
            .b
            .get(self.i)
            .is_some_and(|c| !matches!(c, b'[' | b']' | b'(' | b')' | b',' | b'"'))
        {
            self.i += 1;
        }
        String::from_utf8_lossy(&self.b[start..self.i])
            .trim()
            .to_string()
    }

    fn node(&mut self, depth: usize) -> Result<Node, VortexError> {
        if depth > MAX_DEPTH {
            return Err(malformed("nested too deeply"));
        }
        self.ws();
        let name = self.word();
        if name.is_empty() {
            return Err(malformed("expected a keyword"));
        }
        self.ws();
        let close = match self.b.get(self.i) {
            Some(b'[') => b']',
            Some(b'(') => b')',
            _ => return Err(malformed("expected an opening bracket")),
        };
        self.i += 1;
        let mut args = Vec::new();
        self.ws();
        if self.b.get(self.i) == Some(&close) {
            self.i += 1;
            return Ok(Node { name, args });
        }
        loop {
            self.ws();
            match self.b.get(self.i) {
                None => return Err(malformed("unterminated")),
                Some(b'"') => {
                    self.i += 1;
                    let start = self.i;
                    while self.b.get(self.i).is_some_and(|c| *c != b'"') {
                        self.i += 1;
                    }
                    if self.i >= self.b.len() {
                        return Err(malformed("unterminated string"));
                    }
                    let s = String::from_utf8_lossy(&self.b[start..self.i]).to_string();
                    self.i += 1;
                    args.push(Arg::Text(s));
                }
                Some(_) => {
                    let save = self.i;
                    let w = self.word();
                    self.ws();
                    if matches!(self.b.get(self.i), Some(b'[' | b'(')) {
                        self.i = save;
                        args.push(Arg::Node(self.node(depth + 1)?));
                    } else if w.is_empty() {
                        return Err(malformed("unexpected character"));
                    } else {
                        args.push(Arg::Bare(w));
                    }
                }
            }
            self.ws();
            match self.b.get(self.i) {
                Some(b',') => self.i += 1,
                Some(c) if *c == close => {
                    self.i += 1;
                    return Ok(Node { name, args });
                }
                None => return Err(malformed("unterminated")),
                Some(_) => return Err(malformed("expected a comma or a closing bracket")),
            }
        }
    }
}

fn unsupported(what: impl Into<String>) -> VortexError {
    VortexError::Unsupported { what: what.into() }
}

/// Names compare as GDAL's do: case-insensitively, with spaces as underscores.
fn norm(name: &str) -> String {
    name.trim().replace(' ', "_").to_ascii_lowercase()
}

/// A node's shape: how many plain values lead it, and how many of each child it may have. Any
/// other child is a refusal: an element this parser does not read could change the CRS's
/// meaning, and ignoring it would silently give a different CRS.
fn shape(node: &Node, values: usize, children: &[(&str, usize, usize)]) -> Result<(), VortexError> {
    let plain = node
        .args
        .iter()
        .filter(|a| !matches!(a, Arg::Node(_)))
        .count();
    if plain != values
        || node
            .args
            .iter()
            .take(values)
            .any(|a| matches!(a, Arg::Node(_)))
    {
        return Err(malformed(&format!(
            "{} takes {values} values, found {plain}",
            node.name
        )));
    }
    for arg in &node.args {
        if let Arg::Node(n) = arg {
            if !children.iter().any(|c| n.name.eq_ignore_ascii_case(c.0)) {
                return Err(unsupported(format!("WKT {} in {}", n.name, node.name)));
            }
        }
    }
    for (name, min, max) in children {
        let count = node.children(name).count();
        if count < *min || count > *max {
            return Err(malformed(&format!(
                "{} has {count} {name}, expected {min} to {max}",
                node.name
            )));
        }
    }
    Ok(())
}

/// One parameter's value. A name used twice, or two names for one parameter, is ambiguous even
/// when the values agree; a value that is present and not a finite number is an error rather
/// than an absent parameter.
fn param(node: &Node, names: &[&str]) -> Result<Option<f64>, VortexError> {
    let mut found = None;
    for p in node.children("PARAMETER") {
        if names.contains(&norm(p.text(0).unwrap_or("")).as_str()) {
            if found.is_some() {
                return Err(malformed(&format!("parameter {} given twice", names[0])));
            }
            found =
                Some(p.num(1).ok_or_else(|| {
                    malformed(&format!("parameter {} is not a number", names[0]))
                })?);
        }
    }
    Ok(found)
}

fn required(node: &Node, names: &[&str]) -> Result<f64, VortexError> {
    param(node, names)?.ok_or_else(|| malformed(&format!("missing parameter {}", names[0])))
}

impl CrsDef {
    /// Parse WKT1. WKT2 and unsupported methods, units and meridians are refused, as is anything
    /// unsupported, ambiguous or conflicting.
    pub fn parse_wkt(text: &str) -> Result<CrsDef, VortexError> {
        let def = Self::parse_zone_wkt(text)?;
        if def.projection == Projection::WebMercator {
            // Grids and targets: no warp or raster sink has been compared on this projection.
            return Err(unsupported("projection method mercator_auxiliary_sphere"));
        }
        Ok(def)
    }

    /// [`Self::parse_wkt`], also reading ESRI's `Mercator_Auxiliary_Sphere`, which only a zone file's
    /// coordinates may use: `zonal` transforms them to a grid's CRS and compares the masks.
    pub fn parse_zone_wkt(text: &str) -> Result<CrsDef, VortexError> {
        if text.len() > MAX_LEN {
            return Err(malformed("longer than any supported CRS"));
        }
        let mut p = Parser {
            b: text.as_bytes(),
            i: 0,
        };
        let root = p.node(0)?;
        p.ws();
        if p.i != text.len() {
            return Err(malformed("text after the CRS"));
        }
        match root.name.to_ascii_uppercase().as_str() {
            "GEOGCS" => {
                let (datum, a, rf) = geogcs(&root)?;
                Ok(CrsDef {
                    datum,
                    a,
                    rf,
                    projection: Projection::LongLat,
                })
            }
            "PROJCS" => {
                shape(
                    &root,
                    1,
                    &[
                        ("GEOGCS", 1, 1),
                        ("PROJECTION", 1, 1),
                        ("PARAMETER", 0, 16),
                        ("UNIT", 1, 1),
                        ("AXIS", 0, 2),
                        // GDAL reads the stated parameters, whatever this says (measured).
                        ("AUTHORITY", 0, 1),
                    ],
                )?;
                let geog = root
                    .child("GEOGCS")
                    .ok_or_else(|| malformed("PROJCS without GEOGCS"))?;
                let (datum, a, rf) = geogcs(geog)?;
                let mut projection = projection(&root)?;
                let stated_unit = linear_unit(&root, &projection)?;
                if let Projection::TransverseMercator { to_metre, .. } = &mut projection {
                    *to_metre = stated_unit;
                }
                axes(&root, &projection)?;
                Ok(CrsDef {
                    datum,
                    a,
                    rf,
                    projection,
                })
            }
            other => Err(unsupported(format!("WKT root {other}"))),
        }
    }
}

/// What a complete WKT1 `GEOGCS` or `PROJCS` states, whether or not [`CrsDef`] can hold it. Names of
/// the CRS itself are labels and are left out; every other element the allowlist admits has a field
/// here, so that a comparison built on this can account for all of them.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct WktFacts {
    pub datum: String,
    pub a: f64,
    pub rf: f64,
    /// `TOWGS84`, when stated.
    pub shifts: Option<Vec<f64>>,
    /// Normalised as [`norm`] does, like the other unit and meridian names here.
    pub prime_meridian_name: String,
    pub prime_meridian: f64,
    pub angular_unit_name: String,
    /// Radians per unit of the `GEOGCS`.
    pub angular_unit: f64,
    /// Directions of the `GEOGCS`'s `AXIS` elements, upper-cased, in the order written.
    pub geographic_axes: Vec<String>,
    /// `PROJECTION` of a `PROJCS`; `None` for a `GEOGCS`.
    pub projection: Option<String>,
    /// Lower-cased names, as written, in the order written.
    pub parameters: Vec<(String, f64)>,
    pub linear_unit_name: Option<String>,
    /// Metres per unit of a `PROJCS`.
    pub linear_unit: Option<f64>,
    /// Directions of the `PROJCS`'s own `AXIS` elements, as for `geographic_axes`.
    pub projected_axes: Vec<String>,
    /// `AUTHORITY` elements at any depth.
    pub authorities: usize,
    /// `EXTENSION` elements at any depth.
    pub extensions: usize,
}

fn count_named(node: &Node, name: &str) -> usize {
    node.args
        .iter()
        .map(|arg| match arg {
            Arg::Node(n) => usize::from(n.name.eq_ignore_ascii_case(name)) + count_named(n, name),
            _ => 0,
        })
        .sum()
}

fn named_number(node: &Node, what: &str) -> Result<f64, VortexError> {
    if node.text(0).is_none_or(str::is_empty) {
        return Err(malformed("an element without a name"));
    }
    node.num(1)
        .ok_or_else(|| malformed(&format!("{what} is not a number")))
}

/// `AUTHORITY["EPSG","4326"]` or `EXTENSION["PROJ4","..."]`: two texts. An authority's code may be
/// a bare number, as in Vortex's `orb100yr24ha.prj`, `AUTHORITY["EPSG",4322]`, which GDAL reads.
fn two_texts(node: &Node, name: &str) -> Result<(), VortexError> {
    for found in node.children(name) {
        shape(found, 2, &[])?;
        let number_allowed = name.eq_ignore_ascii_case("AUTHORITY") && found.num(1).is_some();
        if found.text(0).is_none() || (found.text(1).is_none() && !number_allowed) {
            return Err(malformed(&format!("{name} takes two texts")));
        }
    }
    Ok(())
}

/// None, or the pair GDAL reads. Directions are the horizontal ones: `GEOGCS` and `PROJCS` have no
/// other axes, and one axis alone gives no CRS (measured, GDAL 3.12.2 and `AXIS["x",UP]`).
fn stated_axes(node: &Node) -> Result<(), VortexError> {
    let mut count = 0;
    for axis in node.children("AXIS") {
        shape(axis, 2, &[])?;
        let direction = match axis.args.get(1) {
            Some(Arg::Bare(direction)) => direction.as_str(),
            _ => "",
        };
        if axis.text(0).is_none_or(str::is_empty)
            || !["NORTH", "SOUTH", "EAST", "WEST"]
                .iter()
                .any(|d| d.eq_ignore_ascii_case(direction))
        {
            return Err(malformed("AXIS needs a name and a horizontal direction"));
        }
        count += 1;
    }
    if count == 1 {
        return Err(malformed("one AXIS"));
    }
    Ok(())
}

/// The elements a `GEOGCS` may hold, each with the values it must have. An allowlist: text with
/// anything else may read as no CRS in GDAL, as `TOWGS84[1,2]` does, and writing it would lose it.
fn geogcs_shape(geog: &Node) -> Result<(), VortexError> {
    shape(
        geog,
        1,
        &[
            ("DATUM", 1, 1),
            ("PRIMEM", 1, 1),
            ("UNIT", 1, 1),
            ("AXIS", 0, 2),
            ("AUTHORITY", 0, 1),
            ("EXTENSION", 0, 1),
        ],
    )?;
    let datum = geog
        .child("DATUM")
        .ok_or_else(|| malformed("GEOGCS without DATUM"))?;
    shape(
        datum,
        1,
        &[
            ("SPHEROID", 1, 1),
            ("TOWGS84", 0, 1),
            ("AUTHORITY", 0, 1),
            ("EXTENSION", 0, 1),
        ],
    )?;
    if let Some(shift) = datum.child("TOWGS84") {
        if !matches!(shift.args.len(), 3 | 7)
            || (0..shift.args.len()).any(|i| shift.num(i).is_none())
        {
            return Err(malformed("TOWGS84 takes 3 or 7 numbers"));
        }
    }
    two_texts(datum, "EXTENSION")?;
    two_texts(datum, "AUTHORITY")?;
    let spheroid = datum
        .child("SPHEROID")
        .ok_or_else(|| malformed("DATUM without SPHEROID"))?;
    shape(spheroid, 3, &[("AUTHORITY", 0, 1)])?;
    two_texts(spheroid, "AUTHORITY")?;
    for name in ["PRIMEM", "UNIT"] {
        for element in geog.children(name) {
            shape(element, 2, &[("AUTHORITY", 0, 1)])?;
            two_texts(element, "AUTHORITY")?;
        }
    }
    two_texts(geog, "AUTHORITY")?;
    two_texts(geog, "EXTENSION")?;
    stated_axes(geog)
}

/// `PROJCS` elements other than its `GEOGCS`; see [`geogcs_shape`].
fn projcs_shape(root: &Node) -> Result<(), VortexError> {
    shape(
        root,
        1,
        &[
            ("GEOGCS", 1, 1),
            ("PROJECTION", 1, 1),
            ("PARAMETER", 0, usize::MAX),
            ("UNIT", 1, 1),
            ("AXIS", 0, 2),
            ("AUTHORITY", 0, 1),
            ("EXTENSION", 0, 1),
        ],
    )?;
    for projection in root.children("PROJECTION") {
        shape(projection, 1, &[("AUTHORITY", 0, 1)])?;
        two_texts(projection, "AUTHORITY")?;
    }
    for parameter in root.children("PARAMETER") {
        shape(parameter, 2, &[("AUTHORITY", 0, 1)])?;
        two_texts(parameter, "AUTHORITY")?;
    }
    for unit in root.children("UNIT") {
        shape(unit, 2, &[("AUTHORITY", 0, 1)])?;
        two_texts(unit, "AUTHORITY")?;
    }
    two_texts(root, "AUTHORITY")?;
    two_texts(root, "EXTENSION")?;
    stated_axes(root)
}

/// `GEOGCS` content: every element PROJ needs to build a CRS from it, with usable values. The
/// counts of `AUTHORITY` and `EXTENSION` are this element's; a `PROJCS` replaces them with its own.
fn geogcs_facts(geog: &Node) -> Result<WktFacts, VortexError> {
    if geog.text(0).is_none_or(str::is_empty) {
        return Err(malformed("GEOGCS without a name"));
    }
    geogcs_shape(geog)?;
    let datum = geog
        .child("DATUM")
        .ok_or_else(|| malformed("GEOGCS without DATUM"))?;
    let name = datum
        .text(0)
        .filter(|n| !n.is_empty())
        .ok_or_else(|| malformed("DATUM without a name"))?;
    let spheroid = datum
        .child("SPHEROID")
        .ok_or_else(|| malformed("DATUM without SPHEROID"))?;
    let a = named_number(spheroid, "the semi-major axis")?;
    let rf = spheroid
        .num(2)
        .ok_or_else(|| malformed("the inverse flattening is not a number"))?;
    if a <= 0.0 || rf < 0.0 || (rf > 0.0 && rf <= 1.0) {
        return Err(malformed("SPHEROID axes"));
    }
    let prime = geog
        .child("PRIMEM")
        .ok_or_else(|| malformed("GEOGCS without PRIMEM"))?;
    let prime_meridian = named_number(prime, "the prime meridian")?;
    let unit = geog
        .child("UNIT")
        .ok_or_else(|| malformed("GEOGCS without UNIT"))?;
    let angular_unit = named_number(unit, "the angular unit")?;
    if angular_unit <= 0.0 {
        return Err(malformed("the angular unit is not positive"));
    }
    // geogcs_shape allows only 3 or 7 numbers.
    let shifts = datum
        .child("TOWGS84")
        .map(|shift| (0..shift.args.len()).filter_map(|i| shift.num(i)).collect());
    Ok(WktFacts {
        datum: name.to_string(),
        a,
        rf,
        shifts,
        prime_meridian_name: norm(prime.text(0).unwrap_or("")),
        prime_meridian,
        angular_unit_name: norm(unit.text(0).unwrap_or("")),
        angular_unit,
        geographic_axes: axis_directions(geog)?,
        projection: None,
        parameters: Vec::new(),
        linear_unit_name: None,
        linear_unit: None,
        projected_axes: Vec::new(),
        authorities: count_named(geog, "AUTHORITY"),
        extensions: count_named(geog, "EXTENSION"),
    })
}

/// The content of a WKT1 `GEOGCS` or `PROJCS`, or why it is not a complete CRS. Bracket-balanced
/// text such as `GEOGCS["bogus"]` is not one: GDAL reads no CRS from it, so writing it loses the CRS.
pub(crate) fn wkt_facts(text: &str) -> Result<WktFacts, VortexError> {
    if text.len() > MAX_LEN {
        return Err(malformed("longer than any supported CRS"));
    }
    let mut p = Parser {
        b: text.as_bytes(),
        i: 0,
    };
    let root = p.node(0)?;
    p.ws();
    if p.i != text.len() {
        return Err(malformed("text after the CRS"));
    }
    match root.name.to_ascii_uppercase().as_str() {
        "GEOGCS" => geogcs_facts(&root),
        "PROJCS" => {
            if root.text(0).is_none_or(str::is_empty) {
                return Err(malformed("PROJCS without a name"));
            }
            let geog = root
                .child("GEOGCS")
                .ok_or_else(|| malformed("PROJCS without GEOGCS"))?;
            let geographic = geogcs_facts(geog)?;
            projcs_shape(&root)?;
            let projection = root
                .child("PROJECTION")
                .and_then(|n| n.text(0))
                .filter(|n| !n.is_empty())
                .ok_or_else(|| malformed("PROJCS without PROJECTION"))?;
            let unit = root
                .child("UNIT")
                .ok_or_else(|| malformed("PROJCS without UNIT"))?;
            let linear_unit = named_number(unit, "the linear unit")?;
            if linear_unit <= 0.0 {
                return Err(malformed("the linear unit is not positive"));
            }
            let mut parameters = Vec::new();
            for parameter in root.children("PARAMETER") {
                let value = named_number(parameter, "a PARAMETER")?;
                let name = parameter.text(0).unwrap_or("").to_ascii_lowercase();
                parameters.push((name, value));
            }
            Ok(WktFacts {
                projection: Some(projection.to_string()),
                parameters,
                linear_unit_name: Some(norm(unit.text(0).unwrap_or(""))),
                linear_unit: Some(linear_unit),
                projected_axes: axis_directions(&root)?,
                authorities: count_named(&root, "AUTHORITY"),
                extensions: count_named(&root, "EXTENSION"),
                ..geographic
            })
        }
        other => Err(unsupported(format!("WKT root {other}"))),
    }
}

fn geogcs(geog: &Node) -> Result<(Datum, f64, f64), VortexError> {
    shape(
        geog,
        1,
        &[
            ("DATUM", 1, 1),
            ("PRIMEM", 1, 1),
            ("UNIT", 1, 1),
            ("AXIS", 0, 2),
            ("AUTHORITY", 0, 1),
            ("EXTENSION", 0, 1),
        ],
    )?;
    let datum = geog
        .child("DATUM")
        .ok_or_else(|| malformed("GEOGCS without DATUM"))?;
    shape(
        datum,
        1,
        &[("SPHEROID", 1, 1), ("TOWGS84", 0, 1), ("AUTHORITY", 0, 1)],
    )?;
    let name = datum
        .text(0)
        .ok_or_else(|| malformed("DATUM without name"))?;
    let spheroid = datum
        .child("SPHEROID")
        .ok_or_else(|| malformed("DATUM without SPHEROID"))?;
    shape(spheroid, 3, &[("AUTHORITY", 0, 1)])?;
    let a = spheroid
        .num(1)
        .ok_or_else(|| malformed("SPHEROID semi-major axis"))?;
    let rf = spheroid
        .num(2)
        .ok_or_else(|| malformed("SPHEROID inverse flattening"))?;
    // rf of 0 is WKT1's sphere; between 0 and 1 the flattening is above 1, which is no ellipsoid.
    if a <= 0.0 || rf < 0.0 || (rf > 0.0 && rf <= 1.0) {
        return Err(malformed("SPHEROID axes"));
    }
    let shifts = match datum.child("TOWGS84") {
        Some(t) => {
            let n = t.args.len();
            if n != 3 && n != 7 {
                return Err(malformed("TOWGS84 takes 3 or 7 values"));
            }
            let values: Option<Vec<f64>> = (0..n).map(|i| t.num(i)).collect();
            Some(values.ok_or_else(|| malformed("TOWGS84 value is not a number"))?)
        }
        None => None,
    };
    let prime = geog
        .child("PRIMEM")
        .ok_or_else(|| malformed("GEOGCS without PRIMEM"))?;
    shape(prime, 2, &[("AUTHORITY", 0, 1)])?;
    let prime_name = prime.text(0).map(norm);
    if prime.num(1) != Some(0.0) || prime_name.as_deref() != Some("greenwich") {
        return Err(unsupported("prime meridian other than Greenwich"));
    }
    let unit = geog
        .child("UNIT")
        .ok_or_else(|| malformed("GEOGCS without UNIT"))?;
    shape(unit, 2, &[("AUTHORITY", 0, 1)])?;
    let value = unit.num(1).ok_or_else(|| malformed("GEOGCS UNIT value"))?;
    // WKT1 writes the degree to 15 or 16 significant digits; other units are not the degree.
    if unit.text(0).map(norm).as_deref() != Some("degree")
        || (value - super::proj::DEG_TO_RAD).abs() > 1e-13
    {
        return Err(unsupported("angular unit other than the degree"));
    }
    // CENTER_LONG in WKT text has no effect in GDAL 3.2.1 (measured), so it is ignored here
    // too. See `Transformer::with_center_long`.
    for e in geog.children("EXTENSION") {
        shape(e, 2, &[])?;
        let name = e.text(0).unwrap_or("");
        if !name.eq_ignore_ascii_case("CENTER_LONG") {
            return Err(unsupported(format!("WKT EXTENSION {name}")));
        }
    }
    geog_axes(geog)?;
    Ok((
        Datum::classify_stated(
            name,
            a,
            rf,
            shifts.as_deref(),
            authority(datum),
            authority(spheroid),
            authority(geog),
        ),
        a,
        rf,
    ))
}

/// An `EPSG` authority's code. Any other authority gives a code no rule recognises.
fn authority(node: &Node) -> Option<String> {
    let a = node.child("AUTHORITY")?;
    let auth = a.text(0).unwrap_or("");
    let code = a.text(1).unwrap_or("");
    Some(if auth.eq_ignore_ascii_case("EPSG") {
        code.to_string()
    } else {
        format!("{auth}:{code}")
    })
}

/// Names GDAL and ESRI write for the two feet. A unit must have both a name and a value this
/// knows, and they must agree: PROJ reads the value, and a name that says otherwise is a conflict.
const US_FOOT_NAMES: &[&str] = &["foot_us", "us_survey_foot", "us_foot", "us-ft"];
const FOOT_NAMES: &[&str] = &["foot", "international_foot", "foot_intl", "ft"];

/// Metres per unit of the `PROJCS`. Only Transverse Mercator may be in feet: no other method has
/// been compared in them.
fn linear_unit(root: &Node, projection: &Projection) -> Result<f64, VortexError> {
    let unit = root
        .child("UNIT")
        .ok_or_else(|| malformed("PROJCS without UNIT"))?;
    shape(unit, 2, &[("AUTHORITY", 0, 1)])?;
    let name = unit.text(0).map(norm);
    let name = name.as_deref().unwrap_or("");
    let value = unit.num(1).ok_or_else(|| malformed("PROJCS UNIT value"))?;
    if value == 1.0 && matches!(name, "meter" | "metre") {
        return Ok(1.0);
    }
    if !matches!(projection, Projection::TransverseMercator { .. }) {
        return Err(unsupported("linear unit other than the metre"));
    }
    match super::LinearUnit::of_metres(value) {
        Some(super::LinearUnit::UsSurveyFoot) if US_FOOT_NAMES.contains(&name) => Ok(value),
        Some(super::LinearUnit::InternationalFoot) if FOOT_NAMES.contains(&name) => Ok(value),
        _ => Err(unsupported(format!(
            "linear unit {name} of {value} m on Transverse Mercator: only the metre, the US \
             survey foot and the international foot are compared"
        ))),
    }
}

fn axis_directions(node: &Node) -> Result<Vec<String>, VortexError> {
    node.children("AXIS")
        .map(|a| match (a.args.len(), a.text(0), a.args.get(1)) {
            (2, Some(_), Some(Arg::Bare(d))) => Ok(d.to_ascii_uppercase()),
            _ => Err(malformed("AXIS")),
        })
        .collect()
}

/// In traditional GIS order, which Vortex sets on both CRSs, x is east-positive longitude in
/// either of these forms. Vortex's GDAL refuses or changes the others (measured: a west-positive
/// longitude fails every point).
fn geog_axes(geog: &Node) -> Result<(), VortexError> {
    let dirs = axis_directions(geog)?;
    if dirs.is_empty() || dirs == ["NORTH", "EAST"] || dirs == ["EAST", "NORTH"] {
        Ok(())
    } else {
        Err(unsupported("geographic axes other than north and east"))
    }
}

fn axes(root: &Node, projection: &Projection) -> Result<(), VortexError> {
    let dirs = axis_directions(root)?;
    // Traditional GIS order: Vortex sets it on both CRSs. GDAL's SetPS writes a pole's axes as
    // both pointing away from or towards it; the transformation is the same as with none
    // (measured against PROJ).
    let pole_axes = match projection {
        Projection::Stereographic {
            lat_ts: Some(ts), ..
        } if *ts > 0.0 => dirs == ["SOUTH", "SOUTH"],
        Projection::Stereographic {
            lat_ts: Some(_), ..
        } => dirs == ["NORTH", "NORTH"],
        _ => false,
    };
    if dirs.is_empty() || dirs == ["EAST", "NORTH"] || pole_axes {
        Ok(())
    } else {
        Err(unsupported("axes other than easting, northing"))
    }
}

fn projection(root: &Node) -> Result<Projection, VortexError> {
    let node = root
        .child("PROJECTION")
        .ok_or_else(|| malformed("PROJCS without PROJECTION"))?;
    shape(node, 1, &[("AUTHORITY", 0, 1)])?;
    let method = node
        .text(0)
        .ok_or_else(|| malformed("PROJECTION without name"))?;
    let method = norm(method);
    // Each method's parameters: the WKT1 names, and the ESRI ones for Albers.
    const LAT_C: &[&str] = &["latitude_of_center", "latitude_of_origin"];
    const LON_C: &[&str] = &["longitude_of_center", "central_meridian"];
    const LAT_O: &[&str] = &["latitude_of_origin"];
    const LON_O: &[&str] = &["central_meridian"];
    const SP1: &[&str] = &["standard_parallel_1"];
    const SP2: &[&str] = &["standard_parallel_2"];
    const SCALE: &[&str] = &["scale_factor"];
    const FE: &[&str] = &["false_easting"];
    const FN: &[&str] = &["false_northing"];
    const AUX: &[&str] = &["auxiliary_sphere_type"];
    let known: &[&[&str]] = match method.as_str() {
        "albers" | "albers_conic_equal_area" => &[SP1, SP2, LAT_C, LON_C, FE, FN],
        "lambert_conformal_conic_2sp" | "lambert_conformal_conic" => {
            &[SP1, SP2, LAT_O, LON_O, FE, FN]
        }
        "lambert_conformal_conic_1sp" | "stereographic" | "polar_stereographic" => {
            &[LAT_O, LON_O, SCALE, FE, FN]
        }
        "mercator_auxiliary_sphere" => &[SP1, LON_O, FE, FN, AUX],
        "transverse_mercator" => &[LAT_O, LON_O, SCALE, FE, FN],
        other => return Err(unsupported(format!("projection method {other}"))),
    };
    for p in root.children("PARAMETER") {
        shape(p, 2, &[])?;
        let name = norm(p.text(0).unwrap_or(""));
        if !known.iter().any(|set| set.contains(&name.as_str())) {
            return Err(unsupported(format!("parameter {name} on {method}")));
        }
    }
    let x_0 = required(root, FE)?;
    let y_0 = required(root, FN)?;
    match method.as_str() {
        // PROJ reads this as "Popular Visualisation Pseudo Mercator" only when the type and the rest
        // are 0.
        "mercator_auxiliary_sphere" => {
            for set in [SP1, LON_O, AUX] {
                if required(root, set)? != 0.0 {
                    return Err(unsupported(format!(
                        "mercator_auxiliary_sphere with a non-zero {}",
                        set[0]
                    )));
                }
            }
            if x_0 != 0.0 || y_0 != 0.0 {
                return Err(unsupported(
                    "mercator_auxiliary_sphere with a false easting or northing",
                ));
            }
            Ok(Projection::WebMercator)
        }
        // The unit is filled in by the caller, which reads the `PROJCS`'s `UNIT`.
        "transverse_mercator" => Ok(Projection::TransverseMercator {
            lat_0: required(root, LAT_O)?,
            lon_0: required(root, LON_O)?,
            k_0: required(root, SCALE)?,
            x_0,
            y_0,
            to_metre: 1.0,
        }),
        "albers" | "albers_conic_equal_area" => Ok(Projection::Albers {
            lat_1: required(root, SP1)?,
            lat_2: required(root, SP2)?,
            lat_0: required(root, LAT_C)?,
            lon_0: required(root, LON_C)?,
            x_0,
            y_0,
        }),
        "lambert_conformal_conic_2sp" | "lambert_conformal_conic" => Ok(Projection::Lambert {
            lat_1: required(root, SP1)?,
            lat_2: required(root, SP2)?,
            lat_0: required(root, LAT_O)?,
            lon_0: required(root, LON_O)?,
            k_0: 1.0,
            x_0,
            y_0,
        }),
        "lambert_conformal_conic_1sp" => {
            let lat_0 = required(root, LAT_O)?;
            Ok(Projection::Lambert {
                lat_1: lat_0,
                lat_2: lat_0,
                lat_0,
                lon_0: required(root, LON_O)?,
                k_0: required(root, SCALE)?,
                x_0,
                y_0,
            })
        }
        "stereographic" => Ok(Projection::Stereographic {
            lat_0: required(root, LAT_O)?,
            lon_0: required(root, LON_O)?,
            k_0: required(root, SCALE)?,
            lat_ts: None,
            x_0,
            y_0,
        }),
        _ => {
            let lat = required(root, LAT_O)?;
            let lon_0 = required(root, LON_O)?;
            // SetPS omits the scale for a true-scale latitude; it is 1 there.
            let k_0 = param(root, SCALE)?.unwrap_or(1.0);
            if lat.abs() == 90.0 {
                // Variant A: the scale at the pole.
                Ok(Projection::Stereographic {
                    lat_0: lat,
                    lon_0,
                    k_0,
                    lat_ts: None,
                    x_0,
                    y_0,
                })
            } else if lat == 0.0 {
                Err(unsupported("polar stereographic true at the equator"))
            } else if k_0 == 1.0 {
                // Variant B: the latitude of true scale, on the pole of the same sign.
                Ok(Projection::Stereographic {
                    lat_0: if lat < 0.0 { -90.0 } else { 90.0 },
                    lon_0,
                    k_0,
                    lat_ts: Some(lat),
                    x_0,
                    y_0,
                })
            } else {
                Err(unsupported(
                    "polar stereographic with both a true-scale latitude and a scale factor",
                ))
            }
        }
    }
}

/// C's `%.15g`, the precision GDAL's WKT export writes.
pub(crate) fn fmt_g15(value: f64) -> String {
    if value == 0.0 {
        return "0".to_string();
    }
    let sci = format!("{value:.14e}");
    let (mantissa, exp) = sci.split_once('e').unwrap_or((&sci, "0"));
    let exp: i32 = exp.parse().unwrap_or(0);
    if !(-5..15).contains(&exp) {
        let m = mantissa.trim_end_matches('0').trim_end_matches('.');
        let sign = if exp < 0 { '-' } else { '+' };
        return format!("{m}e{sign}{:02}", exp.abs());
    }
    let decimals = usize::try_from(14 - exp).unwrap_or(0);
    let fixed = format!("{value:.decimals$}");
    if fixed.contains('.') {
        fixed
            .trim_end_matches('0')
            .trim_end_matches('.')
            .to_string()
    } else {
        fixed
    }
}

/// C's `%.{significant}g`.
fn fmt_g(v: f64, significant: usize) -> String {
    if v == 0.0 {
        return if v.is_sign_negative() { "-0" } else { "0" }.to_string();
    }
    let text = format!("{v:.*e}", significant - 1);
    let Some((mantissa, exponent)) = text.split_once('e') else {
        return text;
    };
    let exponent: i32 = exponent.parse().unwrap_or(0);
    let negative = mantissa.starts_with('-');
    let digits: String = mantissa.chars().filter(char::is_ascii_digit).collect();
    let trimmed = digits.trim_end_matches('0');
    let trimmed = if trimmed.is_empty() { "0" } else { trimmed };
    let body = if exponent < -4 || exponent >= significant as i32 {
        let sign = if exponent < 0 { '-' } else { '+' };
        let (head, tail) = trimmed.split_at(1);
        let point = if tail.is_empty() { "" } else { "." };
        format!("{head}{point}{tail}e{sign}{:02}", exponent.abs())
    } else if exponent < 0 {
        format!("0.{}{trimmed}", "0".repeat((-exponent - 1) as usize))
    } else {
        let whole = exponent as usize + 1;
        if trimmed.len() <= whole {
            format!("{trimmed}{}", "0".repeat(whole - trimmed.len()))
        } else {
            let (head, tail) = trimmed.split_at(whole);
            format!("{head}.{tail}")
        }
    };
    if negative {
        format!("-{body}")
    } else {
        body
    }
}

/// An Albers angle as Vortex's records word it, from the record's `f32`. PROJ keeps an angle in degrees
/// and converts through SI to write it: `value * c / c`, in doubles, with `c` =
/// `UnitOfMeasure::DEGREE`'s `conversionToSI` (`Measure::getSIValue`, `Measure::convertToUnit`). That round
/// trip moves some values by an ulp, which `%.15g` can show: 29.3 as `f32` is 29.299999237060546875, which
/// `%.15g` alone words as 29.2999992370605 and Vortex 1.1.0 and 0.13.3 as 29.2999992370606. Measured on the
/// `albers_odd*` cases: this agrees with all 16 angles there, and `%.15g` alone does not, nor `%.16g` then
/// `%.15g`. PROJ's source was not at hand to cite. Only Albers parameters come here: [`fmt_g15`] serves
/// other WKT numbers.
pub(crate) fn albers_angle_text(v: f32) -> String {
    const DEGREE_TO_SI: f64 = 0.017453292519943295;
    fmt_g(f64::from(v) * DEGREE_TO_SI / DEGREE_TO_SI, 15)
}

/// An Albers false easting or northing as Vortex's records word it, in metres: `%.15g` of the `f32`.
pub(crate) fn albers_metres_text(v: f32) -> String {
    fmt_g(f64::from(v), 15)
}

impl CrsDef {
    fn pretty_geogcs(&self) -> String {
        let (geog, datum, spheroid) = self.wkt_names();
        format!(
            "    GEOGCS[\"{geog}\",\n        DATUM[\"{datum}\",\n            SPHEROID[\"{spheroid}\",{},{}]],\n        PRIMEM[\"Greenwich\",0],\n        UNIT[\"degree\",0.0174532925199433,\n            AUTHORITY[\"EPSG\",\"9122\"]]],\n",
            fmt_g15(self.a),
            fmt_g15(self.rf)
        )
    }

    /// A refused datum must stay refused after writing and reading back, so its name is kept. A
    /// `Shifted` datum's shift is not held here, and the name written says so.
    fn wkt_names(&self) -> (&'static str, String, &'static str) {
        let label = if self.rf == 0.0 { "sphere" } else { "spheroid" };
        match &self.datum {
            Datum::Nad83 => ("NAD83", "North_American_Datum_1983".into(), "GRS 1980"),
            Datum::Wgs84 => ("WGS 84", "WGS_1984".into(), "WGS 84"),
            Datum::Sphere => ("unknown", "unknown".into(), label),
            Datum::Other(name) => ("unknown", name.clone(), label),
            Datum::Shifted(name) => ("unknown", format!("{name} (shift not written)"), label),
        }
    }

    /// WKT in the form `WktFactory.createWkt` generates, through GDAL's `ExportToPrettyWkt`
    /// (projected) or `ExportToWkt` (geographic).
    pub fn to_wkt(&self) -> String {
        let tail = match self.projection {
            Projection::Stereographic {
                lat_ts: Some(ts), ..
            } if ts > 0.0 => {
                "    UNIT[\"Meter\",1],\n    AXIS[\"Easting\",SOUTH],\n    AXIS[\"Northing\",SOUTH]]"
            }
            Projection::Stereographic {
                lat_ts: Some(_), ..
            } => {
                "    UNIT[\"Meter\",1],\n    AXIS[\"Easting\",NORTH],\n    AXIS[\"Northing\",NORTH]]"
            }
            _ => "    UNIT[\"Meter\",1],\n    AXIS[\"Easting\",EAST],\n    AXIS[\"Northing\",NORTH]]",
        };
        let f = fmt_g15;
        let p = |n: &str, v: f64| format!("    PARAMETER[\"{n}\",{}],\n", f(v));
        match self.projection {
            // GDAL's `SetTM` order. A unit in feet is written with every digit it has, so that
            // reading the text back gives this definition; `%.15g` would drop one.
            Projection::TransverseMercator {
                lat_0,
                lon_0,
                k_0,
                x_0,
                y_0,
                to_metre,
            } => {
                let unit = match super::LinearUnit::of_metres(to_metre) {
                    Some(super::LinearUnit::UsSurveyFoot) => {
                        format!("UNIT[\"US survey foot\",{to_metre}]")
                    }
                    Some(super::LinearUnit::InternationalFoot) => {
                        format!("UNIT[\"Foot\",{to_metre}]")
                    }
                    Some(super::LinearUnit::Metre) => "UNIT[\"Meter\",1]".to_string(),
                    // No parser gives such a unit; written so that reading it back refuses.
                    None => format!("UNIT[\"unsupported\",{to_metre}]"),
                };
                format!(
                    "PROJCS[\"Transverse Mercator\",\n{}    PROJECTION[\"Transverse_Mercator\"],\n{}{}{}{}{}    {unit},\n    AXIS[\"Easting\",EAST],\n    AXIS[\"Northing\",NORTH]]",
                    self.pretty_geogcs(),
                    p("latitude_of_origin", lat_0),
                    p("central_meridian", lon_0),
                    p("scale_factor", k_0),
                    p("false_easting", x_0),
                    p("false_northing", y_0),
                )
            }
            Projection::WebMercator => {
                let (geog, datum, spheroid) = self.wkt_names();
                format!(
                    "PROJCS[\"WGS_1984_Web_Mercator_Auxiliary_Sphere\",GEOGCS[\"{geog}\",DATUM[\"{datum}\",SPHEROID[\"{spheroid}\",{},{}]],PRIMEM[\"Greenwich\",0],UNIT[\"Degree\",0.0174532925199433]],PROJECTION[\"Mercator_Auxiliary_Sphere\"],PARAMETER[\"False_Easting\",0],PARAMETER[\"False_Northing\",0],PARAMETER[\"Central_Meridian\",0],PARAMETER[\"Standard_Parallel_1\",0],PARAMETER[\"Auxiliary_Sphere_Type\",0],UNIT[\"Meter\",1]]",
                    f(self.a),
                    f(self.rf)
                )
            }
            Projection::LongLat => {
                let (geog, datum, spheroid) = self.wkt_names();
                format!(
                    "GEOGCS[\"{geog}\",DATUM[\"{datum}\",SPHEROID[\"{spheroid}\",{},{}]],PRIMEM[\"Greenwich\",0],UNIT[\"degree\",0.0174532925199433,AUTHORITY[\"EPSG\",\"9122\"]],AXIS[\"Latitude\",NORTH],AXIS[\"Longitude\",EAST]]",
                    f(self.a),
                    f(self.rf)
                )
            }
            Projection::Albers {
                lat_1,
                lat_2,
                lat_0,
                lon_0,
                x_0,
                y_0,
            } => format!(
                "PROJCS[\"Albers Equal Area Conic\",\n{}    PROJECTION[\"Albers_Conic_Equal_Area\"],\n{}{}{}{}{}{}{tail}",
                self.pretty_geogcs(),
                p("latitude_of_center", lat_0),
                p("longitude_of_center", lon_0),
                p("standard_parallel_1", lat_1),
                p("standard_parallel_2", lat_2),
                p("false_easting", x_0),
                p("false_northing", y_0),
            ),
            Projection::Lambert {
                lat_1,
                lat_2,
                lat_0,
                lon_0,
                k_0,
                x_0,
                y_0,
            } => {
                if k_0 == 1.0 {
                    format!(
                        "PROJCS[\"Lambert Conformal Conic 2SP\",\n{}    PROJECTION[\"Lambert_Conformal_Conic_2SP\"],\n{}{}{}{}{}{}{tail}",
                        self.pretty_geogcs(),
                        p("latitude_of_origin", lat_0),
                        p("central_meridian", lon_0),
                        p("standard_parallel_1", lat_1),
                        p("standard_parallel_2", lat_2),
                        p("false_easting", x_0),
                        p("false_northing", y_0),
                    )
                } else {
                    format!(
                        "PROJCS[\"Lambert Conformal Conic 1SP\",\n{}    PROJECTION[\"Lambert_Conformal_Conic_1SP\"],\n{}{}{}{}{}{tail}",
                        self.pretty_geogcs(),
                        p("latitude_of_origin", lat_0),
                        p("central_meridian", lon_0),
                        p("scale_factor", k_0),
                        p("false_easting", x_0),
                        p("false_northing", y_0),
                    )
                }
            }
            Projection::Stereographic {
                lat_0,
                lon_0,
                k_0,
                lat_ts,
                x_0,
                y_0,
            } => {
                // GDAL's SetPS writes no scale factor for a true-scale latitude.
                let (method, lat, scale) = match lat_ts {
                    Some(ts) => ("Polar_Stereographic", ts, String::new()),
                    None => ("Stereographic", lat_0, p("scale_factor", k_0)),
                };
                format!(
                    "PROJCS[\"unnamed\",\n{}    PROJECTION[\"{method}\"],\n{}{}{scale}{}{}{tail}",
                    self.pretty_geogcs(),
                    p("latitude_of_origin", lat),
                    p("central_meridian", lon_0),
                    p("false_easting", x_0),
                    p("false_northing", y_0),
                )
            }
        }
    }
}

#[cfg(test)]
mod albers_words {
    use super::*;

    // Each from Vortex 1.1.0's records: the `albers_odd*` cases.
    #[test]
    fn numbers_are_worded_as_vortex_stores_them() {
        for (v, want) in [
            (29.3f32, "29.2999992370606"),
            (45.7, "45.7000007629395"),
            (23.1, "23.1000003814697"),
            (-100.123, "-100.123001098633"),
            (-30.7, "-30.7000007629395"),
            (-10.9, "-10.8999996185303"),
            (134.56, "134.559997558594"),
            (-25.25, "-25.25"),
            (33.33, "33.3300018310547"),
            (41.17, "41.1699981689453"),
            (-77.77, "-77.7699966430664"),
            (37.37, "37.3699989318848"),
            (8.1, "8.10000038146973"),
            (60.9, "60.9000015258789"),
            (12.345, "12.3450002670288"),
            (-5.55, "-5.55000019073486"),
        ] {
            assert_eq!(albers_angle_text(v), want, "{v}");
        }
        for (v, want) in [
            (500000.5f32, "500000.5"),
            (-1234.75, "-1234.75"),
            (123456.79, "123456.7890625"),
            (98765.43, "98765.4296875"),
            (0.1, "0.100000001490116"),
            (-0.3, "-0.300000011920929"),
            (333333.34, "333333.34375"),
            (285714.28, "285714.28125"),
        ] {
            assert_eq!(albers_metres_text(v), want, "{v}");
        }
    }

    #[test]
    fn g_is_c_formatting() {
        for (v, n, want) in [
            (0.0, 15, "0"),
            (23.0, 15, "23"),
            (-100.0, 15, "-100"),
            (500000.0, 15, "500000"),
            (29.5, 15, "29.5"),
            (f64::from(29.3f32), 15, "29.2999992370605"),
            (f64::from(29.3f32), 16, "29.29999923706055"),
            (f64::from(-100.123f32), 15, "-100.123001098633"),
            (0.0001, 15, "0.0001"),
            (0.00001, 15, "1e-05"),
            (1e15, 15, "1e+15"),
            (1e15, 16, "1000000000000000"),
            (123456789012345.0, 15, "123456789012345"),
            (1.5e20, 15, "1.5e+20"),
        ] {
            assert_eq!(fmt_g(v, n), want, "{v} {n}");
        }
    }

    // `fmt_g15` serves other numbers, and is not changed by the above.
    #[test]
    fn other_wkt_numbers_are_as_they_were() {
        assert_eq!(fmt_g15(f64::from(29.3f32)), "29.2999992370605");
        assert_eq!(fmt_g15(6378137.0), "6378137");
        assert_eq!(fmt_g15(298.257222101), "298.257222101");
    }
}
