//! Finds GRIB1 messages and their sections.
//!
//! Sources, NetCDF-Java 5.5.3: `Grib1RecordScanner.hasNext` and `next` (bytes between messages
//! are skipped; a message is `GRIB`, a 3-octet total length and edition 1), `Grib1SectionIndicator`,
//! `Grib1SectionProductDefinition` (octet 8 flags: bit 1 a GDS follows, bit 2 a BMS follows),
//! `Grib1SectionGridDefinition`, `Grib1SectionBitMap` and `Grib1SectionBinaryData` (each a 3-octet
//! length first). WMO FM 92 GRIB edition 1, section 0 to 5.
//!
//! Every length comes from the file, so every range is checked here. `Grib1RecordScanner`
//! tolerates a bad section 4 length for ECMWF (`allowBadDsLength`) and reads ECMWF's large-message
//! length convention; neither is in Stage IV, and both are refused here rather than guessed.

use crate::error::VortexError;
use std::ops::Range;

/// One GRIB1 message: its sections, as byte ranges into the file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    /// Byte offset of the `GRIB` that starts the message.
    pub start: usize,
    /// One past the message's `7777`.
    pub end: usize,
    pub product: Range<usize>,
    /// `None` when section 1 says no GDS follows (a predefined grid).
    pub grid: Option<Range<usize>>,
    /// `None` when section 1 says no BMS follows.
    pub bitmap: Option<Range<usize>>,
    pub data: Range<usize>,
}

/// What a scan found.
#[derive(Debug, Default)]
pub struct ScanResult {
    pub messages: Vec<Message>,
    /// Messages of another edition (GRIB2). A file mixing editions is refused above this layer.
    pub other_editions: usize,
}

pub(super) fn decode_error(message: String) -> VortexError {
    VortexError::Decode {
        format: "GRIB1",
        message,
    }
}

fn find_grib(data: &[u8], from: usize) -> Option<usize> {
    data.get(from..)?
        .windows(4)
        .position(|w| w == b"GRIB")
        .map(|p| p + from)
}

pub(super) fn u24(data: &[u8], at: usize) -> Option<usize> {
    let b = data.get(at..at.checked_add(3)?)?;
    Some((usize::from(b[0]) << 16) | (usize::from(b[1]) << 8) | usize::from(b[2]))
}

/// The edition octet of the first message in `head`, if `head` holds one. Used by the importer to
/// choose a reader.
pub fn first_edition(head: &[u8]) -> Option<u8> {
    let start = find_grib(head, 0)?;
    head.get(start + 7).copied()
}

/// Walk every message in `data`. `max_records` bounds the list a hostile file can force.
pub fn scan(data: &[u8], max_records: usize) -> Result<ScanResult, VortexError> {
    let mut result = ScanResult::default();
    let mut position = 0;
    while let Some(start) = find_grib(data, position) {
        let Some(&edition) = data.get(start + 7) else {
            // Trailing bytes that happen to spell GRIB, too short to be a message.
            break;
        };
        if edition != 1 {
            result.other_editions += 1;
            position = start + 8;
            continue;
        }
        if result.messages.len() >= max_records {
            return Err(VortexError::Refused {
                what: "GRIB1 file".to_string(),
                reason: format!("more than {max_records} records"),
            });
        }
        let message = scan_message(data, start)?;
        position = message.end;
        result.messages.push(message);
    }
    Ok(result)
}

fn section(
    data: &[u8],
    start: usize,
    at: usize,
    end: usize,
    name: &str,
    minimum: usize,
) -> Result<Range<usize>, VortexError> {
    let length = u24(data, at)
        .ok_or_else(|| decode_error(format!("message at {start} is truncated in the {name}")))?;
    let section_end = at
        .checked_add(length)
        .filter(|&e| length >= minimum && e <= end)
        .ok_or_else(|| {
            decode_error(format!(
                "message at {start}: {name} length {length} does not fit"
            ))
        })?;
    Ok(at..section_end)
}

fn scan_message(data: &[u8], start: usize) -> Result<Message, VortexError> {
    let total = u24(data, start + 4)
        .ok_or_else(|| decode_error(format!("message at {start} is truncated in section 0")))?;
    // Grib1RecordScanner.getFixedTotalLengthEcmwfLargeGrib: ECMWF's convention for messages over
    // 8 MB sets the top bit. NCEP Stage IV messages are about 1.3 MB, so it is not ported.
    if total & 0x80_0000 != 0 {
        return Err(VortexError::Refused {
            what: "GRIB1 message".to_string(),
            reason: format!(
                "message at {start} uses ECMWF's large-message length convention; no Stage IV \
                 product has one"
            ),
        });
    }
    let end = start
        .checked_add(total)
        .filter(|&e| e <= data.len() && total >= 8 + 28 + 11 + 4)
        .ok_or_else(|| decode_error(format!("message at {start} declares length {total}")))?;
    if &data[end - 4..end] != b"7777" {
        return Err(decode_error(format!(
            "message at {start}: no end marker at its declared length {total}"
        )));
    }
    let body_end = end - 4;
    // Section 1 is at least 28 octets (octets 27-28 are the decimal scale factor).
    let product = section(data, start, start + 8, body_end, "PDS", 28)?;
    let flags = data[product.start + 7];
    let mut position = product.end;
    let grid = if flags & 0x80 != 0 {
        let range = section(data, start, position, body_end, "GDS", 32)?;
        position = range.end;
        Some(range)
    } else {
        None
    };
    let bitmap = if flags & 0x40 != 0 {
        let range = section(data, start, position, body_end, "BMS", 6)?;
        position = range.end;
        Some(range)
    } else {
        None
    };
    let binary = section(data, start, position, body_end, "BDS", 11)?;
    // Octets between the BDS and `7777` are padding some writers leave. Grib1RecordScanner reads
    // the end marker at the total length, not after the BDS, and so does this.
    Ok(Message {
        start,
        end,
        product,
        grid,
        bitmap,
        data: binary,
    })
}
