extern crate afl;
extern crate quicklz;

use std::io::Cursor;

use quicklz::*;

fn main() {
    afl::read_stdio_bytes(|data| {
        let lvl1 = compress(&data, CompressionLevel::Lvl1);
        let lvl3 = compress(&data, CompressionLevel::Lvl3);

        // Try to decompress again
        let dec1 = decompress(&mut Cursor::new(&lvl1), data.len() as u32).unwrap();
        let dec3 = decompress(&mut Cursor::new(&lvl3), data.len() as u32).unwrap();
        assert_eq!(dec1, data);
        assert_eq!(dec3, data);
    });
}
