//! What `hayro-jpeg2000` 0.4.1 will allocate for a code stream, worked out before it is entered.
//!
//! The decoder's allocations are infallible, so a request the system refuses aborts the process,
//! which `catch_unwind` cannot catch. It sizes them from headers a few bytes long. This module
//! reads those headers first and refuses a stream whose working memory is over
//! [`Limits::max_jpeg2000_bytes`], then reserves that much and releases it, so that an ordinary
//! process memory limit gives a typed error. Between the release and the decoder another thread
//! can take the memory, and a system that overcommits grants a reservation it may not honour.
//! Neither is closed here; nothing short of a fallible decoder closes them.
//!
//! **The check reads what the decoder reads.** A walk that disagreed with the decoder's about
//! where a marker segment ends could be shown one set of parameters while the decoder used another.
//! So this walks the markers in the decoder's order (`j2c::codestream::read_header`,
//! `j2c::tile::parse_tile_part`), advances by the same rules, and refuses every stream whose
//! declared length disagrees with those rules (`size_marker_inner`, `cod_marker` and `sot_marker`
//! ignore their length fields). It also refuses what could change the parameters after it has read
//! them: COC and QCC, overrides in a tile-part header, and packed packet headers. No sampled product
//! has any of these, and GRIB2 needs none: one grey component in a bare code stream.
//!
//! Offsets are refused too. `ResolutionTile::precincts` adds without checking near 2^32, and the
//! wrapped rectangle in `build_precincts` would make a code-block count no formula here predicts.

use super::Limits;
use crate::error::VortexError;

const SOT: u8 = 0x90;
const SOD: u8 = 0x93;
const SIZ: u8 = 0x51;
const COD: u8 = 0x52;
const QCD: u8 = 0x5C;
const TLM: u8 = 0x55;
const PLT: u8 = 0x58;
const CRG: u8 = 0x63;
const COM: u8 = 0x64;

/// `j2c::codestream::MAX_LAYER_COUNT`.
const MAX_LAYERS: u16 = 32;
/// `j2c::codestream::MAX_RESOLUTION_COUNT`, which bounds decomposition levels.
const MAX_LEVELS: u8 = 32;
/// `segment::resolve_segments`: `MAX_CODING_PASSES`, one segment each at most.
const MAX_SEGMENTS_PER_BLOCK: u64 = 94;

// Sizes of the decoder's private types on a 64-bit target, by reading them. The pin on
// `hayro-jpeg2000` holds them, and `tests/grib_jpeg_budget.rs` checks the total against an allocator
// on streams built to stress each one.
const TILE: u64 = 96; // j2c::tile::Tile
const COMPONENT_INFO: u64 = 96; // j2c::codestream::ComponentInfo
const TILE_PART: u64 = 64; // j2c::tile::TilePart
const PRECINCT: u64 = 48; // j2c::build::Precinct
const CODE_BLOCK: u64 = 56; // j2c::build::CodeBlock
const LAYER: u64 = 24; // j2c::build::Layer
const SUB_BAND: u64 = 56; // j2c::build::SubBand
const DECOMPOSITION: u64 = 40; // j2c::build::Decomposition
const TILE_DECOMPOSITIONS: u64 = 32; // j2c::decode::TileDecompositions
const TAG_NODE: u64 = 48; // j2c::tag_tree::TagNode
const SEGMENT: u64 = 24; // j2c::build::Segment
const PRECINCT_STORE: u64 = 24; // j2c::progression::PrecinctStore
const STEP_SIZE: u64 = 4; // j2c::codestream::StepSize
const F32: u64 = 4;
/// `math::SimdBuffer` pads to a multiple of this.
const SIMD_WIDTH: u64 = 8;
/// A heap block's own overhead, for the small per-tile vectors.
const BLOCK: u64 = 32;
/// Headers, one code block's bit-plane buffers (at most 2^12 coefficients), iterators.
const FIXED: u64 = 1 << 20;
/// A `Vec` grown by pushes: capacity under twice the length, and the old block alive while moved.
const GROWN: u64 = 3;

