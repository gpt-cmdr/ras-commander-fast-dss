//! Unpacks section 7 into the same `f32` bits as NetCDF-Java, the decoder under Vortex.
//!
//! Source: NetCDF-Java 5.5.3 `Grib2DataReader`, method named at each function. The arithmetic is
//! Java's `float` arithmetic in Java's order: `(R + X * EE) / DD`, the integer `X` converted to
//! `f32` first, and for template 5.3 the spatial differences summed in `f32`. That order is the
//! reason this file exists: the `grib` crate computes `(R + X * 2^E) * 10^-D`, which rounds
//! differently (pinned by a test in `tests/grib_fixtures.rs`). ecCodes works in `f64`.
//!
//! Hostile input gets a typed error. Cell counts are bounded by [`Limits`] before allocating, and
//! every group table is bounded by the bits present or the points declared.

use super::jpeg2000_budget;
use super::numbers::{int4, pow10_f32, pow2_f32};
use super::Limits;
use crate::error::VortexError;
use std::panic::{catch_unwind, AssertUnwindSafe};

/// Section 5, as `Grib2Drs` reads it.
#[derive(Debug, Clone)]
struct Packing {
    reference: f32,
    binary_scale: i32,
    decimal_scale: i32,
    bits: u8,
}

/// The extra fields of templates 5.2 and 5.3 (`Grib2Drs.Type2`, `Type3`).
#[derive(Debug, Clone)]
struct Complex {
    missing_management: u8,
    groups: i32,
    reference_widths: u8,
    bits_widths: u8,
    reference_lengths: i32,
    length_increment: u8,
    length_last: i32,
    bits_lengths: u8,
    order: u8,
    descriptor: u8,
}

fn decode_error(message: impl Into<String>) -> VortexError {
    VortexError::Decode {
        format: "GRIB2",
        message: message.into(),
    }
}

fn refused(reason: impl Into<String>) -> VortexError {
    VortexError::Refused {
        what: "GRIB2 packing".to_string(),
        reason: reason.into(),
    }
}

fn octet(section: &[u8], n: usize) -> Result<u8, VortexError> {
    section
        .get(n - 1)
        .copied()
        .ok_or_else(|| decode_error(format!("section ends before octet {n}")))
}

fn octets4(section: &[u8], n: usize) -> Result<[u8; 4], VortexError> {
    Ok([
        octet(section, n)?,
        octet(section, n + 1)?,
        octet(section, n + 2)?,
        octet(section, n + 3)?,
    ])
}

/// `GribNumbers.int2`: sign and magnitude, all ones undefined (-9999).
fn int2(a: u8, b: u8) -> i32 {
    if a == 0xff && b == 0xff {
        return -9999;
    }
    let (a, b) = (i32::from(a), i32::from(b));
    (1 - ((a & 128) >> 6)) * ((a & 127) << 8 | b)
}

fn read_packing(section: &[u8]) -> Result<Packing, VortexError> {
    // Grib2Drs.Type0: octets 12-15 reference (IEEE), 16-17 E, 18-19 D, 20 bits.
    Ok(Packing {
        reference: f32::from_be_bytes(octets4(section, 12)?),
        binary_scale: int2(octet(section, 16)?, octet(section, 17)?),
        decimal_scale: int2(octet(section, 18)?, octet(section, 19)?),
        bits: octet(section, 20)?,
    })
}

fn read_complex(section: &[u8], template: u16) -> Result<Complex, VortexError> {
    // Grib2Drs.Type2 and Type3. Octet 21 is the original field type; 22 starts Type2.
    let (order, descriptor) = if template == 3 {
        (octet(section, 48)?, octet(section, 49)?)
    } else {
        (0, 0)
    };
    Ok(Complex {
        missing_management: octet(section, 23)?,
        groups: int4(octets4(section, 32)?),
        reference_widths: octet(section, 36)?,
        bits_widths: octet(section, 37)?,
        reference_lengths: int4(octets4(section, 38)?),
        length_increment: octet(section, 42)?,
        length_last: int4(octets4(section, 43)?),
        bits_lengths: octet(section, 47)?,
        order,
        descriptor,
    })
}

/// `ucar.nc2.iosp.BitReader`, reading a slice. Reading past the end is an error (Java: EOF).
struct BitReader<'a> {
    data: &'a [u8],
    position: usize,
    current: u8,
    left: u32,
}

