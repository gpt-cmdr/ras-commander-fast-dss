//! DSS7 time-series block expansion: repeat-bit decompression and disaggregation of the
//! values1 stream into values, quality, integer notes and character notes
//! (`docs/format/dss7.md` §7.5).
//!
//! Ports of `Internal/ztsDisaggregate.c:ztsDisaggregate()` (files from 7-HA on) and
//! `ztsDisaggregateDep()` (`fver < 70801`, chosen exactly as `ztsDisaggregate()` chooses it)
//! and `Internal/ztsRegRepeatUncompress.c:ztsRegRepeatUncompress()`. All arrays are int32
//! words, as in HEC; a double occupies two words, low word first (little-endian files).
//!
//! The port keeps HEC's position arithmetic, including its corner cases, so a block reads
//! the same values as `hecdss`. Every index into the stored stream is bounds-checked: an
//! out-of-range read or write, which HEC would perform on a damaged record, returns
//! [`Corrupt`] instead.

/// A stored array is shorter than its header says, or a write would leave the output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Corrupt(pub &'static str);

/// Inputs for one block, mirroring the `ztsDisaggregate` argument list.
pub(crate) struct Disaggregate<'a> {
    /// values1 as int32 words.
    pub data_in: &'a [i32],
    /// Words of values1 before the values (the irregular times array; 0 for regular).
    pub data_in_position: usize,
    pub header2: &'a [i32],
    /// TS internal header (at least 17 words; missing words read as 0).
    pub ih: &'a [i32],
    pub number_to_read: i64,
    /// `kinfoNumberData`.
    pub number_stored: i64,
    pub position_relative_first_valid: i64,
    pub position_relative_last_valid: i64,
    /// Use `ztsDisaggregateDep` (file version before 7-HA).
    pub deprecated: bool,
    /// Requested words per value element (1 float, 2 double); 0 skips values.
    pub values_requested: usize,
    /// Requested quality words per value; 0 skips quality.
    pub quality_requested: usize,
    /// Requested integer-note words per value; 0 skips notes.
    pub inotes_requested: usize,
    /// Decode character notes.
    pub want_cnotes: bool,
}

/// Expanded block.
#[derive(Debug, Default)]
pub(crate) struct Expanded {
    pub values: Vec<i32>,
    pub quality: Vec<i32>,
    pub inotes: Vec<i32>,
    /// NUL-terminated character notes, one per value read.
    pub cnotes: Vec<u8>,
    pub number_expanded: i64,
}

// TS internal header indices (`headers/zStructAllocation.h`).
pub(crate) const IH_PROFILE_DEPTHS: usize = 3;
pub(crate) const IH_VALUES_NUMBER: usize = 6;
pub(crate) const IH_VALUE_SIZE: usize = 7;
pub(crate) const IH_VALUE_ELEMENT_SIZE: usize = 8;
pub(crate) const IH_VALUES_COMPRESSION: usize = 9;
pub(crate) const IH_QUALITY_NUMBER: usize = 10;
pub(crate) const IH_QUALITY_ELEMENT_SIZE: usize = 11;
pub(crate) const IH_QUALITY_COMPRESSION: usize = 12;
pub(crate) const IH_INOTES_NUMBER: usize = 13;
pub(crate) const IH_INOTES_ELEMENT_SIZE: usize = 14;
pub(crate) const IH_INOTES_COMPRESSION: usize = 15;
pub(crate) const IH_CNOTES_LENGTH: usize = 16;

/// `UNDEFINED_FLOAT` = −FLT_MAX (`headers/heclib.h`), as an int32 word.
pub(crate) const MISSING_F32_BITS: u32 = 0xFF7F_FFFF;
/// `UNDEFINED_DOUBLE` = −(double)FLT_MAX.
pub(crate) const MISSING_F64_BITS: u64 = 0xC7EF_FFFF_E000_0000;

/// Largest output (in int32 words) one block may expand to. A regular block has at most
/// 86,400 values (1Second daily block); irregular blocks are bounded by the stored count.
pub(crate) const MAX_OUT_WORDS: i64 = 1 << 28;

struct Out {
    buf: Vec<i32>,
}