/// `TagNode::build` visits four children of every node, present or not, so a tree's cost is 4^level
/// however few nodes it stores. A thin image costs almost no memory and hours of this. At 2^32 a
/// tile takes seconds. A 7000 x 3500 grid with 64-sample blocks needs under 2^18.
const MAX_TREE_VISITS: u64 = 1 << 32;

/// What [`scan`] read.
#[derive(Debug)]
pub(super) struct Stream {
    pub width: u32,
    pub height: u32,
    pub depth: u8,
    tile_width: u64,
    tile_height: u64,
    tiles: u64,
    tile_parts: u64,
    step_sizes: u64,
    order: u8,
    layers: u64,
    levels: u32,
    block_width: u32,
    block_height: u32,
    multiple_segments: bool,
    /// (PPx, PPy) per resolution level.
    precincts: Vec<(u32, u32)>,
    bytes: u64,
}

fn malformed(message: impl Into<String>) -> VortexError {
    VortexError::Decode {
        format: "JPEG 2000",
        message: message.into(),
    }
}

fn refused(reason: impl Into<String>) -> VortexError {
    VortexError::Refused {
        what: "JPEG 2000 code stream".to_string(),
        reason: reason.into(),
    }
}

struct Cursor<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl Cursor<'_> {
    fn take(&mut self, n: usize) -> Result<&[u8], VortexError> {
        let end = self
            .at
            .checked_add(n)
            .filter(|&end| end <= self.bytes.len())
            .ok_or_else(|| malformed("code stream ends inside a header"))?;
        let taken = &self.bytes[self.at..end];
        self.at = end;
        Ok(taken)
    }

    fn u8(&mut self) -> Result<u8, VortexError> {
        Ok(self.take(1)?[0])
    }

    fn u16(&mut self) -> Result<u16, VortexError> {
        let b = self.take(2)?;
        Ok(u16::from_be_bytes([b[0], b[1]]))
    }

    fn u32(&mut self) -> Result<u32, VortexError> {
        let b = self.take(4)?;
        Ok(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    }

    /// `BitReader::peek_marker`.
    fn peek_marker(&self) -> Option<u8> {
        match self.bytes.get(self.at..self.at.checked_add(2)?) {
            Some([0xFF, marker]) => Some(*marker),
            _ => None,
        }
    }

    fn marker(&mut self) -> Result<u8, VortexError> {
        let marker = self
            .peek_marker()
            .ok_or_else(|| malformed("expected a marker"))?;
        self.at += 2;
        Ok(marker)
    }

    /// `codestream::skip_marker_segment`.
    fn skip_segment(&mut self) -> Result<(), VortexError> {
        let length = self
            .u16()?
            .checked_sub(2)
            .ok_or_else(|| malformed("marker segment length"))?;
        self.take(usize::from(length))?;
        Ok(())
    }
}

/// `ceil(value / 2^shift)`.
fn ceil_shift(value: u64, shift: u32) -> u64 {
    value.div_ceil(1u64 << shift)
}

struct Siz {
    width: u32,
    height: u32,
    depth: u8,
    tile_width: u32,
    tile_height: u32,
}