impl<'a> BitReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        BitReader {
            data,
            position: 0,
            current: 0,
            left: 0,
        }
    }

    fn bits_remaining(&self) -> u64 {
        (self.data.len() - self.position) as u64 * 8 + u64::from(self.left)
    }

    /// `BitReader.incrByte`: drop the rest of the current byte.
    fn align(&mut self) {
        self.left = 0;
    }

    /// `BitReader.bits2UInt`, including its `int` shift: at 32 bits the top byte is shifted as a
    /// Java `int` and sign-extends into the `long`.
    fn bits(&mut self, count: u32) -> Result<i64, VortexError> {
        let mut result: i64 = 0;
        let mut bits_left = count;
        while bits_left > 0 {
            if self.left == 0 {
                self.current = *self
                    .data
                    .get(self.position)
                    .ok_or_else(|| decode_error("section 7 ends before the data does"))?;
                self.position += 1;
                self.left = 8;
            }
            let size = bits_left.min(self.left);
            let mine = (u32::from(self.current) >> (self.left - size)) & ((1u32 << size) - 1);
            let shift = bits_left - size;
            result |= i64::from((mine as i32).wrapping_shl(shift));
            bits_left -= size;
            self.left -= size;
        }
        Ok(result)
    }
}

fn allocate(points: usize, limits: &Limits) -> Result<Vec<f32>, VortexError> {
    if points > limits.max_cells {
        return Err(VortexError::Refused {
            what: "GRIB2 grid".to_string(),
            reason: format!("{points} cells is over the limit of {}", limits.max_cells),
        });
    }
    let mut cells = Vec::new();
    cells
        .try_reserve_exact(points)
        .map_err(|_| VortexError::Refused {
            what: "GRIB2 grid".to_string(),
            reason: format!("could not allocate {points} cells"),
        })?;
    cells.resize(points, 0.0f32);
    Ok(cells)
}

/// `GribNumbers.testBitIsSet(bitmap[i / 8], i % 8)`.
fn bit_is_set(bitmap: &[u8], index: usize) -> bool {
    bitmap[index / 8] & (0x80 >> (index % 8)) != 0
}

/// Bits set among the first `total`, the values a bitmap says section 7 holds.
fn set_bits(bitmap: &[u8], total: usize) -> u64 {
    let whole = &bitmap[..total / 8];
    let mut count: u64 = whole.iter().map(|b| u64::from(b.count_ones())).sum();
    if !total.is_multiple_of(8) {
        count += u64::from((bitmap[total / 8] & (0xFFu8 << (8 - total % 8))).count_ones());
    }
    count
}

/// The inputs `Grib2DataReader`'s constructor takes.
pub(super) struct Inputs<'a> {
    pub representation: &'a [u8],
    pub bitmap_section: &'a [u8],
    /// Section 7 after its five-octet header.
    pub payload: &'a [u8],
    /// Section 3 octets 7-10.
    pub total_points: usize,
}

