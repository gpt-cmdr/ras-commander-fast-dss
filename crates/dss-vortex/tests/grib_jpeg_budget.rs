//! A small hostile JPEG 2000 message must give a typed error under default limits, never reach the
//! decoder's infallible allocations with more than the system can give.
//!
//! An allocator records live bytes and can refuse large requests, as a process memory limit does.
//! The working-memory estimate must hold above what the decoder really takes, on streams built to
//! stress each thing it counts. One `#[test]` only, so no other test thread adds to the counts.

use dss_vortex::error::VortexError;
use dss_vortex::source::grib::{jpeg2000_required_bytes, Grib2File, Limits};
use std::alloc::{GlobalAlloc, Layout, System};
use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};

struct Recording;

static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);
static LARGEST: AtomicUsize = AtomicUsize::new(0);
static REFUSE_FROM: AtomicUsize = AtomicUsize::new(usize::MAX);
/// The reservation's size. Its release restarts the peak, so the peak is the decoder's.
static RESERVATION: AtomicUsize = AtomicUsize::new(0);

fn note(size: usize) {
    LARGEST.fetch_max(size, Ordering::SeqCst);
    let live = LIVE.fetch_add(size, Ordering::SeqCst) + size;
    PEAK.fetch_max(live, Ordering::SeqCst);
}