/// `codestream::size_marker_inner`, after its marker.
fn read_siz(cursor: &mut Cursor) -> Result<Siz, VortexError> {
    let length = cursor.u16()?;
    let _capabilities = cursor.u16()?;
    let width = cursor.u32()?;
    let height = cursor.u32()?;
    let x_offset = cursor.u32()?;
    let y_offset = cursor.u32()?;
    let tile_width = cursor.u32()?;
    let tile_height = cursor.u32()?;
    let tile_x_offset = cursor.u32()?;
    let tile_y_offset = cursor.u32()?;
    let components = cursor.u16()?;
    if components != 1 {
        return Err(refused("JPEG 2000 with more than one component"));
    }
    if length != 41 {
        return Err(refused(format!("SIZ length {length}")));
    }
    let depth = (cursor.u8()? & 0x7F) + 1;
    let x_step = cursor.u8()?;
    let y_step = cursor.u8()?;
    if (x_offset, y_offset, tile_x_offset, tile_y_offset) != (0, 0, 0, 0) {
        return Err(refused("image or tile offsets"));
    }
    if (x_step, y_step) != (1, 1) {
        return Err(refused(format!(
            "component subsampling {x_step} x {y_step}"
        )));
    }
    if width == 0 || height == 0 || tile_width == 0 || tile_height == 0 {
        return Err(malformed("empty image or tile"));
    }
    Ok(Siz {
        width,
        height,
        depth,
        tile_width,
        tile_height,
    })
}

struct Cod {
    order: u8,
    layers: u16,
    levels: u8,
    block_width: u8,
    block_height: u8,
    multiple_segments: bool,
    precincts: Vec<(u32, u32)>,
}

/// `codestream::cod_marker` and `coding_style_parameters`, after its marker.
fn read_cod(cursor: &mut Cursor) -> Result<Cod, VortexError> {
    let length = cursor.u16()?;
    let flags = cursor.u8()?;
    let order = cursor.u8()?;
    let layers = cursor.u16()?;
    let _mct = cursor.u8()?;
    let levels = cursor.u8()?;
    let block_width = cursor.u8()?.saturating_add(2);
    let block_height = cursor.u8()?.saturating_add(2);
    let style = cursor.u8()?;
    let transformation = cursor.u8()?;
    if order > 4 || transformation > 1 {
        return Err(malformed("COD values"));
    }
    if layers == 0 || layers > MAX_LAYERS {
        return Err(malformed(format!("{layers} layers")));
    }
    if levels > MAX_LEVELS {
        return Err(malformed(format!("{levels} decomposition levels")));
    }
    if block_width > 10 || block_height > 10 || block_width + block_height > 12 {
        return Err(malformed("code-block size"));
    }
    let resolutions = usize::from(levels) + 1;
    let has_precincts = flags & 1 != 0;
    let expected = 12 + if has_precincts { resolutions as u16 } else { 0 };
    if length != expected {
        return Err(refused(format!("COD length {length}, expected {expected}")));
    }
    let mut precincts = Vec::with_capacity(resolutions);
    for resolution in 0..resolutions {
        // "Entropy coder, precincts with PPx = 15 and PPy = 15" without any.
        let (x, y) = if has_precincts {
            let size = cursor.u8()?;
            (u32::from(size & 0xF), u32::from(size >> 4))
        } else {
            (15, 15)
        };
        // ResolutionTile::precincts takes PPx - 1 above resolution 0.
        if resolution > 0 && (x == 0 || y == 0) {
            return Err(malformed("zero precinct exponent above resolution 0"));
        }
        precincts.push((x, y));
    }
    Ok(Cod {
        order,
        layers,
        levels,
        block_width,
        block_height,
        // CodeBlockStyle: selective arithmetic bypass, or termination on each pass.
        multiple_segments: style & 0b101 != 0,
        precincts,
    })
}

/// `codestream::qcd_marker` and `quantization_parameters`, after its marker. Returns the step sizes.
fn read_qcd(cursor: &mut Cursor) -> Result<u64, VortexError> {
    let length = cursor.u16()?;
    let style = cursor.u8()? & 0x1F;
    let remaining = usize::from(
        length
            .checked_sub(3)
            .ok_or_else(|| malformed("QCD length"))?,
    );
    // Style 1 reads one value whatever the length says, and style 2 ignores an odd byte.
    let steps = match (style, remaining) {
        (0, n) => n,
        (1, 2) => 1,
        (2, n) if n % 2 == 0 => n / 2,
        _ => return Err(refused(format!("QCD style {style} with length {length}"))),
    };
    cursor.take(remaining)?;
    Ok(steps as u64)
}