/// `Grib2DataReader.getData`, before `scanningModeCheck`, which the caller refuses or applies.
pub(super) fn unpack(inputs: &Inputs, limits: &Limits) -> Result<Vec<f32>, VortexError> {
    let representation = inputs.representation;
    let template = u16::from_be_bytes([octet(representation, 10)?, octet(representation, 11)?]);
    let data_points = usize::try_from(u32::from_be_bytes(octets4(representation, 6)?))
        .map_err(|_| decode_error("data point count"))?;
    let total = inputs.total_points;
    if total > limits.max_cells {
        return Err(VortexError::Refused {
            what: "GRIB2 grid".to_string(),
            reason: format!("{total} cells is over the limit of {}", limits.max_cells),
        });
    }
    if data_points > total {
        return Err(decode_error(format!(
            "section 5 declares {data_points} points in a grid of {total}"
        )));
    }

    // Grib2SectionBitMap.getBitmap.
    let indicator = octet(inputs.bitmap_section, 6)?;
    let bitmap = match indicator {
        255 => None,
        0 | 254 => {
            let bytes = &inputs.bitmap_section[6..];
            // Grib2DataReader.getData: a bitmap shorter than the grid is an error.
            if (bytes.len() as u64) * 8 < total as u64 {
                return Err(decode_error(format!(
                    "bitmap of {} bytes for {total} points",
                    bytes.len()
                )));
            }
            // Section 5 counts the values section 7 holds and the bitmap places them; each template
            // sizes its checks from section 5, so a disagreement must stop here, before any grid
            // is allocated. getData0 and getData2 would read on or leave zeros: observed behaviour
            // we did not implement.
            let set = set_bits(bytes, total);
            if set != data_points as u64 {
                return Err(decode_error(format!(
                    "bitmap sets {set} points, section 5 declares {data_points}"
                )));
            }
            Some(bytes)
        }
        other => {
            return Err(refused(format!(
                "bitmap indicator {other} refers to a predefined bitmap"
            )))
        }
    };

    let packing = read_packing(representation)?;
    let run = Run {
        packing: &packing,
        payload: inputs.payload,
        bitmap,
        indicator,
        total,
        data_points,
        limits,
    };
    match template {
        0 => run.simple(),
        2 => run.complex(&read_complex(representation, 2)?),
        3 => run.complex_differenced(&read_complex(representation, 3)?),
        40 => run.jpeg2000(),
        41 => run.png(),
        42 => Err(VortexError::Refused {
            what: "GRIB2 template 5.42 (CCSDS)".to_string(),
            reason:
                "NetCDF-Java 5.5.3 under Vortex has no reader for it, so there is no reference \
                     to match"
                    .to_string(),
        }),
        other => Err(VortexError::Unsupported {
            what: format!("GRIB2 data representation template 5.{other}"),
        }),
    }
}

struct Run<'a> {
    packing: &'a Packing,
    payload: &'a [u8],
    bitmap: Option<&'a [u8]>,
    indicator: u8,
    total: usize,
    data_points: usize,
    limits: &'a Limits,
}

