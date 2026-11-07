//! Zone geometry, and the OGR steps that decide how rings become polygons.
//!
//! Sources, GDAL 3.2.1: `ogr/ogrgeometryfactory.cpp` `organizePolygons`, `ogr/ogrlinearring.cpp`
//! `isClockwise`, `isPointInRing`, `isPointOnRingBoundary`.

use crate::error::VortexError;

/// One coordinate pair.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Pt {
    pub x: f64,
    pub y: f64,
}

/// One part of a feature, as GDAL's rasterizer treats it after `gv_rasterize_one_shape` has
/// recursed through multi-geometries and collections. Multi-points are one [`Part::Point`] each.
#[derive(Debug, Clone, PartialEq)]
pub enum Part {
    Point(Pt),
    Line(Vec<Pt>),
    /// Exterior ring first, then interior rings.
    Polygon(Vec<Vec<Pt>>),
}

impl Part {
    pub(crate) fn points_mut(
        &mut self,
        mut each: impl FnMut(&mut Pt) -> Result<(), VortexError>,
    ) -> Result<(), VortexError> {
        match self {
            Part::Point(p) => each(p),
            Part::Line(l) => l.iter_mut().try_for_each(each),
            Part::Polygon(rings) => {
                for ring in rings {
                    for p in ring {
                        each(p)?;
                    }
                }
                Ok(())
            }
        }
    }

    pub(crate) fn for_each_point(&self, mut each: impl FnMut(Pt)) {
        match self {
            Part::Point(p) => each(*p),
            Part::Line(l) => l.iter().for_each(|p| each(*p)),
            Part::Polygon(rings) => rings.iter().flatten().for_each(|p| each(*p)),
        }
    }
}