impl Out {
    fn new(words: i64) -> Result<Self, Corrupt> {
        if !(0..=MAX_OUT_WORDS).contains(&words) {
            return Err(Corrupt("expanded block too large"));
        }
        Ok(Out {
            buf: vec![0; words as usize],
        })
    }
    fn set(&mut self, i: i64, v: i32) -> Result<(), Corrupt> {
        let i = usize::try_from(i).map_err(|_| Corrupt("negative output position"))?;
        match self.buf.get_mut(i) {
            Some(s) => {
                *s = v;
                Ok(())
            }
            None => Err(Corrupt("expanded data overruns the block")),
        }
    }
    fn get(&self, i: i64) -> Result<i32, Corrupt> {
        usize::try_from(i)
            .ok()
            .and_then(|i| self.buf.get(i).copied())
            .ok_or(Corrupt("expanded data overruns the block"))
    }
}

fn get(a: &[i32], i: i64, what: &'static str) -> Result<i32, Corrupt> {
    usize::try_from(i)
        .ok()
        .and_then(|i| a.get(i).copied())
        .ok_or(Corrupt(what))
}

/// `zsetMissing(&values[pos], size)`: −FLT_MAX in the requested width.
fn set_missing(out: &mut Out, pos: i64, size: usize) -> Result<(), Corrupt> {
    if size == 1 {
        out.set(pos, MISSING_F32_BITS as i32)
    } else {
        out.set(pos, MISSING_F64_BITS as u32 as i32)?;
        out.set(pos + 1, (MISSING_F64_BITS >> 32) as u32 as i32)
    }
}

/// `fillArray(&missing, 1, ...)` for profiles: the float missing flag converted
/// (`convertDataType`) into every element of the row.
fn fill_missing_profile(out: &mut Out, pos: i64, size: usize, depths: i64) -> Result<(), Corrupt> {
    for k in 0..depths {
        set_missing(out, pos + k * size as i64, size)?;
    }
    Ok(())
}

/// `convertDataType()`: copy, widen a float to a double, or narrow a double to a float.
fn convert_type(
    src: &[i32],
    at: i64,
    in_len: i64,
    out: &mut Out,
    pos: i64,
    out_len: usize,
) -> Result<(), Corrupt> {
    let w = |k: i64| get(src, at + k, "values array shorter than its header");
    if in_len == out_len as i64 {
        for k in 0..in_len {
            out.set(pos + k, w(k)?)?;
        }
    } else if in_len == 1 && out_len == 2 {
        let d = f64::from(f32::from_bits(w(0)? as u32)).to_bits();
        out.set(pos, d as u32 as i32)?;
        out.set(pos + 1, (d >> 32) as u32 as i32)?;
    } else if in_len == 2 && out_len == 1 {
        let d = f64::from_bits((w(0)? as u32 as u64) | ((w(1)? as u32 as u64) << 32));
        out.set(pos, (d as f32).to_bits() as i32)?;
    }
    Ok(())
}

/// `convertDataLength()`: copy `min(in, out)` words, zero-fill the rest.
fn convert_length(
    src: &[i32],
    at: i64,
    in_len: i64,
    out: &mut Out,
    pos: i64,
    out_len: usize,
) -> Result<(), Corrupt> {
    let out_len = out_len as i64;
    for k in 0..out_len {
        let v = if k < in_len {
            get(
                src,
                at + k,
                "quality or notes array shorter than its header",
            )?
        } else {
            0
        };
        out.set(pos + k, v)?;
    }
    Ok(())
}

/// `Internal/ztsRegRepeatUncompress.c:ztsRegRepeatUncompress()`. Bit `i` (word `i/32`,
/// bit `i%32`) of `header2` set means value `i` repeats value `i-1`; otherwise the next
/// stored value is used. Writes values `start..end` at `out[pos..]`.
#[allow(clippy::too_many_arguments)]
fn repeat_uncompress(
    start: i64,
    end: i64,
    element: i64,
    requested: usize,
    is_value: bool,
    header2: &[i32],
    data: &[i32],
    data_at: i64,
    out: &mut Out,
    pos: i64,
) -> Result<(), Corrupt> {
    let mut icount: i64 = 0;
    let conv = |out: &mut Out, icount: i64, opos: i64| -> Result<(), Corrupt> {
        if is_value {
            convert_type(data, data_at + icount, element, out, opos, requested)
        } else {
            convert_length(data, data_at + icount, element, out, opos, requested)
        }
    };
    let bit = |i: i64| -> Result<bool, Corrupt> {
        let w = get(header2, i / 32, "compression header shorter than the block")?;
        Ok(w & (1i32.wrapping_shl((i % 32) as u32)) != 0)
    };
    if start == 0 {
        conv(out, icount, pos)?;
        icount += element;
    } else {
        // Uncompress everything before `start` to find the first value.
        for i in 0..=start {
            if !bit(i)? {
                conv(out, icount, pos)?;
                icount += element;
            }
        }
    }
    if start == end {
        return Ok(());
    }
    let req = requested as i64;
    let mut ipos = pos + req;
    for i in (start + 1)..end {
        if bit(i)? {
            for _ in 0..req {
                let v = out.get(ipos - req)?;
                out.set(ipos, v)?;
                ipos += 1;
            }
        } else {
            conv(out, icount, ipos)?;
            icount += element;
            ipos += req;
        }
    }
    Ok(())
}