impl Run<'_> {
    /// `(float) Math.pow(10, D)`, `(float) Math.pow(2, E)`.
    fn scales(&self) -> Result<(f32, f32), VortexError> {
        let dd = pow10_f32(self.packing.decimal_scale)
            .ok_or_else(|| refused(format!("decimal scale {}", self.packing.decimal_scale)))?;
        let ee = pow2_f32(self.packing.binary_scale);
        Ok((dd, ee))
    }

    /// `getData0`.
    fn simple(&self) -> Result<Vec<f32>, VortexError> {
        let nb = u32::from(self.packing.bits);
        if nb > 32 {
            return Err(refused(format!("{nb} bits per value")));
        }
        let (dd, ee) = self.scales()?;
        let r = self.packing.reference;
        let needed = (self.bitmap.map_or(self.total, |_| self.data_points) as u64)
            .checked_mul(u64::from(nb))
            .ok_or_else(|| decode_error("packed size overflows"))?;
        if needed > (self.payload.len() as u64).saturating_mul(8) {
            return Err(decode_error("section 7 is shorter than the packed values"));
        }
        let mut data = allocate(self.total, self.limits)?;
        let mut reader = BitReader::new(self.payload);
        match self.bitmap {
            None => {
                for cell in data.iter_mut() {
                    *cell = (r + reader.bits(nb)? as f32 * ee) / dd;
                }
            }
            Some(bitmap) => {
                for (i, cell) in data.iter_mut().enumerate() {
                    *cell = if bit_is_set(bitmap, i) {
                        (r + reader.bits(nb)? as f32 * ee) / dd
                    } else {
                        f32::NAN
                    };
                }
            }
        }
        Ok(data)
    }

    /// `nGroups0`. Its `mask` is an `int` taken from a signed byte and tested against 128.
    fn no_groups(&self, reference: f32) -> Result<Vec<f32>, VortexError> {
        let mut data = allocate(self.total, self.limits)?;
        match (self.indicator, self.bitmap) {
            (255, _) => data.fill(reference),
            (_, Some(bitmap)) => {
                let mut mask: i32 = 0;
                for (i, cell) in data.iter_mut().enumerate() {
                    if i & 7 == 0 {
                        mask = i32::from(bitmap[i / 8] as i8);
                    }
                    *cell = if mask & 128 == 0 { reference } else { f32::NAN };
                    mask = mask.wrapping_shl(1);
                }
            }
            _ => return Err(decode_error("no bitmap for a missing-value indicator")),
        }
        Ok(data)
    }

    fn group_count(&self, complex: &Complex) -> Result<usize, VortexError> {
        usize::try_from(complex.groups)
            .ok()
            .filter(|&n| n <= self.total)
            .ok_or_else(|| decode_error(format!("{} groups", complex.groups)))
    }

    /// Checks the bits exist before reserving anything, and a zero-bit table holds no storage.
    fn read_table(
        &self,
        reader: &mut BitReader,
        count: usize,
        bits: u8,
    ) -> Result<Table, VortexError> {
        let bits = u32::from(bits);
        if bits > 32 {
            return Err(refused(format!("{bits} bits in a group table")));
        }
        if bits == 0 {
            return Ok(Table(Vec::new()));
        }
        let needed = (count as u64)
            .checked_mul(u64::from(bits))
            .ok_or_else(|| decode_error("group table size overflows"))?;
        if needed > reader.bits_remaining() {
            return Err(decode_error("section 7 is shorter than a group table"));
        }
        let mut values = Vec::new();
        values
            .try_reserve_exact(count)
            .map_err(|_| refused("could not allocate a group table"))?;
        for _ in 0..count {
            values.push(reader.bits(bits)? as i32);
        }
        Ok(Table(values))
    }

    /// `getData2`. It reads group widths without adding `referenceGroupWidths`, and treats every
    /// zero-width group as missing when missing-value management is on. Both are followed
    /// because the reference for this tier is Vortex's array.
    fn complex(&self, c: &Complex) -> Result<Vec<f32>, VortexError> {
        let (dd, ee) = self.scales()?;
        let r = self.packing.reference;
        let mvm = c.missing_management;
        if mvm > 2 {
            return Err(refused(format!("missing value management {mvm}")));
        }
        if c.groups == 0 {
            return self.no_groups(r / dd);
        }
        let groups = self.group_count(c)?;
        let mut reader = BitReader::new(self.payload);
        let x1 = self.read_table(&mut reader, groups, self.packing.bits)?;
        if c.bits_widths != 0 {
            reader.align();
        }
        let widths = self.read_table(&mut reader, groups, c.bits_widths)?;
        reader.align();
        let scaled = self.read_table(&mut reader, groups, c.bits_lengths)?;
        let length_of = |i: usize| {
            if i == groups - 1 {
                c.length_last
            } else {
                c.reference_lengths
                    .wrapping_add(scaled.get(i).wrapping_mul(i32::from(c.length_increment)))
            }
        };
        reader.align();

        // Everything the groups declare, before the grid is allocated. getData2 fails past the
        // end of the grid, and leaves zeros when the groups fall short of it.
        let (points, bits) = totals(groups, 31, length_of, |i| widths.get(i))?;
        if points > self.total as u64 {
            return Err(decode_error("group lengths exceed the grid"));
        }
        // Under a bitmap the groups must supply every point it sets, or the grid is allocated for
        // values section 7 does not hold. getData2 leaves zeros: observed behaviour we did not
        // implement. Without a bitmap that is the behaviour under test, and it is kept.
        if self.bitmap.is_some() && points != self.data_points as u64 {
            return Err(decode_error(format!(
                "group lengths sum to {points}, section 5 declares {}",
                self.data_points
            )));
        }
        if bits > reader.bits_remaining() {
            return Err(decode_error("section 7 is shorter than the packed values"));
        }

        let mut data = allocate(self.total, self.limits)?;
        let mut count = 0usize;
        for i in 0..groups {
            let length = length_of(i) as usize;
            let width = widths.get(i) as u32;
            if width == 0 {
                let value = if mvm == 0 {
                    (r + x1.get(i) as f32 * ee) / dd
                } else {
                    f32::NAN
                };
                data[count..count + length].fill(value);
                count += length;
            } else {
                let missing = (1i64 << width) - 1;
                for _ in 0..length {
                    let x2 = reader.bits(width)?;
                    data[count] = if mvm != 0 && x2 == missing {
                        f32::NAN
                    } else {
                        (r + x1.get(i).wrapping_add(x2 as i32) as f32 * ee) / dd
                    };
                    count += 1;
                }
            }
        }
        self.expand_bitmap(data)
    }

    /// The tail of `getData2` and `getData3`: place packed values at the set bits.
    fn expand_bitmap(&self, data: Vec<f32>) -> Result<Vec<f32>, VortexError> {
        let Some(bitmap) = self.bitmap else {
            return Ok(data);
        };
        let mut expanded = allocate(self.total, self.limits)?;
        let mut next = 0;
        for (i, cell) in expanded.iter_mut().enumerate() {
            *cell = if bit_is_set(bitmap, i) {
                let value = *data
                    .get(next)
                    .ok_or_else(|| decode_error("bitmap has more set bits than the grid"))?;
                next += 1;
                value
            } else {
                f32::NAN
            };
        }
        Ok(expanded)
    }

    /// `getData3`.
    fn complex_differenced(&self, c: &Complex) -> Result<Vec<f32>, VortexError> {
        let (dd, ee) = self.scales()?;
        let r = self.packing.reference;
        let mvm = c.missing_management;
        if mvm > 2 {
            return Err(refused(format!("missing value management {mvm}")));
        }
        if c.groups == 0 {
            return self.no_groups(r / dd);
        }
        let groups = self.group_count(c)?;
        let order = c.order;
        let total = self.total;
        if order > 2 {
            return Err(refused(format!("spatial differencing order {order}")));
        }
        if usize::from(order) > total {
            return Err(decode_error("differencing order exceeds the grid"));
        }

        let mut reader = BitReader::new(self.payload);
        let descriptor_bits = u32::from(c.descriptor) * 8;
        if descriptor_bits == 0 {
            // getData3 returns an all-missing grid here.
            let mut data = allocate(total, self.limits)?;
            data.fill(f32::NAN);
            return Ok(data);
        }
        if descriptor_bits > 32 {
            return Err(refused(format!("{} descriptor octets", c.descriptor)));
        }
        let signed = |reader: &mut BitReader| -> Result<i32, VortexError> {
            let sign = reader.bits(1)?;
            let magnitude = reader.bits(descriptor_bits - 1)? as i32;
            Ok(if sign == 1 {
                magnitude.wrapping_neg()
            } else {
                magnitude
            })
        };
        let first = signed(&mut reader)?;
        let second = if order == 2 { signed(&mut reader)? } else { 0 };
        let minimum = signed(&mut reader)?;

        if self.packing.bits != 0 {
            reader.align();
        }
        let x1 = self.read_table(&mut reader, groups, self.packing.bits)?;
        if c.bits_widths != 0 {
            reader.align();
        }
        let widths = self.read_table(&mut reader, groups, c.bits_widths)?;
        let width_of = |i: usize| widths.get(i).wrapping_add(i32::from(c.reference_widths));
        if c.bits_lengths != 0 {
            reader.align();
        }
        let scaled = self.read_table(&mut reader, groups, c.bits_lengths)?;
        let length_of = |i: usize| {
            if i == groups - 1 {
                c.length_last
            } else {
                scaled
                    .get(i)
                    .wrapping_mul(i32::from(c.length_increment))
                    .wrapping_add(c.reference_lengths)
            }
        };
        reader.align();

        // getData3 returns an all-missing grid when the count disagrees, after a warning. A count
        // mismatch means the message cannot be decoded, so here it is an error: observed
        // behaviour we did not implement.
        let maximum_width = if mvm == 0 { 32 } else { 31 };
        let (points, bits) = totals(groups, maximum_width, length_of, width_of)?;
        let expected = if mvm != 0 { total } else { self.data_points };
        if points != expected as u64 {
            return Err(decode_error(format!(
                "group lengths sum to {points}, expected {expected}"
            )));
        }
        if bits > reader.bits_remaining() {
            return Err(decode_error("section 7 is shorter than the packed values"));
        }

        let mut data = allocate(total, self.limits)?;
        let mut count = 0usize;
        let mut present: Vec<bool> = Vec::new();
        let mut size = 0usize;
        if mvm == 0 {
            for i in 0..groups {
                let width = width_of(i) as u32;
                let length = length_of(i) as usize;
                if width != 0 {
                    for _ in 0..length {
                        data[count] = (reader.bits(width)? as i32).wrapping_add(x1.get(i)) as f32;
                        count += 1;
                    }
                } else {
                    data[count..count + length].fill(x1.get(i) as f32);
                    count += length;
                }
            }
        } else {
            present
                .try_reserve_exact(total)
                .map_err(|_| refused("could not allocate the missing-value map"))?;
            present.resize(total, false);
            for i in 0..groups {
                let width = width_of(i) as u32;
                let length = length_of(i) as usize;
                if width != 0 {
                    let missing1 = ((1i64 << width) - 1) as i32;
                    let missing2 = missing1.wrapping_sub(1);
                    for _ in 0..length {
                        // getData3 stores into float, then compares as float.
                        let value = reader.bits(width)? as i32 as f32;
                        data[count] = value;
                        if value == missing1 as f32 || (mvm == 2 && value == missing2 as f32) {
                            present[count] = false;
                        } else {
                            present[count] = true;
                            data[size] = value + x1.get(i) as f32;
                            size += 1;
                        }
                        count += 1;
                    }
                } else {
                    let reference_bits = u32::from(self.packing.bits);
                    if reference_bits > 30 {
                        return Err(refused(format!("{reference_bits} reference bits")));
                    }
                    let missing1 = (1i32 << reference_bits) - 1;
                    let missing2 = missing1 - 1;
                    if x1.get(i) == missing1 || (mvm == 2 && x1.get(i) == missing2) {
                        count += length;
                    } else {
                        for _ in 0..length {
                            present[count] = true;
                            data[size] = x1.get(i) as f32;
                            size += 1;
                            count += 1;
                        }
                    }
                }
            }
        }

        // Summed in f32, as getData3 does. This loses integer precision above 2^24, where ecCodes
        // keeps it; the reference here is Vortex's array.
        let limit = if mvm == 0 { total } else { size };
        if order == 1 {
            data[0] = first as f32;
            for i in 1..limit {
                data[i] += minimum as f32;
                data[i] += data[i - 1];
            }
        } else if order == 2 {
            data[0] = first as f32;
            data[1] = second as f32;
            for i in 2..limit {
                data[i] += minimum as f32;
                data[i] = data[i] + (2.0 * data[i - 1]) - data[i - 2];
            }
        }

        if mvm == 0 {
            for cell in data.iter_mut() {
                *cell = (r + *cell * ee) / dd;
            }
        } else {
            let mut unpacked = allocate(total, self.limits)?;
            let mut next = 0;
            for (i, cell) in unpacked.iter_mut().enumerate() {
                *cell = if present[i] {
                    let value = (r + data[next] * ee) / dd;
                    next += 1;
                    value
                } else {
                    f32::NAN
                };
            }
            data = unpacked;
        }
        self.expand_bitmap(data)
    }

    /// `getData40`, with `hayro-jpeg2000` in place of `Grib2JpegDecoder`.
    fn jpeg2000(&self) -> Result<Vec<f32>, VortexError> {
        let (dd, ee) = self.scales()?;
        let r = self.packing.reference;
        if self.packing.bits == 0 {
            let mut result = allocate(self.total, self.limits)?;
            let reference = r / dd;
            result[..self.data_points].fill(reference);
            return Ok(result);
        }
        let values = decode_jpeg2000(
            self.payload,
            u32::from(self.packing.bits),
            self.data_points,
            self.limits,
        )?;
        let mut result = allocate(self.total, self.limits)?;
        match self.bitmap {
            None => {
                if values.len() != self.data_points {
                    return Err(decode_error(format!(
                        "JPEG 2000 image has {} samples, section 5 declares {}",
                        values.len(),
                        self.data_points
                    )));
                }
                for (cell, &x) in result.iter_mut().zip(&values) {
                    *cell = (r + x as f32 * ee) / dd;
                }
            }
            Some(bitmap) => {
                let mut next = 0;
                for (i, cell) in result.iter_mut().enumerate() {
                    if bit_is_set(bitmap, i) {
                        // getData40 stops here and leaves the rest zero; a short image cannot
                        // be decoded, so here it is an error.
                        let x = *values.get(next).ok_or_else(|| {
                            decode_error("JPEG 2000 image is short of the bitmap")
                        })?;
                        *cell = (r + x as f32 * ee) / dd;
                        next += 1;
                    } else {
                        *cell = f32::NAN;
                    }
                }
            }
        }
        Ok(result)
    }

    /// `getData41` and `decodePng`, with the `png` crate in place of `ImageIO`.
    fn png(&self) -> Result<Vec<f32>, VortexError> {
        let (dd, ee) = self.scales()?;
        let r = self.packing.reference;
        if self.packing.bits == 0 {
            // getData41 fills with R here, not R / DD.
            let mut data = allocate(self.total, self.limits)?;
            data.fill(r);
            return Ok(data);
        }
        let image = decode_png(self.payload, self.limits)?;
        let mut data = allocate(self.total, self.limits)?;
        match self.bitmap {
            None => {
                if image.count < self.data_points {
                    return Err(decode_error("PNG image is short of section 5's count"));
                }
                for (i, cell) in data.iter_mut().take(self.data_points).enumerate() {
                    *cell = (r + image.value(i) as f32 * ee) / dd;
                }
            }
            Some(bitmap) => {
                let mut next = 0;
                for (i, cell) in data.iter_mut().enumerate() {
                    if bit_is_set(bitmap, i) {
                        if next >= image.count {
                            return Err(decode_error("PNG image is short of the bitmap"));
                        }
                        *cell = (r + image.value(next) as f32 * ee) / dd;
                        next += 1;
                    } else {
                        *cell = f32::NAN;
                    }
                }
            }
        }
        Ok(data)
    }
}

