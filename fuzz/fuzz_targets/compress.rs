#![no_main]
#[macro_use] extern crate libfuzzer_sys;
extern crate quicklz;

use std::io::Cursor;

use quicklz::*;
use quicklz::errors::*;

fuzz_target!(|data: &[u8]| {
    let res = test(data);
    if res.is_err() {
        println!("Input: {:?}", data)
    }
    res.unwrap();
});

fn test(data: &[u8]) -> Result<()> {
    // Compress data
    let lvl1 = compress(data, CompressionLevel::Lvl1);
    let lvl3 = compress(data, CompressionLevel::Lvl3);

    // Try to decompress again
    let dec1 = decompress(&mut Cursor::new(&lvl1), data.len() as u32)?;
    let dec3 = decompress(&mut Cursor::new(&lvl3), data.len() as u32)?;
    assert_eq!(dec1, data);
    assert_eq!(dec3, data);
    Ok(())
}