/// Reads every header the decoder will act on. Refuses what the module header names.
pub(super) fn scan(payload: &[u8]) -> Result<Stream, VortexError> {
    // lib.rs: JP2_MAGIC. A wrapper brings palette and channel-definition boxes, sized from the
    // boxes. GRIB2 template 5.40 is a code stream, and no sampled product wraps one.
    if payload.starts_with(b"\x00\x00\x00\x0C\x6A\x50\x20\x20") {
        return Err(refused("a JP2 file rather than a code stream"));
    }
    // lib.rs: CODESTREAM_MAGIC.
    if !payload.starts_with(b"\xFF\x4F\xFF\x51") {
        return Err(malformed("no code stream signature"));
    }
    let mut cursor = Cursor {
        bytes: payload,
        at: 2,
    };
    if cursor.marker()? != SIZ {
        return Err(malformed("expected SIZ"));
    }
    let siz = read_siz(&mut cursor)?;

    // codestream::read_header's loop: anything it would act on that is not here is refused.
    let (mut cod, mut steps) = (None, None);
    loop {
        match cursor.marker()? {
            SOT => {
                cursor.at -= 2;
                break;
            }
            COD if cod.is_none() => cod = Some(read_cod(&mut cursor)?),
            QCD if steps.is_none() => steps = Some(read_qcd(&mut cursor)?),
            TLM | COM | CRG => cursor.skip_segment()?,
            other => {
                return Err(refused(format!(
                    "marker 0xFF{other:02X} in the main header"
                )))
            }
        }
    }
    let cod = cod.ok_or_else(|| malformed("no COD"))?;
    let steps = steps.ok_or_else(|| malformed("no QCD"))?;

    // SizeData::num_tiles, with the offsets refused above.
    let tiles = u64::from(siz.width.div_ceil(siz.tile_width))
        * u64::from(siz.height.div_ceil(siz.tile_height));
    // tile::parse builds every tile whether or not it has data, and each needs 14 bytes here.
    let remaining = (payload.len() - cursor.at) as u64;
    if tiles > remaining / 14 {
        return Err(refused(format!(
            "{tiles} tiles in {remaining} bytes, so some have no data"
        )));
    }
    let mut seen = Vec::new();
    seen.try_reserve_exact(tiles as usize)
        .map_err(|_| refused("could not allocate the tile list"))?;
    seen.resize(tiles as usize, false);
    let mut tile_parts = 0u64;
    while cursor.peek_marker() == Some(SOT) {
        read_tile_part(&mut cursor, &mut seen)?;
        tile_parts += 1;
    }
    if seen.iter().any(|&s| !s) {
        return Err(refused("a tile with no tile part"));
    }

    Ok(Stream {
        width: siz.width,
        height: siz.height,
        depth: siz.depth,
        tile_width: u64::from(siz.tile_width.min(siz.width)),
        tile_height: u64::from(siz.tile_height.min(siz.height)),
        tiles,
        tile_parts,
        step_sizes: steps,
        order: cod.order,
        layers: u64::from(cod.layers),
        levels: u32::from(cod.levels),
        block_width: u32::from(cod.block_width),
        block_height: u32::from(cod.block_height),
        multiple_segments: cod.multiple_segments,
        precincts: cod.precincts,
        bytes: payload.len() as u64,
    })
}

/// `tile::parse_tile_part`, with every path the non-strict decoder would tolerate refused.
fn read_tile_part(cursor: &mut Cursor, seen: &mut [bool]) -> Result<(), VortexError> {
    cursor.marker()?;
    let length = cursor.u16()?;
    if length != 10 {
        return Err(refused(format!("SOT length {length}")));
    }
    let tile = cursor.u16()?;
    let declared = cursor.u32()?;
    let _part = cursor.u8()?;
    let _parts = cursor.u8()?;
    *seen
        .get_mut(usize::from(tile))
        .ok_or_else(|| malformed(format!("tile index {tile}")))? = true;
    let start = cursor.at;
    let data = if declared == 0 {
        cursor.bytes.len() - start
    } else {
        (declared as usize)
            .checked_sub(12)
            .ok_or_else(|| malformed("tile-part length"))?
    };
    loop {
        match cursor.marker()? {
            SOD => break,
            PLT | COM => cursor.skip_segment()?,
            other => {
                return Err(refused(format!(
                    "marker 0xFF{other:02X} in a tile-part header"
                )))
            }
        }
    }
    let used = cursor.at - start;
    let body = data
        .checked_sub(used)
        .ok_or_else(|| refused("tile-part header longer than its tile part"))?;
    cursor.take(body)?;
    Ok(())
}