/// A group table. Empty when its field width is zero bits: every entry is then 0.
struct Table(Vec<i32>);

impl Table {
    fn get(&self, i: usize) -> i32 {
        self.0.get(i).copied().unwrap_or(0)
    }
}

/// Total points and packed bits the groups declare, in checked arithmetic. Each length is
/// non-negative and each width within `maximum_width` when this returns.
fn totals(
    groups: usize,
    maximum_width: u32,
    length_of: impl Fn(usize) -> i32,
    width_of: impl Fn(usize) -> i32,
) -> Result<(u64, u64), VortexError> {
    let (mut points, mut bits) = (0u64, 0u64);
    for i in 0..groups {
        let length = u64::try_from(length_of(i))
            .map_err(|_| decode_error(format!("group length {}", length_of(i))))?;
        let width = u32::try_from(width_of(i))
            .ok()
            .filter(|&w| w <= maximum_width)
            .ok_or_else(|| refused(format!("group width {}", width_of(i))))?;
        points = points
            .checked_add(length)
            .ok_or_else(|| decode_error("group lengths overflow"))?;
        bits = length
            .checked_mul(u64::from(width))
            .and_then(|b| bits.checked_add(b))
            .ok_or_else(|| decode_error("packed size overflows"))?;
    }
    Ok((points, bits))
}

