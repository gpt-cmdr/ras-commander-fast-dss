//! Test harness: the `tiff` crate's decoder on one file, for the P50 process-limit evidence.
//! Usage: tiff_crate_probe <input>
use std::io::Cursor;
use tiff::decoder::{Decoder, Limits};

fn main() {
    let path = std::env::args().nth(1).unwrap();
    let bytes = std::fs::read(path).unwrap();
    let mut decoder = Decoder::new(Cursor::new(bytes))
        .unwrap()
        .with_limits(Limits::unlimited());
    match decoder.read_image() {
        Ok(_) => println!("decoded"),
        Err(e) => println!("error: {e}"),
    }
}