/// `Vec::with_capacity` that reports failure instead of aborting the process, as a hostile count
/// in a file or a truly large zone could otherwise do.
pub(crate) fn try_vec<T>(capacity: usize, what: &str) -> Result<Vec<T>, VortexError> {
    let mut v = Vec::new();
    v.try_reserve_exact(capacity)
        .map_err(|_| VortexError::Refused {
            what: what.to_string(),
            reason: format!("{capacity} items could not be allocated"),
        })?;
    Ok(v)
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct Envelope {
    pub min_x: f64,
    pub max_x: f64,
    pub min_y: f64,
    pub max_y: f64,
}

impl Envelope {
    fn of(ring: &[Pt]) -> Envelope {
        let mut e = Envelope {
            min_x: f64::INFINITY,
            max_x: f64::NEG_INFINITY,
            min_y: f64::INFINITY,
            max_y: f64::NEG_INFINITY,
        };
        for p in ring {
            e.grow(*p);
        }
        e
    }

    pub(crate) fn empty() -> Envelope {
        Envelope::of(&[])
    }

    pub(crate) fn grow(&mut self, p: Pt) {
        // OGRSimpleCurve::getEnvelope: plain comparisons, in this order.
        if p.x < self.min_x {
            self.min_x = p.x;
        }
        if p.x > self.max_x {
            self.max_x = p.x;
        }
        if p.y < self.min_y {
            self.min_y = p.y;
        }
        if p.y > self.max_y {
            self.max_y = p.y;
        }
    }

    // ogr_core.h OGREnvelope::Contains.
    fn contains(&self, o: &Envelope) -> bool {
        self.min_x <= o.min_x
            && self.min_y <= o.min_y
            && self.max_x >= o.max_x
            && self.max_y >= o.max_y
    }
}

/// `OGRLinearRing::isClockwise`. A ring is expected to repeat its first point at its end.
pub(crate) fn is_clockwise(ring: &[Pt]) -> bool {
    const EPSILON: f64 = 1.0e-5;
    let n = ring.len();
    if n < 2 {
        return true;
    }
    let near = |a: Pt, b: Pt| (a.x - b.x).abs() < EPSILON && (a.y - b.y).abs() < EPSILON;
    let mut fallback = false;
    // Lowest rightmost vertex.
    let mut v = 0;
    for i in 1..n.saturating_sub(1) {
        if ring[i].y < ring[v].y || (ring[i].y == ring[v].y && ring[i].x > ring[v].x) {
            v = i;
            fallback = false;
        } else if ring[i].y == ring[v].y && ring[i].x == ring[v].x {
            fallback = true;
        }
    }
    let previous = if v == 0 { n - 2 } else { v - 1 };
    if near(ring[previous], ring[v]) {
        fallback = true;
    }
    let dx0 = ring[previous].x - ring[v].x;
    let dy0 = ring[previous].y - ring[v].y;
    let following = if v + 1 >= n - 1 { 0 } else { v + 1 };
    if near(ring[following], ring[v]) {
        fallback = true;
    }
    let dx1 = ring[following].x - ring[v].x;
    let dy1 = ring[following].y - ring[v].y;
    let cross = dx1 * dy0 - dx0 * dy1;
    if !fallback {
        if cross > 0.0 {
            return false;
        } else if cross < 0.0 {
            return true;
        }
    }
    // The degenerate case: Green's formula.
    let mut sum = ring[0].x * (ring[1].y - ring[n - 1].y);
    for i in 1..n - 1 {
        sum += ring[i].x * (ring[i + 1].y - ring[i - 1].y);
    }
    sum += ring[n - 1].x * (ring[0].y - ring[n - 2].y);
    sum < 0.0
}

// OGRCurve::get_Area for a ring. Only ever compared.
fn ring_area(ring: &[Pt]) -> f64 {
    let n = ring.len();
    if n < 3 {
        return 0.0;
    }
    let mut sum = ring[0].x * (ring[1].y - ring[n - 1].y);
    for i in 1..n - 1 {
        sum += ring[i].x * (ring[i + 1].y - ring[i - 1].y);
    }
    sum += ring[n - 1].x * (ring[0].y - ring[n - 2].y);
    0.5 * sum.abs()
}

fn in_ring(ring: &[Pt], p: Pt) -> bool {
    if ring.len() < 4 {
        return false;
    }
    let mut crossings = 0u64;
    let mut prev_x = ring[0].x - p.x;
    let mut prev_y = ring[0].y - p.y;
    for q in &ring[1..] {
        let x1 = q.x - p.x;
        let y1 = q.y - p.y;
        let (x2, y2) = (prev_x, prev_y);
        if (y1 > 0.0 && y2 <= 0.0) || (y2 > 0.0 && y1 <= 0.0) {
            let at = (x1 * y2 - x2 * y1) / (y2 - y1);
            if 0.0 < at {
                crossings += 1;
            }
        }
        prev_x = x1;
        prev_y = y1;
    }
    crossings % 2 == 1
}

fn on_ring(ring: &[Pt], p: Pt) -> bool {
    if ring.len() < 4 {
        return false;
    }
    let mut prev_x = p.x - ring[0].x;
    let mut prev_y = p.y - ring[0].y;
    for i in 1..ring.len() {
        let dx1 = p.x - ring[i].x;
        let dy1 = p.y - ring[i].y;
        let (dx2, dy2) = (prev_x, prev_y);
        if dx1 * dy2 - dx2 * dy1 == 0.0 && !(dx1 == dx2 && dy1 == dy2) {
            let sx = ring[i].x - ring[i - 1].x;
            let sy = ring[i].y - ring[i - 1].y;
            let along = dx2 * sx + dy2 * sy;
            if along >= 0.0 && along <= sx * sx + sy * sy {
                return true;
            }
        }
        prev_x = dx1;
        prev_y = dy1;
    }
    false
}

// The part of `organizePolygons` for "i's first point lies on j's boundary".
fn inside_by_other_points(i: &[Pt], j: &[Pt]) -> bool {
    let n = i.len();
    let mut previous = i[0];
    let mut k = 1;
    while k < n {
        let point = i[k];
        if point.x == previous.x && point.y == previous.y {
            k += 1;
            continue;
        }
        if on_ring(j, point) {
            // On the boundary too: look further.
        } else {
            return in_ring(j, point);
        }
        previous = point;
        k += 1;
    }
    if n <= 2 {
        return false;
    }
    // Every point is on the boundary: try the middle of each segment.
    let mut previous = i[0];
    for point in &i[1..] {
        if point.x == previous.x && point.y == previous.y {
            continue;
        }
        let middle = Pt {
            x: (point.x + previous.x) / 2.0,
            y: (point.y + previous.y) / 2.0,
        };
        if on_ring(j, middle) {
            // Look further.
        } else {
            return in_ring(j, middle);
        }
        previous = *point;
    }
    false
}

/// `OGRGeometryFactory::organizePolygons` with `METHOD=ONLY_CCW` and the fast version, as
/// `SHPReadOGRObject` calls it for a shape with more than one ring. Each result is one polygon,
/// exterior ring first. Which rings share a polygon decides the mask: rings of one polygon fill by
/// even-odd, and separate polygons are burned one after another.
pub(crate) fn organize_rings(mut rings: Vec<Vec<Pt>>) -> Vec<Vec<Vec<Pt>>> {
    if rings.len() <= 1 {
        return if rings.is_empty() {
            Vec::new()
        } else {
            vec![rings]
        };
    }
    struct Info {
        index: usize,
        envelope: Envelope,
        area: f64,
        cw: bool,
        top: bool,
        enclosing: Option<usize>,
    }
    let mut mixed = false;
    let mut cw_count = 0;
    let mut cw_index = 0;
    let mut infos: Vec<Info> = Vec::with_capacity(rings.len());
    for (index, ring) in rings.iter().enumerate() {
        let mut info = Info {
            index,
            envelope: Envelope::of(ring),
            area: 0.0,
            cw: false,
            top: false,
            enclosing: None,
        };
        if ring.len() >= 4 {
            info.area = ring_area(ring);
            info.cw = is_clockwise(ring);
            if info.cw {
                cw_index = index;
                cw_count += 1;
            }
        } else {
            mixed = true;
        }
        infos.push(info);
    }
    // One outer ring: every other ring is taken to be inside it.
    if cw_count == 1 {
        let outer = std::mem::take(&mut rings[cw_index]);
        let mut polygon = Vec::with_capacity(rings.len());
        polygon.push(outer);
        for (i, ring) in rings.into_iter().enumerate() {
            if i != cw_index {
                polygon.push(ring);
            }
        }
        return vec![polygon];
    }
    if mixed {
        return rings.into_iter().map(|r| vec![r]).collect();
    }
    // STEP 1. `qsort` here, and stable below; equal areas are the only place the two can differ.
    infos.sort_by(|a, b| {
        b.area
            .partial_cmp(&a.area)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    infos[0].top = true;
    // STEP 2.
    for i in 1..infos.len() {
        if infos[i].cw {
            infos[i].top = true;
            continue;
        }
        let mut found = false;
        for j in (0..i).rev() {
            if !infos[j].cw {
                continue;
            }
            let mut inside = false;
            if infos[j].envelope.contains(&infos[i].envelope) {
                let ring_i = &rings[infos[i].index];
                let ring_j = &rings[infos[j].index];
                if j == 0 {
                    inside = true;
                } else if on_ring(ring_j, ring_i[0]) {
                    inside = inside_by_other_points(ring_i, ring_j);
                } else if in_ring(ring_j, ring_i[0]) {
                    inside = true;
                }
            }
            if inside {
                if infos[j].top {
                    infos[i].top = false;
                    infos[i].enclosing = Some(infos[j].index);
                } else {
                    infos[i].top = true;
                }
                found = true;
                break;
            }
        }
        if !found {
            infos[i].top = true;
        }
    }
    // STEP 3 to 5.
    infos.sort_by_key(|info| info.index);
    let mut holes: Vec<Vec<usize>> = vec![Vec::new(); infos.len()];
    for info in &infos {
        if let (false, Some(outer)) = (info.top, info.enclosing) {
            holes[outer].push(info.index);
        }
    }
    let mut polygons = Vec::new();
    for info in &infos {
        if info.top {
            let mut polygon = vec![std::mem::take(&mut rings[info.index])];
            for &hole in &holes[info.index] {
                polygon.push(std::mem::take(&mut rings[hole]));
            }
            polygons.push(polygon);
        }
    }
    polygons
}