/// A third-party decoder panicking on hostile bytes must still be a typed error.
fn guarded<T>(
    format: &'static str,
    decode: impl FnOnce() -> Result<T, VortexError>,
) -> Result<T, VortexError> {
    catch_unwind(AssertUnwindSafe(decode)).unwrap_or_else(|_| {
        Err(VortexError::Decode {
            format,
            message: "the decoder panicked on this input".to_string(),
        })
    })
}

fn decode_jpeg2000(
    payload: &[u8],
    bits: u32,
    data_points: usize,
    limits: &Limits,
) -> Result<Vec<i32>, VortexError> {
    use hayro_jpeg2000::{DecodeSettings, DecoderContext, Image};
    // Before the decoder is entered: its allocations are not fallible, so failing one aborts.
    let stream = jpeg2000_budget::admit(payload, limits)?;
    guarded("JPEG 2000", || {
        let error = |e: String| VortexError::Decode {
            format: "JPEG 2000",
            message: e,
        };
        let image =
            Image::new(payload, &DecodeSettings::default()).map_err(|e| error(e.to_string()))?;
        if (image.width(), image.height(), image.original_bit_depth())
            != (stream.width, stream.height, stream.depth)
        {
            return Err(refused("JPEG 2000 header read differently by the decoder"));
        }
        let pixels = u64::from(image.width()) * u64::from(image.height());
        if pixels > limits.max_cells as u64 {
            return Err(refused(format!("JPEG 2000 image of {pixels} pixels")));
        }
        if pixels != data_points as u64 {
            return Err(decode_error(format!(
                "JPEG 2000 image has {pixels} pixels, section 5 declares {data_points}"
            )));
        }
        // Grib2JpegDecoder passes `-rate nbits` to JJ2000, which stops reading the code stream
        // after nbits per pixel. A stream longer than that decodes to different values there, and
        // to the packed values here and in ecCodes. Observed on Vortex 0.13.3 and 1.1.0 with a
        // one-row image under a bitmap and with noise fields at 8, 12 and 16 bits; a smooth field
        // under the limit is exact. Observed behaviour we did not implement, so it is refused
        // rather than returned with values that differ from Vortex's.
        if payload.len() as u64 > u64::from(bits) * pixels / 8 {
            return Err(refused(format!(
                "JPEG 2000 stream of {} bytes is longer than {bits} bits per pixel for {pixels} \
                 pixels; Vortex's decoder truncates such a stream",
                payload.len()
            )));
        }
        // f32 samples lose integers above 2^24.
        if image.original_bit_depth() > 24 {
            return Err(refused(format!(
                "JPEG 2000 depth {}",
                image.original_bit_depth()
            )));
        }
        let mut context = DecoderContext::default();
        let decoded = image
            .decode(&mut context)
            .map_err(|e| error(e.to_string()))?;
        let [component] = decoded.components() else {
            return Err(refused("JPEG 2000 with more than one component"));
        };
        let samples = component.samples();
        let mut values = Vec::new();
        values
            .try_reserve_exact(samples.len())
            .map_err(|_| refused("could not allocate the JPEG 2000 samples"))?;
        for &s in samples {
            // An irreversible code stream gives fractions, which Grib2JpegDecoder would round
            // in its own way. No sampled product has one, so there is no reference for it.
            if s.fract() != 0.0 || !s.is_finite() {
                return Err(refused("JPEG 2000 sample is not an integer"));
            }
            values.push(s as i32);
        }
        Ok(values)
    })
}