unsafe impl GlobalAlloc for Recording {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if layout.size() >= REFUSE_FROM.load(Ordering::SeqCst) {
            return std::ptr::null_mut();
        }
        note(layout.size());
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        if layout.size() >= REFUSE_FROM.load(Ordering::SeqCst) {
            return std::ptr::null_mut();
        }
        note(layout.size());
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        let live = LIVE.fetch_sub(layout.size(), Ordering::SeqCst) - layout.size();
        if RESERVATION
            .compare_exchange(layout.size(), 0, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
        {
            PEAK.store(live, Ordering::SeqCst);
        }
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        if new_size >= REFUSE_FROM.load(Ordering::SeqCst) {
            return std::ptr::null_mut();
        }
        // Moved blocks hold both sizes for a moment.
        note(new_size);
        LIVE.fetch_sub(layout.size(), Ordering::SeqCst);
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static ALLOCATOR: Recording = Recording;

fn section(bytes: &[u8], number: u8) -> usize {
    let mut at = 16;
    loop {
        let length = u32::from_be_bytes(bytes[at..at + 4].try_into().unwrap()) as usize;
        if bytes[at + 4] == number {
            return at;
        }
        at += length;
    }
}

fn fixture() -> Vec<u8> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/public/grib");
    fs::read(dir.join("jpeg.grib2")).unwrap()
}

fn set(bytes: &mut [u8], at: usize, value: u32) {
    bytes[at..at + 4].copy_from_slice(&value.to_be_bytes());
}

/// The fixture carrying `stream` in section 7, with its grid and count set to `width` x `height`.
fn message(stream: &[u8], width: u32, height: u32) -> Vec<u8> {
    let mut bytes = fixture();
    let cells = u64::from(width) * u64::from(height);
    let grid = section(&bytes, 3);
    set(&mut bytes, grid + 6, cells as u32);
    set(&mut bytes, grid + 30, width);
    set(&mut bytes, grid + 34, height);
    let representation = section(&bytes, 5);
    set(&mut bytes, representation + 5, cells as u32);
    bytes.truncate(section(&bytes, 7));
    bytes.extend_from_slice(&(5 + stream.len() as u32).to_be_bytes());
    bytes.push(7);
    bytes.extend_from_slice(stream);
    bytes.extend_from_slice(b"7777");
    let total = bytes.len() as u64;
    bytes[8..16].copy_from_slice(&total.to_be_bytes());
    bytes
}

fn code_stream() -> Vec<u8> {
    let bytes = fixture();
    let data = section(&bytes, 7);
    let length = u32::from_be_bytes(bytes[data..data + 4].try_into().unwrap()) as usize;
    bytes[data + 5..data + length].to_vec()
}

fn marker(stream: &[u8], code: u8) -> usize {
    stream
        .windows(2)
        .position(|w| w == [0xFF, code])
        .unwrap_or_else(|| panic!("no marker 0xFF{code:02X}"))
}

const SIZ: usize = 2;
const SOT: u8 = 0x90;
const COD: u8 = 0x52;

/// SOC, SIZ, then the fixture's QCD, a COD of our own, and one empty tile part per tile.
#[allow(clippy::too_many_arguments)]
fn built(
    width: u32,
    height: u32,
    tile: (u32, u32),
    order: u8,
    layers: u16,
    levels: u8,
    block: (u8, u8),
    style: u8,
    precincts: &[u8],
    body: &[u8],
) -> Vec<u8> {
    let source = code_stream();
    let mut out = vec![0xFF, 0x4F, 0xFF, 0x51];
    let depth = source[SIZ + 2 + 36 + 2];
    out.extend_from_slice(&41u16.to_be_bytes());
    out.extend_from_slice(&[0, 0]);
    for v in [width, height, 0, 0, tile.0, tile.1, 0, 0] {
        out.extend_from_slice(&v.to_be_bytes());
    }
    out.extend_from_slice(&[0, 1, depth, 1, 1]);
    // COD.
    out.extend_from_slice(&[0xFF, 0x52]);
    out.extend_from_slice(&(12 + precincts.len() as u16).to_be_bytes());
    out.push(u8::from(!precincts.is_empty()));
    out.push(order);
    out.extend_from_slice(&layers.to_be_bytes());
    out.extend_from_slice(&[0, levels, block.0, block.1, style, 1]);
    out.extend_from_slice(precincts);
    // QCD: reversible, one exponent per sub-band.
    let bands = 3 * usize::from(levels) + 1;
    out.extend_from_slice(&[0xFF, 0x5C]);
    out.extend_from_slice(&(3 + bands as u16).to_be_bytes());
    out.push(0x40);
    out.resize(out.len() + bands, 0x48);
    let tiles = width.div_ceil(tile.0) * height.div_ceil(tile.1);
    for index in 0..tiles {
        let data: &[u8] = if index == 0 { body } else { &[] };
        out.extend_from_slice(&[0xFF, SOT, 0, 10]);
        out.extend_from_slice(&(index as u16).to_be_bytes());
        out.extend_from_slice(&(14 + data.len() as u32).to_be_bytes());
        out.extend_from_slice(&[0, 1, 0xFF, 0x93]);
        out.extend_from_slice(data);
    }
    out.extend_from_slice(&[0xFF, 0xD9]);
    out
}

struct Observed {
    result: Result<Vec<f32>, VortexError>,
    peak: usize,
    largest: usize,
}

fn decode(bytes: Vec<u8>, limits: Limits) -> Observed {
    let file = Grib2File::from_bytes("x.grib2", bytes, limits).unwrap();
    let before = LIVE.load(Ordering::SeqCst);
    PEAK.store(before, Ordering::SeqCst);
    LARGEST.store(0, Ordering::SeqCst);
    let result = file.decode_record(0);
    Observed {
        peak: PEAK.load(Ordering::SeqCst) - before,
        largest: LARGEST.load(Ordering::SeqCst),
        result,
    }
}

fn assert_refused(label: &str, stream: Vec<u8>, width: u32, height: u32, expect: &str) {
    let seen = decode(message(&stream, width, height), Limits::default());
    let error = match seen.result {
        Ok(_) => panic!("{label}: decoded"),
        Err(e) => e.to_string(),
    };
    assert!(error.contains(expect), "{label}: {error}");
    assert!(
        seen.largest < 1 << 20,
        "{label}: asked for {}",
        seen.largest
    );
}

/// The recheck's reproduction: 821 bytes, the fixture with larger dimensions and no more payload.
fn resized(size: u32) -> Vec<u8> {
    let mut stream = code_stream();
    for at in [SIZ + 2 + 4, SIZ + 2 + 8, SIZ + 2 + 20, SIZ + 2 + 24] {
        set(&mut stream, at, size);
    }
    message(&stream, size, size)
}

fn quiet_panics() {
    // The decoder panics on some hostile streams. The guard turns that into an error, but the
    // default hook would print each one.
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        if info
            .location()
            .is_some_and(|l| l.file().ends_with("grib_jpeg_budget.rs"))
        {
            default(info);
        }
    }));
}