/// Copy `count` NUL-terminated lines; returns bytes consumed (`copyLines()` counting).
fn count_lines(src: &[u8], lines: i64) -> usize {
    let mut ipos = 0usize;
    for _ in 0..lines.max(0) {
        if ipos >= src.len() {
            break;
        }
        let len = src[ipos..]
            .iter()
            .position(|&b| b == 0)
            .unwrap_or(src.len() - ipos);
        ipos += len + 1;
        if ipos >= src.len() {
            break;
        }
    }
    ipos.min(src.len())
}

impl Disaggregate<'_> {
    fn ih(&self, i: usize) -> i64 {
        i64::from(self.ih.get(i).copied().unwrap_or(0))
    }

    /// Run the port. Output arrays are sized for `number_to_read` values and start zeroed
    /// (HEC `calloc`s them in `ztsRetrieveReg7` / `ztsRetrieveIrreg7`).
    pub(crate) fn run(&self) -> Result<Expanded, Corrupt> {
        let n = self.number_to_read.max(0);
        let values_length = self.ih(IH_VALUES_NUMBER);
        let values_size_read = self.ih(IH_VALUE_SIZE);
        let values_element = self.ih(IH_VALUE_ELEMENT_SIZE);
        let depths = self.ih(IH_PROFILE_DEPTHS);
        let values_flag = self.ih(IH_VALUES_COMPRESSION);
        let quality_number = self.ih(IH_QUALITY_NUMBER);
        let quality_element = self.ih(IH_QUALITY_ELEMENT_SIZE);
        let quality_flag = self.ih(IH_QUALITY_COMPRESSION);
        let inotes_number = self.ih(IH_INOTES_NUMBER);
        let inotes_element = self.ih(IH_INOTES_ELEMENT_SIZE);
        let inotes_flag = self.ih(IH_INOTES_COMPRESSION);
        let cnotes_length = self.ih(IH_CNOTES_LENGTH);
        for v in [
            values_length,
            values_size_read,
            values_element,
            depths,
            quality_number,
            quality_element,
            inotes_number,
            inotes_element,
            cnotes_length,
        ] {
            if !(0..=MAX_OUT_WORDS).contains(&v) {
                return Err(Corrupt("internal header size out of range"));
            }
        }
        let number_stored = self.number_stored;
        if !(0..=MAX_OUT_WORDS).contains(&number_stored) {
            return Err(Corrupt("stored value count out of range"));
        }
        let prfv = self.position_relative_first_valid;
        let prlv = self.position_relative_last_valid;
        let dmul = depths.max(1);

        let mut vreq = self.values_requested;
        let mut qreq = self.quality_requested;
        let mut nreq = self.inotes_requested;
        let mut values = Out::new(n * vreq as i64 * dmul)?;
        let mut quality = Out::new(n * qreq as i64)?;
        let mut inotes = Out::new(n * nreq as i64)?;
        let mut cnotes: Vec<u8> = Vec::new();
        let mut want_c = self.want_cnotes;

        let (mut vpos, mut qpos, mut npos) = (0i64, 0i64, 0i64);
        let data = self.data_in;
        let base = if self.deprecated {
            self.data_in_position as i64
        } else {
            0
        };
        let mut dpos = if self.deprecated {
            0
        } else {
            self.data_in_position as i64
        };

        // Requested but not stored: fill and skip.
        if vreq > 0 && values_length == 0 {
            for i in 0..n {
                set_missing(&mut values, i * vreq as i64, vreq)?;
            }
            vreq = 0;
        }
        if qreq > 0 && quality_number == 0 {
            qreq = 0;
        }
        if nreq > 0 && inotes_number == 0 {
            nreq = 0;
        }
        if want_c && cnotes_length == 0 {
            cnotes.resize(n as usize, 0);
            want_c = false;
        }

        let missing_value = |values: &mut Out, pos: i64, vreq: usize| -> Result<(), Corrupt> {
            if depths == 0 {
                set_missing(values, pos, vreq)
            } else {
                fill_missing_profile(values, pos, vreq, depths)
            }
        };
        let vstep = vreq as i64 * dmul;

        // Missing values before the stored data.
        if prfv < 0 {
            let before = (-prfv).min(n);
            for _ in 0..before {
                if vreq > 0 {
                    missing_value(&mut values, vpos, vreq)?;
                    vpos += vstep;
                }
                qpos += qreq as i64;
                npos += nreq as i64;
                if want_c {
                    cnotes.push(0);
                }
            }
            if before >= n {
                return Ok(Expanded {
                    values: values.buf,
                    quality: quality.buf,
                    inotes: inotes.buf,
                    cnotes,
                    number_expanded: n,
                });
            }
        }

        let stored_to_read = number_stored - prfv;
        let (start_array, mut end_array) = if prfv > 0 {
            (prfv, prfv + stored_to_read)
        } else {
            (0, number_stored)
        };
        if prlv < 0 {
            end_array += prlv;
        }
        if end_array - start_array > n {
            end_array = start_array + n;
        }
        let expanded = end_array - start_array;

        if expanded > 0 && stored_to_read > 0 {
            // Values.
            if vreq > 0 {
                if values_size_read > 0 {
                    let at = base + dpos;
                    match values_flag {
                        0 => {
                            if vreq as i64 == values_size_read {
                                for i in
                                    start_array * values_size_read..end_array * values_size_read
                                {
                                    values.set(
                                        vpos,
                                        get(data, at + i, "values array shorter than its header")?,
                                    )?;
                                    vpos += 1;
                                }
                            } else if depths == 0 {
                                for i in start_array..end_array {
                                    convert_type(
                                        data,
                                        at + i * values_size_read,
                                        values_element,
                                        &mut values,
                                        vpos,
                                        vreq,
                                    )?;
                                    vpos += vreq as i64;
                                }
                            } else {
                                // convertDataArray(): per-element conversion of a profile row.
                                for i in start_array..end_array {
                                    for k in 0..depths {
                                        convert_type(
                                            data,
                                            at + i * values_size_read + k * values_element,
                                            values_element,
                                            &mut values,
                                            vpos + k * vreq as i64,
                                            vreq,
                                        )?;
                                    }
                                    vpos += vstep;
                                }
                            }
                        }
                        1 => {
                            repeat_uncompress(
                                start_array,
                                end_array,
                                values_size_read,
                                vreq,
                                true,
                                self.header2,
                                data,
                                at,
                                &mut values,
                                vpos,
                            )?;
                        }
                        2 => {
                            for _ in start_array..end_array {
                                if vreq as i64 == values_size_read {
                                    for j in 0..values_size_read {
                                        values.set(
                                            vpos,
                                            get(
                                                data,
                                                at + j,
                                                "values array shorter than its header",
                                            )?,
                                        )?;
                                        vpos += 1;
                                    }
                                } else {
                                    convert_type(
                                        data,
                                        at,
                                        values_size_read,
                                        &mut values,
                                        vpos,
                                        vreq,
                                    )?;
                                    vpos += vreq as i64;
                                }
                            }
                        }
                        _ => return Err(Corrupt("unknown values compression flag")),
                    }
                } else {
                    for _ in start_array..end_array {
                        missing_value(&mut values, vpos, vreq)?;
                        vpos += vstep;
                    }
                }
            }

            // Quality. ztsDisaggregate pads the values area to an even word count;
            // ztsDisaggregateDep starts quality right after the values.
            if !self.deprecated {
                dpos += values_length;
                if dpos % 2 != 0 {
                    dpos += 1;
                }
            }
            let qat = if self.deprecated {
                base + values_length
            } else {
                dpos
            };
            if qreq > 0 && quality_element > 0 {
                match quality_flag {
                    0 => {
                        if qreq as i64 == quality_element {
                            for i in start_array * quality_element..end_array * quality_element {
                                quality.set(
                                    qpos,
                                    get(data, qat + i, "quality array shorter than its header")?,
                                )?;
                                qpos += 1;
                            }
                        } else {
                            for i in start_array..end_array {
                                convert_length(
                                    data,
                                    qat + i * quality_element,
                                    quality_element,
                                    &mut quality,
                                    qpos,
                                    qreq,
                                )?;
                                qpos += qreq as i64;
                            }
                        }
                    }
                    1 => {
                        let h2 = if values_flag == 1 {
                            (number_stored - 1) / 32 + 1
                        } else {
                            0
                        };
                        let h2 = usize::try_from(h2)
                            .map_err(|_| Corrupt("bad compression header position"))?;
                        let header2 = self.header2.get(h2..).unwrap_or(&[]);
                        repeat_uncompress(
                            start_array,
                            end_array,
                            quality_element,
                            qreq,
                            false,
                            header2,
                            data,
                            qat,
                            &mut quality,
                            qpos,
                        )?;
                    }
                    2 => {
                        for _ in start_array..end_array {
                            if qreq as i64 == quality_element {
                                for j in 0..quality_element {
                                    quality.set(
                                        qpos,
                                        get(
                                            data,
                                            qat + j,
                                            "quality array shorter than its header",
                                        )?,
                                    )?;
                                    qpos += 1;
                                }
                            } else {
                                convert_length(
                                    data,
                                    qat,
                                    quality_element,
                                    &mut quality,
                                    qpos,
                                    qreq,
                                )?;
                                qpos += qreq as i64;
                            }
                        }
                    }
                    _ => return Err(Corrupt("unknown quality compression flag")),
                }
                // No stored quality: the zero-filled output already holds zsetUndefined().
            }

            // Integer notes.
            if !self.deprecated {
                dpos += quality_number;
                if dpos % 2 != 0 {
                    dpos += 1;
                }
            }
            let nat = if self.deprecated {
                base + values_length + quality_number
            } else {
                dpos
            };
            if nreq > 0 && inotes_element > 0 {
                match inotes_flag {
                    0 => {
                        if nreq as i64 == inotes_element {
                            for i in start_array * inotes_element..end_array * inotes_element {
                                inotes.set(
                                    npos,
                                    get(data, nat + i, "notes array shorter than its header")?,
                                )?;
                                npos += 1;
                            }
                        } else {
                            for i in start_array..end_array {
                                convert_length(
                                    data,
                                    nat + i * inotes_element,
                                    inotes_element,
                                    &mut inotes,
                                    npos,
                                    nreq,
                                )?;
                                npos += nreq as i64;
                            }
                        }
                    }
                    1 => {
                        let mut h2 = 0;
                        if values_flag == 1 {
                            h2 += (number_stored - 1) / 32 + 1;
                        }
                        if quality_flag == 1 {
                            h2 += (number_stored - 1) / 32 + 1;
                        }
                        let h2 = usize::try_from(h2)
                            .map_err(|_| Corrupt("bad compression header position"))?;
                        let header2 = self.header2.get(h2..).unwrap_or(&[]);
                        repeat_uncompress(
                            start_array,
                            end_array,
                            inotes_element,
                            nreq,
                            false,
                            header2,
                            data,
                            nat,
                            &mut inotes,
                            npos,
                        )?;
                    }
                    2 => {
                        for _ in start_array..end_array {
                            if nreq as i64 == inotes_element {
                                for j in 0..inotes_element {
                                    inotes.set(
                                        npos,
                                        get(data, nat + j, "notes array shorter than its header")?,
                                    )?;
                                    npos += 1;
                                }
                            } else {
                                convert_length(data, nat, inotes_element, &mut inotes, npos, nreq)?;
                                npos += nreq as i64;
                            }
                        }
                    }
                    _ => return Err(Corrupt("unknown notes compression flag")),
                }
            }

            // Character notes: never compressed; one NUL-terminated string per value.
            if cnotes_length > 0 && want_c {
                let start = usize::try_from(nat).map_err(|_| Corrupt("bad notes position"))?;
                let words = (cnotes_length as usize).div_ceil(4);
                let src_words = data
                    .get(start..start + words)
                    .ok_or(Corrupt("character notes shorter than their header"))?;
                let mut bytes: Vec<u8> = src_words.iter().flat_map(|w| w.to_le_bytes()).collect();
                bytes.truncate(cnotes_length as usize);
                let skip = count_lines(&bytes, start_array);
                let rest = &bytes[skip..];
                let take = count_lines(rest, end_array - start_array);
                cnotes.extend_from_slice(&rest[..take]);
            }
        }

        // Missing values after the stored data.
        if prlv > 0 {
            let mut position = number_stored - prfv;
            let mut count = prlv;
            if position < 0 {
                count += position;
                position = 0;
            }
            if vreq > 0 {
                let mut p = position * vstep;
                for _ in 0..count {
                    missing_value(&mut values, p, vreq)?;
                    p += vstep;
                }
            }
            if qreq > 0 || nreq > 0 || want_c {
                let mut qp = position * qreq as i64;
                let mut np = position * nreq as i64;
                for _ in 0..count {
                    for k in 0..qreq as i64 {
                        quality.set(qp + k, 0)?;
                    }
                    qp += qreq as i64;
                    for k in 0..nreq as i64 {
                        inotes.set(np + k, 0)?;
                    }
                    np += nreq as i64;
                    if want_c {
                        cnotes.push(0);
                    }
                }
            }
        }
        Ok(Expanded {
            values: values.buf,
            quality: quality.buf,
            inotes: inotes.buf,
            cnotes,
            number_expanded: expanded.max(0),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ih(n: i32, size: i32, flag: i32) -> Vec<i32> {
        let mut h = vec![0; 18];
        h[IH_VALUES_NUMBER] = n;
        h[IH_VALUE_SIZE] = size;
        h[IH_VALUE_ELEMENT_SIZE] = size;
        h[IH_VALUES_COMPRESSION] = flag;
        h
    }

    fn run(d: &Disaggregate) -> Vec<f32> {
        d.run()
            .unwrap()
            .values
            .iter()
            .map(|&w| f32::from_bits(w as u32))
            .collect()
    }

    fn base<'a>(
        data: &'a [i32],
        h2: &'a [i32],
        ih: &'a [i32],
        n: i64,
        stored: i64,
        prfv: i64,
        prlv: i64,
    ) -> Disaggregate<'a> {
        Disaggregate {
            data_in: data,
            data_in_position: 0,
            header2: h2,
            ih,
            number_to_read: n,
            number_stored: stored,
            position_relative_first_valid: prfv,
            position_relative_last_valid: prlv,
            deprecated: false,
            values_requested: 1,
            quality_requested: 0,
            inotes_requested: 0,
            want_cnotes: false,
        }
    }

    #[test]
    fn repeat_bits_expand() {
        // 1, 1, 2, 2, 2, 3 stored as [1, 2, 3] with repeat bits on positions 1, 3, 4.
        let data: Vec<i32> = [1.0f32, 2.0, 3.0]
            .iter()
            .map(|v| v.to_bits() as i32)
            .collect();
        let h2 = [0b11010];
        let h = ih(3, 1, 1);
        let d = base(&data, &h2, &h, 6, 6, 0, 0);
        assert_eq!(run(&d), vec![1.0, 1.0, 2.0, 2.0, 2.0, 3.0]);
        // Start mid-block.
        let d = base(&data, &h2, &h, 3, 6, 2, -1);
        assert_eq!(run(&d), vec![2.0, 2.0, 2.0]);
    }

    #[test]
    fn all_repeat_and_padding() {
        let data = [7.0f32.to_bits() as i32, 0];
        let h = ih(1, 1, 2);
        // Block positions 2..4 stored; read the whole 6-value block.
        let d = base(&data, &[], &h, 6, 3, -2, 1);
        let m = f32::from_bits(MISSING_F32_BITS);
        assert_eq!(run(&d), vec![m, m, 7.0, 7.0, 7.0, m]);
    }

    #[test]
    fn hostile_sizes_are_errors() {
        let data = [0i32; 2];
        let h = ih(1_000_000, 1, 0);
        let d = base(&data, &[], &h, 10, 10, 0, 0);
        assert!(d.run().is_err());
        let h = ih(2, 1, 1);
        let d = base(&data, &[], &h, 10, 10, 0, 0);
        assert!(d.run().is_err());
    }
}