struct PngImage {
    bytes: Vec<u8>,
    bands: usize,
    wide: bool,
    count: usize,
}

impl PngImage {
    /// `decodePng`. `ImageIO` hands back RGB as BGR and RGBA as ABGR, and `decodePng`'s names
    /// are for that order. Written in file order here, so the 4-band case is `int4(R, G, B, A)`.
    fn value(&self, i: usize) -> i32 {
        let at = i * self.bands;
        match (self.bands, self.wide) {
            (1, false) => i32::from(self.bytes[at]),
            (1, true) => i32::from(u16::from_be_bytes([
                self.bytes[at * 2],
                self.bytes[at * 2 + 1],
            ])),
            (3, _) => {
                i32::from(self.bytes[at]) << 16
                    | i32::from(self.bytes[at + 1]) << 8
                    | i32::from(self.bytes[at + 2])
            }
            _ => int4([
                self.bytes[at],
                self.bytes[at + 1],
                self.bytes[at + 2],
                self.bytes[at + 3],
            ]),
        }
    }
}

fn decode_png(payload: &[u8], limits: &Limits) -> Result<PngImage, VortexError> {
    use png::{BitDepth, ColorType};
    guarded("PNG", || {
        let error = |e: String| VortexError::Decode {
            format: "PNG",
            message: e,
        };
        let bound = limits.max_cells.saturating_mul(8);
        let decoder = png::Decoder::new_with_limits(
            std::io::Cursor::new(payload),
            png::Limits { bytes: bound },
        );
        let mut reader = decoder.read_info().map_err(|e| error(e.to_string()))?;
        let info = reader.info();
        let (width, height) = (info.width as usize, info.height as usize);
        let (bands, wide) = match (info.color_type, info.bit_depth) {
            (ColorType::Grayscale, BitDepth::Eight) => (1, false),
            (ColorType::Grayscale, BitDepth::Sixteen) => (1, true),
            (ColorType::Rgb, BitDepth::Eight) => (3, false),
            (ColorType::Rgba, BitDepth::Eight) => (4, false),
            (color, depth) => {
                // ImageIO packs depths below 8 and 16-bit colour differently, and no sampled
                // product has them, so there is no reference.
                return Err(refused(format!("PNG {color:?} at {depth:?}")));
            }
        };
        let count = width
            .checked_mul(height)
            .filter(|&n| n <= limits.max_cells)
            .ok_or_else(|| refused(format!("PNG image of {width} x {height}")))?;
        let size = reader
            .output_buffer_size()
            .ok_or_else(|| error("image is too large".to_string()))?;
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(size)
            .map_err(|_| refused("could not allocate the PNG image"))?;
        bytes.resize(size, 0u8);
        reader
            .next_frame(&mut bytes)
            .map_err(|e| error(e.to_string()))?;
        Ok(PngImage {
            bytes,
            bands,
            wide,
            count,
        })
    })
}