/// Bytes the decoder and `decode_jpeg2000`'s copy of its samples hold at their peak, and the visits
/// `TagNode::build` makes. Counted for the largest tile, since `DecompositionStorage` keeps one set
/// of buffers for all of them.
fn working_memory(stream: &Stream) -> (u64, u64) {
    // A tile not at the origin can straddle one more cell of any grid; one at the origin cannot.
    let several = stream.tiles > 1;
    let slack = u64::from(several);
    let (mut precincts, mut sub_bands, mut blocks, mut position_entries) = (0u64, 0u64, 0u64, 0u64);
    let (mut roots, mut visits, mut top, mut second) = (0u64, 0u64, (0u64, 0u64), (0u64, 0u64));
    for (resolution, &(ppx, ppy)) in stream.precincts.iter().enumerate() {
        let level = stream.levels - resolution as u32;
        // ResolutionTile::new.
        let width = ceil_shift(stream.tile_width, level) + slack;
        let height = ceil_shift(stream.tile_height, level) + slack;
        second = top;
        top = (width, height);
        // ResolutionTile::num_precincts.
        let per_band =
            (ceil_shift(width, ppx) + slack).saturating_mul(ceil_shift(height, ppy) + slack);
        position_entries = position_entries.saturating_add(per_band);
        // ResolutionTile::code_block_width, sub_band_rect. Above resolution 0 a precinct in a
        // sub-band is half its size at the resolution.
        let halved = u32::from(resolution > 0);
        let bands = if resolution > 0 { 3 } else { 1 };
        let band_width = ceil_shift(width, halved) + slack;
        let band_height = ceil_shift(height, halved) + slack;
        let (cell_x, cell_y) = (ppx - halved, ppy - halved);
        let (block_x, block_y) = (
            stream.block_width.min(cell_x),
            stream.block_height.min(cell_y),
        );
        // Blocks partition a precinct and align to it, so counting them over the whole band counts
        // each once (build_precincts).
        let across = ceil_shift(band_width, block_x) + 1;
        let down = ceil_shift(band_height, block_y) + 1;
        blocks = blocks.saturating_add(across.saturating_mul(down).saturating_mul(bands));
        let band_precincts = per_band.saturating_mul(bands);
        precincts = precincts.saturating_add(band_precincts);
        sub_bands += bands;

        // TagTree::new: levels from the larger side of a precinct's blocks.
        let side = across
            .min((1u64 << (cell_x - block_x)) + 1)
            .max(down.min((1u64 << (cell_y - block_y)) + 1));
        let tree_levels = u64::from(side.next_power_of_two().ilog2());
        roots = roots.saturating_add(band_precincts.saturating_mul(tree_levels + 1));
        // (4^(L+1) - 1) / 3 visits, two trees a precinct.
        let tree_visits = ((1u128 << (2 * tree_levels + 2)) / 3 + 1) * 2;
        visits = visits.saturating_add(
            u64::try_from(tree_visits)
                .unwrap_or(u64::MAX)
                .saturating_mul(band_precincts),
        );
    }
    // A tree over n x m blocks stores at most Sum ceil(n/2^i) * ceil(m/2^i) nodes, under
    // 6 n m + L + 1, and an empty one stores its root.
    let nodes = blocks
        .saturating_mul(12)
        .saturating_add(roots.saturating_mul(2));
    // A segment takes at least the three bits of Lblock from a packet header, and a block has a
    // bounded number of them (segment::resolve_segments).
    let per_block = if stream.multiple_segments {
        MAX_SEGMENTS_PER_BLOCK
    } else {
        stream.layers
    };
    let segments = (stream.bytes.saturating_mul(8) / 3 + 1).min(blocks.saturating_mul(per_block));

    let storage = [
        (precincts, PRECINCT),
        (blocks, CODE_BLOCK + stream.layers * LAYER),
        (nodes, TAG_NODE),
        (sub_bands, SUB_BAND),
        (stream.levels.into(), DECOMPOSITION),
        (1, TILE_DECOMPOSITIONS),
        (segments, SEGMENT),
    ]
    .iter()
    .fold(0u64, |sum, &(n, size)| {
        sum.saturating_add(n.saturating_mul(size))
    })
    .saturating_mul(GROWN);

    // tile::parse: every tile clones the main header's component, with its two vectors.
    let per_tile = TILE
        + COMPONENT_INFO
        + 2 * (u64::from(stream.levels) + 1)
        + STEP_SIZE * stream.step_sizes
        + 4 * TILE_PART
        + 3 * BLOCK;
    let tiles = stream
        .tiles
        .saturating_mul(per_tile)
        .saturating_add(stream.tile_parts.saturating_mul(TILE_PART * GROWN));

    let cells = u64::from(stream.width) * u64::from(stream.height);
    let tile_area = stream.tile_width * stream.tile_height;
    // A later, larger tile moves a buffer while the old one lives.
    let moved = 1 + u64::from(several);
    let transform = (top.0 + 1)
        .saturating_mul(top.1 + 1)
        .saturating_add((second.0 + 1).saturating_mul(second.1 + 1));
    let samples = cells
        .saturating_add(SIMD_WIDTH) // DecoderContext::reset: the channel
        .saturating_add(cells) // decode_jpeg2000's copy
        .saturating_add(tile_area.saturating_mul(moved)) // build_decompositions: coefficients
        .saturating_add(transform.saturating_mul(moved)) // idwt::apply: output and scratch
        .saturating_mul(F32);
    // decode_component_tile_bit_planes joins one block's segments, at most the whole input.
    let joined = stream.bytes.saturating_mul(GROWN);
    // position_progression_common collects every precinct, and its stable sort adds a buffer.
    let entries = if stream.order >= 2 {
        position_entries.saturating_mul(PRECINCT_STORE * GROWN)
    } else {
        0
    };

    let total = [FIXED, storage, tiles, samples, joined, entries]
        .iter()
        .fold(0u64, |sum, &b| sum.saturating_add(b));
    (total, visits.saturating_mul(stream.tiles))
}

/// The bytes [`admit`] would require for this stream.
pub fn required_bytes(payload: &[u8]) -> Result<u64, VortexError> {
    Ok(working_memory(&scan(payload)?).0)
}

/// Call this before the decoder sees `payload`. On success the decoder's allocations fit the budget
/// and, at the moment of the check, the system could supply them.
pub(super) fn admit(payload: &[u8], limits: &Limits) -> Result<Stream, VortexError> {
    let stream = scan(payload)?;
    let (bytes, visits) = working_memory(&stream);
    if bytes > limits.max_jpeg2000_bytes {
        return Err(refused(format!(
            "decoding it takes {bytes} bytes, over the limit of {}",
            limits.max_jpeg2000_bytes
        )));
    }
    if visits > MAX_TREE_VISITS {
        return Err(refused(format!(
            "its tag trees take {visits} visits to build"
        )));
    }
    let bytes = usize::try_from(bytes).map_err(|_| refused("too large for this platform"))?;
    Vec::<u8>::new()
        .try_reserve_exact(bytes)
        .map_err(|_| refused(format!("could not allocate {bytes} bytes to decode it")))?;
    Ok(stream)
}
