#![no_main]
#[macro_use] extern crate libfuzzer_sys;
extern crate quicklz;

use std::io::Cursor;

use quicklz::*;

fuzz_target!(|data: &[u8]| {
    // Try to decompress
    let _ = decompress(&mut Cursor::new(data), data.len() as u32 * 2);
});
