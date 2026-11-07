//! Envelope reprojection as Vortex does it (`geo/Reprojector.java`, `reproject(Envelope)`):
//! the four corners go through the coordinate transformation and the result is their bounding
//! box. There is no densification along the edges.

use super::{CrsDef, DatumPolicy, Transformer};
use crate::error::VortexError;

/// An axis-aligned box in a CRS's units.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Envelope {
    pub min_x: f64,
    pub max_x: f64,
    pub min_y: f64,
    pub max_y: f64,
}

/// The corners' bounding box in `to`. A corner that fails to transform is a typed refusal.
pub fn reproject_envelope(
    envelope: Envelope,
    from: &CrsDef,
    to: &CrsDef,
    policy: DatumPolicy,
) -> Result<Envelope, VortexError> {
    let t = Transformer::new(from, to, policy)?;
    let corners = [
        (envelope.min_x, envelope.min_y),
        (envelope.min_x, envelope.max_y),
        (envelope.max_x, envelope.max_y),
        (envelope.max_x, envelope.min_y),
    ];
    let mut out: Option<Envelope> = None;
    for (x, y) in corners {
        let (px, py) = t.transform(x, y).map_err(|_| VortexError::Refused {
            what: "envelope".into(),
            reason: format!("corner ({x}, {y}) does not transform"),
        })?;
        out = Some(match out {
            None => Envelope {
                min_x: px,
                max_x: px,
                min_y: py,
                max_y: py,
            },
            Some(e) => Envelope {
                min_x: e.min_x.min(px),
                max_x: e.max_x.max(px),
                min_y: e.min_y.min(py),
                max_y: e.max_y.max(py),
            },
        });
    }
    out.ok_or_else(|| VortexError::Unsupported {
        what: "empty envelope".into(),
    })
}
