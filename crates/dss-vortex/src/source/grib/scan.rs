//! Finds GRIB2 messages and their sections, including repeated sections.
//!
//! Rules from NetCDF-Java 5.5.3 `Grib2RecordScanner.hasNext` and `nextRepeating`: bytes between
//! messages are skipped, and a message may repeat sections 2, 3 or 4 onward, each repeat being
//! one record. Section 6 indicator 254 reuses the last bitmap in the message.
//!
//! Every length comes from the file, so every range is checked here. A malformed message is an
//! error, not a guess. `Grib2RecordScanner` tolerates some of these and then fails or yields
//! empty data later; refusing them is observed behaviour we did not implement.

use crate::error::VortexError;
use std::ops::Range;

/// One record: the sections its decoding needs, as byte ranges into the file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordSpan {
    /// Byte offset of the `GRIB` that starts the message.
    pub message_start: usize,
    /// Section 0 octet 7.
    pub discipline: u8,
    pub identification: Range<usize>,
    pub grid: Range<usize>,
    pub product: Range<usize>,
    pub representation: Range<usize>,
    pub bitmap: Range<usize>,
    pub data: Range<usize>,
}

/// What a scan found.
#[derive(Debug, Default)]
pub struct ScanResult {
    pub records: Vec<RecordSpan>,
    /// Count of messages with an edition other than 2. `GRIB1` is refused above this layer.
    pub other_editions: usize,
}

fn decode_error(message: String) -> VortexError {
    VortexError::Decode {
        format: "GRIB2",
        message,
    }
}

fn find_grib(data: &[u8], from: usize) -> Option<usize> {
    data.get(from..)?
        .windows(4)
        .position(|w| w == b"GRIB")
        .map(|p| p + from)
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
        if edition != 2 {
            result.other_editions += 1;
            position = start + 8;
            continue;
        }
        let end = scan_message(data, start, &mut result, max_records)?;
        position = end;
    }
    Ok(result)
}

fn be_u32(data: &[u8], at: usize) -> Option<u32> {
    let bytes = data.get(at..at.checked_add(4)?)?;
    Some(u32::from_be_bytes(bytes.try_into().ok()?))
}

fn scan_message(
    data: &[u8],
    start: usize,
    result: &mut ScanResult,
    max_records: usize,
) -> Result<usize, VortexError> {
    let header = data
        .get(start..start.saturating_add(16))
        .ok_or_else(|| decode_error(format!("message at {start} is truncated in section 0")))?;
    let discipline = header[6];
    let length = u64::from_be_bytes(
        header[8..16]
            .try_into()
            .map_err(|_| decode_error("section 0 length".to_string()))?,
    );
    let end = usize::try_from(length)
        .ok()
        .and_then(|n| start.checked_add(n))
        .filter(|&e| e <= data.len() && e >= start + 16 + 4)
        .ok_or_else(|| decode_error(format!("message at {start} declares length {length}")))?;

    let mut identification = None;
    let mut grid = None;
    let mut product = None;
    let mut representation = None;
    let mut bitmap = None;
    let mut last_explicit_bitmap = None;
    let mut position = start + 16;
    loop {
        if data.get(position..position + 4) == Some(b"7777".as_slice()) {
            if position + 4 > end {
                return Err(decode_error(format!(
                    "message at {start}: end marker past the declared length"
                )));
            }
            return Ok(end);
        }
        let section_length = be_u32(data, position)
            .and_then(|n| usize::try_from(n).ok())
            .ok_or_else(|| decode_error(format!("message at {start} is truncated")))?;
        let section_number = *data
            .get(position + 4)
            .ok_or_else(|| decode_error(format!("message at {start} is truncated")))?;
        let section_end = position
            .checked_add(section_length)
            .filter(|&e| section_length >= 5 && e <= end)
            .ok_or_else(|| {
                decode_error(format!(
                    "message at {start}: section {section_number} length {section_length} \
                     does not fit"
                ))
            })?;
        let range = position..section_end;
        match section_number {
            1 => identification = Some(range),
            2 => {}
            3 => grid = Some(range),
            4 => product = Some(range),
            5 => representation = Some(range),
            6 => {
                let indicator = *data
                    .get(position + 5)
                    .ok_or_else(|| decode_error(format!("message at {start}: empty section 6")))?;
                if indicator == 254 {
                    bitmap = Some(last_explicit_bitmap.clone().ok_or_else(|| {
                        decode_error(format!(
                            "message at {start}: bitmap indicator 254 with no earlier bitmap"
                        ))
                    })?);
                } else {
                    if indicator == 0 {
                        last_explicit_bitmap = Some(range.clone());
                    }
                    bitmap = Some(range);
                }
            }
            7 => {
                let missing = |what: &str| {
                    decode_error(format!("message at {start}: section 7 with no {what}"))
                };
                if result.records.len() >= max_records {
                    return Err(VortexError::Refused {
                        what: "GRIB2 file".to_string(),
                        reason: format!("more than {max_records} records"),
                    });
                }
                result.records.push(RecordSpan {
                    message_start: start,
                    discipline,
                    identification: identification.clone().ok_or_else(|| missing("section 1"))?,
                    grid: grid.clone().ok_or_else(|| missing("section 3"))?,
                    product: product.clone().ok_or_else(|| missing("section 4"))?,
                    representation: representation.clone().ok_or_else(|| missing("section 5"))?,
                    bitmap: bitmap.clone().ok_or_else(|| missing("section 6"))?,
                    data: range,
                });
            }
            other => {
                return Err(decode_error(format!(
                    "message at {start}: unknown section {other}"
                )))
            }
        }
        position = section_end;
    }
}