#[test]
fn hostile_code_streams_are_refused_and_the_estimate_holds() {
    quiet_panics();

    // 1. Under default limits this reached a 1 GiB infallible request, and aborted under a limit.
    let bytes = resized(16384);
    assert!(bytes.len() < 1024);
    let seen = decode(bytes, Limits::default());
    let error = seen.result.unwrap_err().to_string();
    assert!(error.contains("over the limit"), "{error}");
    assert!(seen.largest < 1 << 20, "asked for {}", seen.largest);

    // 2. Under the budget but over what the process may have: a typed error, not an abort. If the
    // reservation were skipped the decoder's own request would fail here and abort this test.
    let bytes = resized(4096);
    let stream_size = code_stream();
    let required = {
        let mut stream = stream_size.clone();
        for at in [SIZ + 2 + 4, SIZ + 2 + 8, SIZ + 2 + 20, SIZ + 2 + 24] {
            set(&mut stream, at, 4096);
        }
        jpeg2000_required_bytes(&stream).unwrap()
    };
    assert!(required < Limits::default().max_jpeg2000_bytes);
    REFUSE_FROM.store(1 << 26, Ordering::SeqCst);
    let seen = decode(bytes, Limits::default());
    REFUSE_FROM.store(usize::MAX, Ordering::SeqCst);
    let error = seen.result.unwrap_err().to_string();
    assert!(error.contains("could not allocate"), "{error}");

    // 3. Each way a header could differ from what the check read, or that it cannot price.
    let plain = code_stream();
    let cod = marker(&plain, COD);
    let sot = marker(&plain, SOT);
    let (w, h) = (
        u32::from_be_bytes(plain[SIZ + 2 + 4..SIZ + 2 + 8].try_into().unwrap()),
        u32::from_be_bytes(plain[SIZ + 2 + 8..SIZ + 2 + 12].try_into().unwrap()),
    );

    let insert = |at: usize, added: &[u8]| {
        let mut stream = plain.clone();
        stream.splice(at..at, added.iter().copied());
        stream
    };
    let in_tile_part = |added: &[u8]| {
        // Directly after SOT, with Psot grown to keep the walk aligned.
        let mut stream = insert(sot + 12, added);
        let psot = u32::from_be_bytes(stream[sot + 6..sot + 10].try_into().unwrap());
        if psot != 0 {
            set(&mut stream, sot + 6, psot + added.len() as u32);
        }
        stream
    };
    // A COD asking for tiny precincts, as the decoder would apply it to this tile.
    let tiny_cod = [0xFF, 0x52, 0, 13, 1, 0, 0, 32, 0, 0, 0, 0, 0, 1, 0x00];

    assert_refused(
        "COD in a tile part",
        in_tile_part(&tiny_cod),
        w,
        h,
        "tile-part header",
    );
    assert_refused(
        "COC in a tile part",
        in_tile_part(&[0xFF, 0x53, 0, 9, 0, 0, 0, 0, 0, 0, 1]),
        w,
        h,
        "tile-part header",
    );
    assert_refused(
        "QCD in a tile part",
        in_tile_part(&[0xFF, 0x5C, 0, 4, 0, 0x48]),
        w,
        h,
        "tile-part header",
    );
    assert_refused(
        "PPT in a tile part",
        in_tile_part(&[0xFF, 0x61, 0, 4, 0, 0]),
        w,
        h,
        "tile-part header",
    );
    assert_refused(
        "COC in the main header",
        insert(cod, &[0xFF, 0x53, 0, 9, 0, 0, 0, 0, 0, 0, 1]),
        w,
        h,
        "main header",
    );
    assert_refused(
        "QCC in the main header",
        insert(cod, &[0xFF, 0x5D, 0, 5, 0, 0, 0x48]),
        w,
        h,
        "main header",
    );
    assert_refused(
        "PPM in the main header",
        insert(cod, &[0xFF, 0x60, 0, 3, 0]),
        w,
        h,
        "main header",
    );
    assert_refused("a second COD", insert(cod, &tiny_cod), w, h, "main header");

    // Length fields the decoder ignores: a walk trusting them would read something else.
    let mut stream = plain.clone();
    stream[SIZ + 3] += 1;
    assert_refused("SIZ length", stream, w, h, "SIZ length");
    let mut stream = plain.clone();
    stream[cod + 3] += 1;
    assert_refused("COD length", stream, w, h, "COD length");
    let mut stream = plain.clone();
    stream[sot + 3] += 1;
    assert_refused("SOT length", stream, w, h, "SOT length");

    // Offsets, subsampling, and several components.
    let mut stream = plain.clone();
    set(&mut stream, SIZ + 2 + 12, 1);
    assert_refused("image offset", stream, w, h, "offsets");
    let mut stream = plain.clone();
    stream[SIZ + 2 + 39] = 2;
    assert_refused("subsampling", stream, w, h, "subsampling");
    let mut stream = insert(SIZ + 2 + 41, &[7, 1, 1]);
    stream[SIZ + 2 + 37] = 2;
    stream[SIZ + 3] += 3;
    assert_refused("two components", stream, w, h, "more than one component");

    // A JP2 wrapper brings its own boxes.
    let mut stream = b"\x00\x00\x00\x0C\x6A\x50\x20\x20\x0D\x0A\x87\x0A".to_vec();
    stream.extend_from_slice(&plain);
    assert_refused("JP2", stream, w, h, "JP2");

    // Tiles the file holds nothing for: 1 x 1 tiles over a large image.
    let mut stream = plain.clone();
    for at in [SIZ + 2 + 20, SIZ + 2 + 24] {
        set(&mut stream, at, 1);
    }
    for at in [SIZ + 2 + 4, SIZ + 2 + 8] {
        set(&mut stream, at, 8192);
    }
    assert_refused("tiny tiles", stream, 8192, 8192, "tiles");
    // A tile part naming no tile, and tiles with no part.
    let stream = built(64, 64, (32, 32), 0, 1, 1, (4, 4), 0, &[], &[]);
    let last = stream.len() - 14 - 2;
    let mut missing = stream.clone();
    missing[last + 4..last + 6].copy_from_slice(&0u16.to_be_bytes());
    assert_refused("a tile with no part", missing, 64, 64, "no tile part");
    let mut wrong = stream;
    wrong[last + 4..last + 6].copy_from_slice(&99u16.to_be_bytes());
    assert_refused("tile index", wrong, 64, 64, "tile index");

    // A thin image needs almost no memory and 4^level visits to build each tag tree.
    let thin = built(1 << 20, 1, (1 << 20, 1), 0, 1, 0, (0, 0), 0, &[], &[]);
    let seen = decode(message(&thin, 1 << 20, 1), Limits::default());
    let error = seen.result.unwrap_err().to_string();
    assert!(error.contains("tag trees"), "{error}");

    // 4. The real fixture decodes exactly, and within its estimate.
    let required = jpeg2000_required_bytes(&plain).unwrap() as usize;
    RESERVATION.store(required, Ordering::SeqCst);
    let seen = decode(fixture(), Limits::default());
    assert!(seen.result.is_ok());
    assert!(seen.peak <= required, "{} over {required}", seen.peak);

    // 5. Every thing the estimate counts, stressed. Junk bodies drive the packet parser so that
    // segments are pushed; most of these then fail, which is fine. What matters is the peak.
    let junk: Vec<u8> = (0..4096u32)
        .map(|i| (i.wrapping_mul(2654435761) >> 13) as u8 | 0x80)
        .collect();
    let levels_of = |r: usize, v: u8| vec![v; r];
    let cases: Vec<(&str, Vec<u8>, u32, u32)> = vec![
        // The ordinary shape, where the sample buffers dominate and the estimate is tightest.
        (
            "ordinary",
            built(3000, 1500, (3000, 1500), 0, 1, 5, (4, 4), 0, &[], &[]),
            3000,
            1500,
        ),
        // Tiny precincts: one per sample at the top resolutions.
        (
            "precincts",
            built(
                256,
                256,
                (256, 256),
                0,
                1,
                2,
                (0, 0),
                0,
                &levels_of(3, 0x11),
                &[],
            ),
            256,
            256,
        ),
        (
            "precincts, 32 layers",
            built(
                256,
                256,
                (256, 256),
                0,
                32,
                2,
                (0, 0),
                0,
                &levels_of(3, 0x11),
                &[],
            ),
            256,
            256,
        ),
        // Position orders collect and sort every precinct.
        (
            "precincts, order 2",
            built(
                256,
                256,
                (256, 256),
                2,
                1,
                2,
                (0, 0),
                0,
                &levels_of(3, 0x11),
                &[],
            ),
            256,
            256,
        ),
        (
            "precincts, order 3",
            built(
                256,
                256,
                (256, 256),
                3,
                2,
                1,
                (0, 0),
                0,
                &levels_of(2, 0x11),
                &[],
            ),
            256,
            256,
        ),
        (
            "precincts, order 4",
            built(
                256,
                256,
                (256, 256),
                4,
                2,
                1,
                (0, 0),
                0,
                &levels_of(2, 0x11),
                &[],
            ),
            256,
            256,
        ),
        // Smallest code blocks, long thin blocks.
        (
            "small blocks",
            built(1024, 1024, (1024, 1024), 0, 4, 3, (0, 0), 0, &[], &[]),
            1024,
            1024,
        ),
        (
            "thin blocks",
            built(1024, 512, (1024, 512), 0, 1, 0, (8, 0), 0, &[], &[]),
            1024,
            512,
        ),
        // Many tiles, some levels. Each clones the header.
        (
            "tiles",
            built(512, 512, (16, 16), 0, 1, 2, (2, 2), 0, &[], &[]),
            512,
            512,
        ),
        (
            "tiles with precincts",
            built(
                512,
                512,
                (64, 64),
                2,
                3,
                2,
                (0, 0),
                0,
                &levels_of(3, 0x22),
                &[],
            ),
            512,
            512,
        ),
        (
            "uneven tiles",
            built(1000, 700, (333, 251), 0, 1, 3, (1, 1), 0, &[], &[]),
            1000,
            700,
        ),
        // Many decomposition levels on a small image.
        (
            "levels",
            built(512, 512, (512, 512), 0, 1, 32, (4, 4), 0, &[], &[]),
            512,
            512,
        ),
        // A body that makes the parser run hard over bytes that are not a real packet.
        (
            "junk body",
            built(512, 512, (512, 512), 1, 4, 2, (1, 1), 0b101, &[], &junk),
            512,
            512,
        ),
    ];
    // One code block per precinct, so one bit each settles its tag trees. A packet of one byte is
    // then nonzero, included, no missing planes, one pass, Lblock unchanged, length zero: one
    // segment per byte. The most the estimate allows for is 8/3 per byte.
    let no_body = built(1024, 1024, (1024, 1024), 0, 2, 0, (0, 0), 0, &[0x22], &[]);
    let mut packets = vec![0b1110_0000u8; 65536];
    packets.resize(2 * 65536, 0b1100_0000);
    let with_segments = built(
        1024,
        1024,
        (1024, 1024),
        0,
        2,
        0,
        (0, 0),
        0,
        &[0x22],
        &packets,
    );
    RESERVATION.store(
        jpeg2000_required_bytes(&no_body).unwrap() as usize,
        Ordering::SeqCst,
    );
    let bare = decode(message(&no_body, 1024, 1024), Limits::default()).peak;
    let required = jpeg2000_required_bytes(&with_segments).unwrap() as usize;
    RESERVATION.store(required, Ordering::SeqCst);
    let seen = decode(message(&with_segments, 1024, 1024), Limits::default());
    println!("segments: took {}, estimated {required}", seen.peak);
    assert!(seen.result.is_ok());
    assert!(
        seen.peak <= required,
        "took {}, estimated {required}",
        seen.peak
    );
    assert!(
        seen.peak >= bare + packets.len() * 24,
        "took {} with segments, {bare} without",
        seen.peak
    );

    for (label, stream, width, height) in cases {
        let required =
            jpeg2000_required_bytes(&stream).unwrap_or_else(|e| panic!("{label}: {e}")) as usize;
        RESERVATION.store(required, Ordering::SeqCst);
        let seen = decode(message(&stream, width, height), Limits::default());
        assert_eq!(
            RESERVATION.load(Ordering::SeqCst),
            0,
            "{label}: no reservation"
        );
        assert!(
            seen.peak <= required,
            "{label}: took {} bytes, estimated {required}",
            seen.peak
        );
        println!("{label}: took {}, estimated {required}", seen.peak);
        // The structure was really built: a refusal here would prove nothing about the estimate.
        if let Err(VortexError::Refused { reason, .. }) = &seen.result {
            panic!("{label}: refused: {reason}");
        }
        assert!(
            seen.peak > required / 200,
            "{label}: took only {} of {required}",
            seen.peak
        );
    }
}
