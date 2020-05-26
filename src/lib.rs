//! QuickLZ is a fast compression algorithm. This library implements the
//! algorithm version 1.5.0 (latest version since 2011). Compression and
//! decompression are implemented for the compression levels 1 and 3.

#![cfg_attr(feature = "cargo-clippy",
           allow(verbose_bit_mask, unreadable_literal))]

#![cfg_attr(feature = "nightly", feature(test))]

#[cfg(feature = "nightly")]
extern crate test;

use thiserror::Error;
use std::cell::RefCell;
use std::cmp;
use std::io::{Read, Write};
use bit_vec::BitVec;
use byteorder::{LittleEndian, ReadBytesExt, WriteBytesExt};

type Result<T> = std::result::Result<T, Error>;

const HASHTABLE_SIZE: usize = 4096;
// hashtable_count MUST be 2^x for maximum efficiency
const HASHTABLE_COUNT: usize = 1 << 4;

// Thread-local buffers for various hashtables used by different
// (de-)compression levels so they don't have to be reallocated each call.
thread_local! {
    static HASHTABLE: RefCell<Box<[u32; HASHTABLE_SIZE]>>
        = RefCell::new(Box::new([0; HASHTABLE_SIZE]));
    static HASHTABLE_ARR: RefCell<Box<[[u32; HASHTABLE_SIZE]; HASHTABLE_COUNT]>>
        = RefCell::new(Box::new([[0; HASHTABLE_SIZE]; HASHTABLE_COUNT]));
    static CACHETABLE: RefCell<Box<[u32; HASHTABLE_SIZE]>>
        = RefCell::new(Box::new([0; HASHTABLE_SIZE]));
    static HASHCOUNTER_U8: RefCell<Box<[u8; HASHTABLE_SIZE]>>
        = RefCell::new(Box::new([0; HASHTABLE_SIZE]));
    static HASHCOUNTER_BIT: RefCell<BitVec>
        = RefCell::new(BitVec::from_elem(HASHTABLE_SIZE, false));
}

#[derive(Error, Debug)]
pub enum Error {
    #[error("{0}")]
    Io(#[from] std::io::Error),
    /// This library supports only level 1 and 3 if another level is detected,
    /// this error will be returned.
    #[error("Unsupported QuickLZ level, this library only supports \
        level 1 and 3")]
    UnsupportedLevel,
    /// If the given maximum decompressed size is exceeded, this error will be
    /// returned.
    #[error("Maximum uncompressed size exceeded: {}/{}", dec, max)]
    SizeLimitExceeded { dec: u32, max: u32 },
    /// If the compressed data cannot be decompressed, this error will be
    /// returned, containing a short description.
    #[error("{0}")]
    CorruptData(String),
}


#[derive(PartialEq, Eq, Hash, Debug, Clone, Copy)]
pub enum CompressionLevel {
    Lvl1,
    Lvl3,
}

impl CompressionLevel {
    fn as_u8(&self) -> u8 {
        match *self {
            CompressionLevel::Lvl1 => 1,
            CompressionLevel::Lvl3 => 3,
        }
    }
}

/// The state while decompressing.
enum DecompressState<'a> {
    /// Next hash position, hashtable
    Level1(usize, &'a mut [u32; HASHTABLE_SIZE]),
    Level3,
}

fn hash(val: u32) -> u32 {
    ((val >> 12) ^ val) & 0xfff
}

/// Copy `[start; start + length)` bytes from `buf` to the end of `buf`.
///
/// It is expected, that `start + length` does not overflow.
fn copy_buffer_bytes(
    buf: &mut Vec<u8>,
    mut start: usize,
    length: usize,
) -> Result<()> {
    let buf_len = buf.len();
    let end = start + length;

    // Use extend_from_slice for the longest part possible
    let copy_len = cmp::min(buf_len.saturating_sub(start), length);
    if copy_len >= 4 {
        buf.resize(buf_len + copy_len, 0);
        let (a, b) = buf.split_at_mut(buf_len);
        b.copy_from_slice(&a[start..(start + copy_len)]);
        start += copy_len;
    }

    // Copy the rest in a loop
    for i in start..end {
        let val = *buf.get(i).ok_or_else(|| Error::CorruptData(String::from(
            "Invalid back reference in QuickLZ")))?;
        buf.push(val);
    }
    Ok(())
}

/// Updates the hashtable for the data in `dest` between `start` and `end`.
fn update_hashtable(
    hashtable: &mut [u32; HASHTABLE_SIZE],
    dest: &[u8],
    start: usize,
    end: usize,
) {
    #[cfg_attr(feature = "cargo-clippy", allow(needless_range_loop))]
    for i in start..end {
        hashtable[hash(read_u24(&dest[i..])) as usize] = i as u32;
    }
}

fn read_u24(inp: &[u8]) -> u32 {
    u32::from(inp[0]) | u32::from(inp[1]) << 8 | u32::from(inp[2]) << 16
}

/// Decompress data read from `r`.
///
/// # Example
/// ```
/// # (|| -> Result<(), quicklz::Error> {
/// let data = [
///     0x47, 0x17, 0x00, 0x00, 0x00, 0x0a, 0x00, 0x00, 0x00, 0x00, 0x00,
///     0x00, 0x80, 0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08,
///     0x09,
/// ];
/// let mut r = std::io::Cursor::new(data.as_ref());
/// let dec = quicklz::decompress(&mut r, 1024)?;
///
/// assert_eq!(&dec, &vec![0, 1, 2, 3, 4, 5, 6, 7, 8, 9]);
/// # Ok(())
/// # })().unwrap()
/// ```
///
/// # Errors
/// If an error is returned, some bytes were already read from `r`, but some
/// of the data that belongs to the compressed part is maybe still in the
/// stream.
///
/// The decompressed size is limited to `max_size`. An error will be returned
/// if data would be larger.
///
/// If the incoming data are compressed using level 2, an error is returned.
pub fn decompress(r: &mut dyn Read, max_size: u32) -> Result<Vec<u8>> {
    let mut res = Vec::new();
    let mut control: u32 = 1;

    // Read header
    let flags = r.read_u8()?;
    let level = (flags >> 2) & 0b11;
    if level != 3 && level != 1 {
        return Err(Error::UnsupportedLevel);
    }
    let header_len = if flags & 2 == 2 { 9 } else { 3 };
    let dec_size;
    let comp_size;
    if header_len == 3 {
        comp_size = u32::from(r.read_u8()?);
        dec_size = u32::from(r.read_u8()?);
    } else {
        comp_size = r.read_u32::<LittleEndian>()?;
        dec_size = r.read_u32::<LittleEndian>()?;
    }
    if dec_size > max_size {
        return Err(Error::SizeLimitExceeded { dec: dec_size, max: max_size });
    }
    if comp_size < header_len {
        return Err(Error::CorruptData(format!(
            "Invalid compressed size: {}", comp_size)));
    }
    res.reserve(dec_size as usize);
    if flags & 1 != 1 {
        // Uncompressed
        if comp_size - header_len != dec_size {
            return Err(Error::CorruptData(format!(
                "Compressed and uncompressed size of uncompressed data do not \
                 match ({} != {})",
                comp_size - header_len,
                dec_size,
            )));
        }
        // Uncompressed
        res.resize(dec_size as usize, 0);
        r.read_exact(&mut res)?;
        return Ok(res);
    }

    HASHTABLE.with(|tab| -> Result<()> {
        let mut tab = tab.borrow_mut();
        **tab = [0u32; HASHTABLE_SIZE];

        let mut state = if level == 1 {
            DecompressState::Level1(0, &mut tab)
        } else {
            DecompressState::Level3
        };

        loop {
            if control == 1 {
                control = r.read_u32::<LittleEndian>()?;
            }
            if control & 1 == 1 {
                // Found a reference
                control >>= 1;
                let next = u32::from(r.read_u8()?);
                match state {
                    DecompressState::Level1(
                        ref mut next_hashed,
                        ref mut hashtable,
                    ) => {
                        let mut matchlen = (next & 0xf) as u8;
                        let hash = (next >> 4) | (u32::from(r.read_u8()?) << 4);
                        if matchlen != 0 {
                            matchlen += 2;
                        } else {
                            matchlen = r.read_u8()?;
                        }
                        if matchlen < 3 {
                            return Err(Error::CorruptData(format!(
                                "Too small length for QuickLZ reference ({})",
                                matchlen
                            )));
                        }
                        let offset = *hashtable
                            .get(hash as usize)
                            .ok_or_else(||
                                Error::CorruptData(String::from(
                                    "Invalid QuickLZ hashtable entry"))
                            )?;

                        // Check the size
                        if let Some(len) =
                            (res.len() as u32).checked_add(u32::from(matchlen))
                        {
                            if len > dec_size {
                                return Err(Error::CorruptData(format!(
                                    "Decompressed size exceeded ({})",
                                    dec_size
                                )));
                            }
                        } else {
                            return Err(Error::CorruptData(format!(
                                "Too big length in QuickLZ reference ({})",
                                matchlen
                            )));
                        };

                        copy_buffer_bytes(
                            &mut res,
                            offset as usize,
                            matchlen as usize,
                        )?;
                        let end = res.len() + 1 - matchlen as usize;
                        update_hashtable(
                            &mut *hashtable,
                            &res,
                            *next_hashed,
                            end,
                        );
                        *next_hashed = res.len();
                    }
                    DecompressState::Level3 => {
                        let offset;
                        let matchlen;
                        if next & 0b11 == 0 {
                            matchlen = 3;
                            offset = next >> 2;
                        } else if next & 0b11 == 0b01 {
                            matchlen = 3;
                            offset =
                                (next >> 2) | (u32::from(r.read_u8()?) << 6);
                        } else if next & 0b11 == 0b10 {
                            matchlen = 3 + ((next >> 2) & 0xf);
                            offset =
                                (next >> 6) | (u32::from(r.read_u8()?) << 2);
                        } else if next & 0x7f == 0b11 {
                            let next2 = u32::from(r.read_u8()?);
                            let next3 = u32::from(r.read_u8()?);
                            let next4 = u32::from(r.read_u8()?);
                            matchlen =
                                3 + ((next >> 7) | ((next2 & 0x7f) << 1));
                            offset = (next2 >> 7) | (next3 << 1) | (next4 << 1);
                        } else {
                            matchlen = 2 + ((next >> 2) & 0x1f);
                            offset = (next >> 7)
                                | (u32::from(r.read_u8()?) << 1)
                                | (u32::from(r.read_u8()?) << 9);
                        }

                        // Insert reference
                        if res.len() < offset as usize {
                            return Err(Error::CorruptData(String::from(
                                "Too big offset in QuickLZ reference")));
                        }
                        let start = res.len() - offset as usize;

                        // Check the size
                        if let Some(len) =
                            (res.len() as u32).checked_add(matchlen as u32)
                        {
                            if len > dec_size {
                                return Err(Error::CorruptData(format!(
                                    "Decompressed size exceeded ({})",
                                    dec_size
                                )));
                            }
                        } else {
                            return Err(Error::CorruptData(format!(
                                "Too big length in QuickLZ reference ({})",
                                matchlen
                            )));
                        };

                        copy_buffer_bytes(&mut res, start, matchlen as usize)?;
                    }
                }
            } else if res.len() >= cmp::max(dec_size as usize, 10) - 10 {
                while res.len() < dec_size as usize {
                    if control == 1 {
                        r.read_u32::<LittleEndian>()?;
                    }
                    control >>= 1;
                    res.push(r.read_u8()?);
                }
                break;
            } else {
                // Check the size
                if let Some(len) = res.len().checked_add(1) {
                    if len > dec_size as usize {
                        return Err(Error::CorruptData(format!("Decompressed size exceeded ({})", dec_size)));
                    }
                } else {
                    return Err(Error::CorruptData(format!("Decompressed size exceeded ({})", dec_size)));
                };

                res.push(r.read_u8()?);
                control >>= 1;
                if let DecompressState::Level1(
                    ref mut next_hashed,
                    ref mut hashtable,
                ) = state
                {
                    let end = res.len().saturating_sub(2);
                    update_hashtable(&mut *hashtable, &res, *next_hashed, end);
                    *next_hashed = cmp::max(*next_hashed, end);
                }
            }
        }
        Ok(())
    })?;
    Ok(res)
}

/// Checks if all elements in the slice have the same value.
fn is_eq<T: PartialEq>(arr: &[T]) -> bool {
    for i in 1..arr.len() {
        if arr[0] != arr[i] {
            return false;
        }
    }
    true
}

/// Writes the qlz header at the start of the dest vec.
fn write_header(
    dest: &mut [u8],
    dest_len: usize,
    srclen: usize,
    level: u8,
    compressed: bool,
) -> Result<()> {
    let flags: u8 = if compressed {
        0x01 | (level << 2) | 0x40 // compressed | level | always
    } else {
        (level << 2) | 0x40 //  (not compressed) | level | always
    };

    if dest.len() == 3 {
        // short header
        dest[0] = flags;
        dest[1] = dest_len as u8;
        dest[2] = srclen as u8;
    } else if dest.len() == 9 {
        // long header
        let mut vec = vec![];
        vec.write_u8(flags | 0x02)?;
        vec.write_u32::<LittleEndian>(dest_len as u32)?;
        vec.write_u32::<LittleEndian>(srclen as u32)?;
        dest.copy_from_slice(&vec);
    } else {
        panic!("The header length must be either 3 or 9.");
    }
    Ok(())
}

/// Writes an u32 control sequence.
fn write_control(dest: &mut [u8], ctrl_pos: usize, ctrl: u32) -> Result<()> {
    dest[ctrl_pos..ctrl_pos + 4]
        .as_mut()
        .write_u32::<LittleEndian>(ctrl)?;
    Ok(())
}

/// Compress `data` using the specified [`CompressionLevel`].
///
/// # Panic
///
/// This function panics if `data.len() > u32::max_value() - 400`.
///
/// [`CompressionLevel`]: enum.CompressionLevel.html
pub fn compress(data: &[u8], level: CompressionLevel) -> Vec<u8> {
    if data.len() >= (u32::max_value() - 400) as usize {
        panic!("QuickLZ can only compress up to {}", u32::max_value() - 400);
    }

    let headerlen: u8 = if data.len() < 216 { 3 } else { 9 };
    let mut dest = vec![0u8; headerlen as usize + 4];

    let mut control: u32 = 1 << 31;
    let mut control_pos: usize = headerlen as usize;
    let mut source_pos = 0;

    let mut done = false;

    if level == CompressionLevel::Lvl1 {
        HASHTABLE.with(|hashtable| -> Result<()> {
        HASHCOUNTER_BIT.with(|hash_counter| -> Result<()> {
        CACHETABLE.with(|cachetable| -> Result<()> {

        let mut hashtable = hashtable.borrow_mut();
        let mut hash_counter = hash_counter.borrow_mut();
        let mut cachetable = cachetable.borrow_mut();

        **hashtable = [0u32; HASHTABLE_SIZE];
        hash_counter.clear();
        **cachetable = [0u32; HASHTABLE_SIZE];

        let mut lits: u32 = 0;

        while source_pos + 10 < data.len() {
            if check_inefficient(& mut control, source_pos, level,
                &mut control_pos, &mut dest, data) {
                    done = true;
                    return Ok(());
            }

            let next = read_u24(&data[source_pos..]);
            let hash = hash(next);
            let hash_i = hash as usize;
            let offset = hashtable[hash_i];
            let cache = cachetable[hash_i];
            let counter = hash_counter[hash_i];
            cachetable[hash_i] = next;
            hashtable[hash_i] = source_pos as u32;

            if cache == next && counter
                && (source_pos as u32 - offset >= 3
                    || source_pos == (offset + 1) as usize && lits >= 3
                        && source_pos > 3
                        && is_eq(&data[source_pos - 3..source_pos + 3]))
            {
                control = (control >> 1) | (1 << 31);
                let mut matchlen = 3;
                let remainder = cmp::min(data.len() - 4 - source_pos, 0xff);
                while data[(offset + matchlen) as usize]
                    == data[source_pos + matchlen as usize]
                    && (matchlen as usize) < remainder
                {
                    matchlen += 1;
                }
                if matchlen < 18 {
                    dest.write_u16::<LittleEndian>(
                        (hash << 4 | (matchlen - 2)) as u16,
                    ).unwrap();
                } else {
                    dest.write_u24::<LittleEndian>(
                        (hash << 4 | (matchlen << 16)) as u32,
                    ).unwrap();
                }
                source_pos += matchlen as usize;
                lits = 0;
            } else {
                lits += 1;
                hash_counter.set(hash_i, true);

                dest.write_u8(data[source_pos]).unwrap();
                source_pos += 1;
                control >>= 1;
            }
        }
        Ok(())})?;
        Ok(())})?;
        Ok(())}).unwrap();

    } else if level == CompressionLevel::Lvl3 {
        HASHTABLE_ARR.with(|hashtable| -> Result<()> {
        HASHCOUNTER_U8.with(|hash_counter| -> Result<()> {

        let mut hashtable = hashtable.borrow_mut();
        let mut hash_counter = hash_counter.borrow_mut();

        **hashtable = [[0u32; HASHTABLE_SIZE]; HASHTABLE_COUNT];
        **hash_counter = [0u8; HASHTABLE_SIZE];

        while source_pos + 10 < data.len() {
            if check_inefficient(& mut control, source_pos, level,
                &mut control_pos, &mut dest, data) {
                    done = true;
                    return Ok(());
            }

            let next = &data[source_pos..source_pos + 3];
            let remainder = cmp::min(data.len() - 4 - source_pos, 0xff);
            let hash = hash(read_u24(next)) as usize;
            let counter = hash_counter[hash];
            let mut matchlen = 0;
            let mut offset = 0;

            #[cfg_attr(feature = "cargo-clippy", allow(needless_range_loop))]
            for index in 0..HASHTABLE_COUNT {
                if index as u8 >= counter {
                    break;
                }
                let hasht = &hashtable[index];
                let o = hasht[hash] as usize;

                if &data[o..o + 3] == next && o + 2 < source_pos {
                    let mut m = 3;
                    while data[o + m] == data[source_pos + m] && m < remainder {
                        m += 1;
                    }

                    if m > matchlen || (m == matchlen && o > offset) {
                        offset = o;
                        matchlen = m;
                    }
                }
            }

            hashtable[counter as usize % HASHTABLE_COUNT][hash] =
                source_pos as u32;
            hash_counter[hash] = counter.wrapping_add(1);

            if matchlen >= 3 && source_pos - offset < 0x1FFFF {
                offset = source_pos - offset;

                #[cfg_attr(feature = "cargo-clippy",
                           allow(needless_range_loop))]
                for u in 1..matchlen {
                    let hash =
                        crate::hash(read_u24(&data[(source_pos + u)..])) as usize;
                    let counter = hash_counter[hash];
                    hash_counter[hash] = counter.wrapping_add(1);
                    hashtable[counter as usize % HASHTABLE_COUNT][hash] = (source_pos + u) as u32;
                }

                source_pos += matchlen;
                control = (control >> 1) | (1 << 31);

                if matchlen == 3 && offset < (1 << 6) {
                    dest.write_u8((offset << 2) as u8).unwrap();
                } else if matchlen == 3 && offset < (1 << 14) {
                    dest.write_u16::<LittleEndian>(((offset << 2) | 1) as u16)
                        .unwrap();
                } else if (matchlen - 3) < (1 << 4) && offset < (1 << 12) {
                    dest.write_u16::<LittleEndian>(
                        ((offset << 6) | ((matchlen - 3) << 2) | 2) as u16,
                    ).unwrap();
                } else if (matchlen - 2) < (1 << 5) {
                    dest.write_u24::<LittleEndian>(
                        ((offset << 7) | ((matchlen - 2) << 2) | 3) as u32,
                    ).unwrap();
                } else {
                    dest.write_u32::<LittleEndian>(
                        ((offset << 15) | ((matchlen - 3) << 7) | 3) as u32,
                    ).unwrap();
                }
            } else {
                dest.write_u8(data[source_pos]).unwrap();
                source_pos += 1;
                control >>= 1;
            }
        }

        Ok(())})?;
        Ok(())}).unwrap();
    }

    if done {
        return dest;
    }

    while source_pos < data.len() {
        if control & 1 != 0 {
            write_control(&mut dest, control_pos, (control >> 1) | (1 << 31))
                .unwrap();
            control_pos = dest.len();
            dest.write_u32::<LittleEndian>(0).unwrap();
            control = 1 << 31;
        }
        dest.write_u8(data[source_pos]).unwrap();
        source_pos += 1;
        control >>= 1;
    }

    while control & 1 == 0 {
        control >>= 1;
    }
    write_control(&mut dest, control_pos, (control >> 1) | (1 << 31)).unwrap();

    let dest_len = dest.len();
    write_header(&mut dest[0..(headerlen as usize)], dest_len, data.len(),
        level.as_u8(), true).unwrap();
    dest
}

#[cfg_attr(feature = "cargo-clippy", allow(inline_always))]
// inline gives about 15%-20% performance boost since this method is used
// at a hot point of the compression method.
#[inline(always)]
fn check_inefficient(control: &mut u32, source_pos: usize,
    level: CompressionLevel, control_pos: &mut usize, dest: &mut Vec<u8>,
    data: &[u8]) -> bool {
    if *control & 1 != 0 {
        if source_pos > 3 * (data.len() / 4)
            && dest.len() > source_pos - (source_pos / 32)
        {
            let headerlen = if data.len() > 255 { 9 } else { 3 };
            dest.clear();
            // To prevent a double allocation we reserve the exact size needed
            // for the header and data.
            dest.reserve(headerlen + data.len());
            // Now we fill the header size with zeros since it will be
            // written by the write_header method.
            dest.resize(headerlen, 0);
            dest.write_all(data).unwrap();
            let dest_len = dest.len();
            write_header(
                &mut dest[0..headerlen],
                dest_len,
                data.len(),
                level.as_u8(),
                false,
            ).unwrap();
            return true;
        }
        write_control(dest, *control_pos, (*control >> 1) | (1 << 31)).unwrap();
        *control_pos = dest.len();
        dest.write_u32::<LittleEndian>(0).unwrap();
        *control = 1 << 31;
    }
    false
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;
    use std::iter;
    use super::*;

    #[test]
    fn continuous10() {
        let data = [
            0x47, 0x17, 0x00, 0x00, 0x00, 0x0a, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x80, 0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08,
            0x09,
        ];
        let orig: Vec<u8> = (0..10).collect();

        let mut r = Cursor::new(data.as_ref());
        let dec = decompress(&mut r, 1024).unwrap();
        assert_eq!(&orig, &dec);
    }

    #[test]
    fn continuous128() {
        let data = [
            0x47u8, 0x9d, 0x00, 0x00, 0x00, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x80, 0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08,
            0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e, 0x0f, 0x10, 0x11, 0x12, 0x13,
            0x14, 0x15, 0x16, 0x17, 0x18, 0x19, 0x1a, 0x1b, 0x1c, 0x1d, 0x1e,
            0x00, 0x00, 0x00, 0x80, 0x1f, 0x20, 0x21, 0x22, 0x23, 0x24, 0x25,
            0x26, 0x27, 0x28, 0x29, 0x2a, 0x2b, 0x2c, 0x2d, 0x2e, 0x2f, 0x30,
            0x31, 0x32, 0x33, 0x34, 0x35, 0x36, 0x37, 0x38, 0x39, 0x3a, 0x3b,
            0x3c, 0x3d, 0x00, 0x00, 0x00, 0x80, 0x3e, 0x3f, 0x40, 0x41, 0x42,
            0x43, 0x44, 0x45, 0x46, 0x47, 0x48, 0x49, 0x4a, 0x4b, 0x4c, 0x4d,
            0x4e, 0x4f, 0x50, 0x51, 0x52, 0x53, 0x54, 0x55, 0x56, 0x57, 0x58,
            0x59, 0x5a, 0x5b, 0x5c, 0x00, 0x00, 0x00, 0x80, 0x5d, 0x5e, 0x5f,
            0x60, 0x61, 0x62, 0x63, 0x64, 0x65, 0x66, 0x67, 0x68, 0x69, 0x6a,
            0x6b, 0x6c, 0x6d, 0x6e, 0x6f, 0x70, 0x71, 0x72, 0x73, 0x74, 0x75,
            0x76, 0x77, 0x78, 0x79, 0x7a, 0x7b, 0x00, 0x00, 0x00, 0x80, 0x7c,
            0x7d, 0x7e, 0x7f,
        ];
        let orig: Vec<u8> = (0..0x80).collect();

        let mut r = Cursor::new(data.as_ref());
        let dec = decompress(&mut r, 1024).unwrap();
        assert_eq!(&orig, &dec);
    }

    #[test]
    fn continuous4x10() {
        let data = [
            0x47, 0x1e, 0x00, 0x00, 0x00, 0x28, 0x00, 0x00, 0x00, 0x00, 0x04,
            0x00, 0x80, 0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08,
            0x09, 0x00, 0x12, 0x1a, 0x06, 0x07, 0x08, 0x09,
        ];
        let orig: Vec<u8> = iter::repeat((0..10).into_iter())
            .take(4)
            .flat_map(|v| v)
            .collect();

        let mut r = Cursor::new(data.as_ref());
        let dec = decompress(&mut r, 1024).unwrap();
        assert_eq!(&orig, &dec);
    }

    #[test]
    fn string_decompress_lvl1() {
        let data = [
            0x47, 0x4c, 0x2, 0, 0, 0xf9, 0x3, 0, 0, 0, 0, 0x4, 0x84, 0x69,
            0x6e, 0x69, 0x74, 0x73, 0x65, 0x72, 0x76, 0x65, 0x72, 0x20, 0x76,
            0x69, 0x72, 0x74, 0x75, 0x61, 0x6c, 0x54, 0x25, 0x5f, 0x6e, 0x61,
            0x6d, 0x65, 0x3d, 0x53, 0x23, 0x50, 0x5c, 0x73, 0x64, 0x65, 0,
            0x20, 0, 0x80, 0x72, 0x5c, 0x73, 0x56, 0x65, 0x72, 0x70, 0x6c,
            0x61, 0x6e, 0x74, 0x65, 0x6e, 0x7d, 0xb, 0x77, 0x65, 0x6c, 0x63,
            0x6f, 0x6d, 0x65, 0x6d, 0x65, 0x73, 0x73, 0x61, 0x67, 0x65, 0x3d,
            0x54, 0x68, 0x50, 0x82, 0x1, 0xb0, 0x69, 0x73, 0x5c, 0x73, 0xe2,
            0x6a, 0x53, 0x61, 0xa6, 0x6d, 0x79, 0x61, 0xb4, 0x57, 0x6f, 0x72,
            0x6c, 0x64, 0x7d, 0xb, 0x61, 0xa6, 0x74, 0x66, 0x6f, 0x72, 0x6d,
            0x3d, 0x4c, 0x69, 0x6e, 0x75, 0x78, 0x7d, 0xb, 0x1, 0x25, 0x73, 0,
            0, 0, 0x80, 0x69, 0x6f, 0x6e, 0x3d, 0x33, 0x2e, 0x30, 0x2e, 0x31,
            0x33, 0x2e, 0x38, 0x5c, 0x73, 0x5b, 0x42, 0x75, 0x69, 0x6c, 0x64,
            0x3a, 0x5c, 0x73, 0x31, 0x35, 0x30, 0x30, 0x34, 0x35, 0x32, 0x38,
            0x8, 0, 0x2, 0x88, 0x31, 0x31, 0x5d, 0x7d, 0xb, 0x6d, 0x61, 0x78,
            0x63, 0x6c, 0x69, 0x65, 0x6e, 0x74, 0x73, 0x3d, 0x33, 0x32, 0x7d,
            0xb, 0x63, 0x72, 0x65, 0x61, 0x74, 0x65, 0x64, 0x3d, 0x30, 0x7e,
            0xb, 0x6f, 0x64, 0x65, 0, 0x92, 0x10, 0x9c, 0x63, 0x5f, 0x65, 0x6e,
            0x63, 0x72, 0x79, 0x70, 0x74, 0xf1, 0x98, 0x5f, 0x6d, 0x91, 0x23,
            0x3d, 0x31, 0x7d, 0xb, 0x68, 0x6f, 0x73, 0x74, 0xb6, 0x25, 0x4c,
            0xc3, 0xa9, 0x5c, 0x73, 0x58, 0x27, 0xb1, 0x66, 0x61, 0xa6, 0x6d,
            0x79, 0x7, 0x8, 0x18, 0xe0, 0x70, 0xb, 0x1a, 0x94, 0xba, 0x2e,
            0x75, 0x64, 0x65, 0x66, 0x61, 0x75, 0x6c, 0x74, 0x5f, 0x55, 0x25,
            0x67, 0x72, 0x6f, 0x75, 0x70, 0x3d, 0x38, 0x7d, 0xb, 0x26, 0x30,
            0x63, 0x68, 0x61, 0x6e, 0x6e, 0x65, 0x6c, 0x5f, 0, 0x49, 0x16,
            0xe2, 0x85, 0x82, 0x11, 0x81, 0x80, 0x62, 0x72, 0x88, 0x72, 0x5f,
            0x75, 0x72, 0x6c, 0x7d, 0xb, 0xe9, 0x85, 0x67, 0x66, 0x78, 0x80,
            0x27, 0x22, 0x69, 0x6e, 0x74, 0x21, 0x50, 0x61, 0x6c, 0x3d, 0x32,
            0x30, 0x30, 0x2e, 0x75, 0x70, 0x72, 0x69, 0x6f, 0x72, 0x69, 0x74,
            0, 0x40, 0, 0xb0, 0x79, 0x5f, 0x73, 0x70, 0x65, 0x61, 0x6b, 0x65,
            0x72, 0x5f, 0x64, 0x69, 0x6d, 0x6d, 0x92, 0xba, 0x69, 0x66, 0x69,
            0x63, 0x61, 0x74, 0x6f, 0x72, 0x3d, 0x2d, 0x31, 0x38, 0x2e, 0x31,
            0x33, 0x2e, 0x75, 0x69, 0x2, 0, 0x3f, 0x80, 0x64, 0xe0, 0x33, 0x15,
            0x62, 0x75, 0x74, 0x74, 0x6f, 0x6e, 0x5f, 0x74, 0x6f, 0x6f, 0x6c,
            0x74, 0x69, 0x70, 0x70, 0xb, 0x14, 0x24, 0x33, 0x20, 0x4b, 0x17,
            0x24, 0x33, 0x10, 0x1e, 0x16, 0x82, 0x7b, 0x5f, 0x70, 0x68, 0x6f,
            0x6e, 0x65, 0x74, 0x69, 0x63, 0x90, 0x1, 0x88, 0xc1, 0x3d, 0x6d,
            0x6f, 0x62, 0x7d, 0xb, 0x69, 0x63, 0x91, 0xb9, 0xf1, 0x7b, 0x32,
            0x35, 0x36, 0x38, 0x35, 0x35, 0x35, 0x32, 0x31, 0x33, 0x7e, 0xb,
            0x70, 0x3d, 0x30, 0xd1, 0x2c, 0x21, 0xd3, 0x2c, 0x5c, 0x73, 0x3a,
            0x3a, 0x7d, 0xb, 0x50, 0, 0x1f, 0x80, 0x61, 0x73, 0x6b, 0x5f, 0x1,
            0x84, 0x5f, 0x71, 0x4e, 0x76, 0x69, 0x6c, 0x65, 0x67, 0x65, 0x6b,
            0x65, 0x79, 0xef, 0x23, 0xe9, 0x85, 0xb3, 0x92, 0x2e, 0x75, 0x56,
            0xe7, 0x74, 0x65, 0x6d, 0x70, 0x5f, 0x64, 0x65, 0x6c, 0x65, 0x74,
            0x32, 0, 0x28, 0x80, 0x65, 0x92, 0x20, 0x61, 0x79, 0x91, 0x20, 0x3,
            0x63, 0x3d, 0x31, 0x30, 0x20, 0x61, 0x63, 0x6e, 0x3d, 0x49, 0x74,
            0x73, 0x4d, 0x65, 0x61, 0x71, 0x6c, 0xf1, 0x7b, 0x31, 0x20, 0x70,
            0x76, 0x3d, 0x36, 0x20, 0x6c, 0x74, 0x8, 0xc0, 0x40, 0x80, 0x3d,
            0x30, 0x20, 0x54, 0xaf, 0x5f, 0x74, 0x61, 0x6c, 0x6b, 0x5f, 0x70,
            0x6f, 0x77, 0x65, 0x12, 0xfa, 0x66, 0x5e, 0x6e, 0x65, 0x65, 0x64,
            0x65, 0x64, 0x85, 0x50, 0x71, 0x75, 0x65, 0x72, 0x79, 0x5f, 0x76,
            0x69, 0, 0, 0, 0x80, 0x65, 0x77, 0x5f, 0x70, 0x6f, 0x77, 0x65,
            0x72, 0x3d, 0x37, 0x35,
        ];
        let orig = b"initserver virtualserver_name=Server\\sder\\sVerplanten virtualserver_welcomemessage=This\\sis\\sSplamys\\sWorld virtualserver_platform=Linux virtualserver_version=3.0.13.8\\s[Build:\\s1500452811] virtualserver_maxclients=32 virtualserver_created=0 virtualserver_codec_encryption_mode=1 virtualserver_hostmessage=L\xc3\xa9\\sServer\\sde\\sSplamy virtualserver_hostmessage_mode=0 virtualserver_default_server_group=8 virtualserver_default_channel_group=8 virtualserver_hostbanner_url virtualserver_hostbanner_gfx_url virtualserver_hostbanner_gfx_interval=2000 virtualserver_priority_speaker_dimm_modificator=-18.0000 virtualserver_id=1 virtualserver_hostbutton_tooltip virtualserver_hostbutton_url virtualserver_hostbutton_gfx_url virtualserver_name_phonetic=mob virtualserver_icon_id=2568555213 virtualserver_ip=0.0.0.0,\\s:: virtualserver_ask_for_privilegekey=0 virtualserver_hostbanner_mode=0 virtualserver_channel_temp_delete_delay_default=10 acn=ItsMe aclid=1 pv=6 lt=0 client_talk_power=-1 client_needed_serverquery_view_power=75";

        let mut r = Cursor::new(data.as_ref());
        let dec = decompress(&mut r, 1024).unwrap();
        assert_eq!(orig.as_ref(), dec.as_slice());
    }

    #[test]
    fn string_compress_lvl1() {
        let data = [
            0x47, 0x4c, 0x2, 0, 0, 0xf9, 0x3, 0, 0, 0, 0, 0x4, 0x84, 0x69,
            0x6e, 0x69, 0x74, 0x73, 0x65, 0x72, 0x76, 0x65, 0x72, 0x20, 0x76,
            0x69, 0x72, 0x74, 0x75, 0x61, 0x6c, 0x54, 0x25, 0x5f, 0x6e, 0x61,
            0x6d, 0x65, 0x3d, 0x53, 0x23, 0x50, 0x5c, 0x73, 0x64, 0x65, 0,
            0x20, 0, 0x80, 0x72, 0x5c, 0x73, 0x56, 0x65, 0x72, 0x70, 0x6c,
            0x61, 0x6e, 0x74, 0x65, 0x6e, 0x7d, 0xb, 0x77, 0x65, 0x6c, 0x63,
            0x6f, 0x6d, 0x65, 0x6d, 0x65, 0x73, 0x73, 0x61, 0x67, 0x65, 0x3d,
            0x54, 0x68, 0x50, 0x82, 0x1, 0xb0, 0x69, 0x73, 0x5c, 0x73, 0xe2,
            0x6a, 0x53, 0x61, 0xa6, 0x6d, 0x79, 0x61, 0xb4, 0x57, 0x6f, 0x72,
            0x6c, 0x64, 0x7d, 0xb, 0x61, 0xa6, 0x74, 0x66, 0x6f, 0x72, 0x6d,
            0x3d, 0x4c, 0x69, 0x6e, 0x75, 0x78, 0x7d, 0xb, 0x1, 0x25, 0x73, 0,
            0, 0, 0x80, 0x69, 0x6f, 0x6e, 0x3d, 0x33, 0x2e, 0x30, 0x2e, 0x31,
            0x33, 0x2e, 0x38, 0x5c, 0x73, 0x5b, 0x42, 0x75, 0x69, 0x6c, 0x64,
            0x3a, 0x5c, 0x73, 0x31, 0x35, 0x30, 0x30, 0x34, 0x35, 0x32, 0x38,
            0x8, 0, 0x2, 0x88, 0x31, 0x31, 0x5d, 0x7d, 0xb, 0x6d, 0x61, 0x78,
            0x63, 0x6c, 0x69, 0x65, 0x6e, 0x74, 0x73, 0x3d, 0x33, 0x32, 0x7d,
            0xb, 0x63, 0x72, 0x65, 0x61, 0x74, 0x65, 0x64, 0x3d, 0x30, 0x7e,
            0xb, 0x6f, 0x64, 0x65, 0, 0x92, 0x10, 0x9c, 0x63, 0x5f, 0x65, 0x6e,
            0x63, 0x72, 0x79, 0x70, 0x74, 0xf1, 0x98, 0x5f, 0x6d, 0x91, 0x23,
            0x3d, 0x31, 0x7d, 0xb, 0x68, 0x6f, 0x73, 0x74, 0xb6, 0x25, 0x4c,
            0xc3, 0xa9, 0x5c, 0x73, 0x58, 0x27, 0xb1, 0x66, 0x61, 0xa6, 0x6d,
            0x79, 0x7, 0x8, 0x18, 0xe0, 0x70, 0xb, 0x1a, 0x94, 0xba, 0x2e,
            0x75, 0x64, 0x65, 0x66, 0x61, 0x75, 0x6c, 0x74, 0x5f, 0x55, 0x25,
            0x67, 0x72, 0x6f, 0x75, 0x70, 0x3d, 0x38, 0x7d, 0xb, 0x26, 0x30,
            0x63, 0x68, 0x61, 0x6e, 0x6e, 0x65, 0x6c, 0x5f, 0, 0x49, 0x16,
            0xe2, 0x85, 0x82, 0x11, 0x81, 0x80, 0x62, 0x72, 0x88, 0x72, 0x5f,
            0x75, 0x72, 0x6c, 0x7d, 0xb, 0xe9, 0x85, 0x67, 0x66, 0x78, 0x80,
            0x27, 0x22, 0x69, 0x6e, 0x74, 0x21, 0x50, 0x61, 0x6c, 0x3d, 0x32,
            0x30, 0x30, 0x2e, 0x75, 0x70, 0x72, 0x69, 0x6f, 0x72, 0x69, 0x74,
            0, 0x40, 0, 0xb0, 0x79, 0x5f, 0x73, 0x70, 0x65, 0x61, 0x6b, 0x65,
            0x72, 0x5f, 0x64, 0x69, 0x6d, 0x6d, 0x92, 0xba, 0x69, 0x66, 0x69,
            0x63, 0x61, 0x74, 0x6f, 0x72, 0x3d, 0x2d, 0x31, 0x38, 0x2e, 0x31,
            0x33, 0x2e, 0x75, 0x69, 0x2, 0, 0x3f, 0x80, 0x64, 0xe0, 0x33, 0x15,
            0x62, 0x75, 0x74, 0x74, 0x6f, 0x6e, 0x5f, 0x74, 0x6f, 0x6f, 0x6c,
            0x74, 0x69, 0x70, 0x70, 0xb, 0x14, 0x24, 0x33, 0x20, 0x4b, 0x17,
            0x24, 0x33, 0x10, 0x1e, 0x16, 0x82, 0x7b, 0x5f, 0x70, 0x68, 0x6f,
            0x6e, 0x65, 0x74, 0x69, 0x63, 0x90, 0x1, 0x88, 0xc1, 0x3d, 0x6d,
            0x6f, 0x62, 0x7d, 0xb, 0x69, 0x63, 0x91, 0xb9, 0xf1, 0x7b, 0x32,
            0x35, 0x36, 0x38, 0x35, 0x35, 0x35, 0x32, 0x31, 0x33, 0x7e, 0xb,
            0x70, 0x3d, 0x30, 0xd1, 0x2c, 0x21, 0xd3, 0x2c, 0x5c, 0x73, 0x3a,
            0x3a, 0x7d, 0xb, 0x50, 0, 0x1f, 0x80, 0x61, 0x73, 0x6b, 0x5f, 0x1,
            0x84, 0x5f, 0x71, 0x4e, 0x76, 0x69, 0x6c, 0x65, 0x67, 0x65, 0x6b,
            0x65, 0x79, 0xef, 0x23, 0xe9, 0x85, 0xb3, 0x92, 0x2e, 0x75, 0x56,
            0xe7, 0x74, 0x65, 0x6d, 0x70, 0x5f, 0x64, 0x65, 0x6c, 0x65, 0x74,
            0x32, 0, 0x28, 0x80, 0x65, 0x92, 0x20, 0x61, 0x79, 0x91, 0x20, 0x3,
            0x63, 0x3d, 0x31, 0x30, 0x20, 0x61, 0x63, 0x6e, 0x3d, 0x49, 0x74,
            0x73, 0x4d, 0x65, 0x61, 0x71, 0x6c, 0xf1, 0x7b, 0x31, 0x20, 0x70,
            0x76, 0x3d, 0x36, 0x20, 0x6c, 0x74, 0x8, 0xc0, 0x40, 0x80, 0x3d,
            0x30, 0x20, 0x54, 0xaf, 0x5f, 0x74, 0x61, 0x6c, 0x6b, 0x5f, 0x70,
            0x6f, 0x77, 0x65, 0x12, 0xfa, 0x66, 0x5e, 0x6e, 0x65, 0x65, 0x64,
            0x65, 0x64, 0x85, 0x50, 0x71, 0x75, 0x65, 0x72, 0x79, 0x5f, 0x76,
            0x69, 0, 0, 0, 0x80, 0x65, 0x77, 0x5f, 0x70, 0x6f, 0x77, 0x65,
            0x72, 0x3d, 0x37, 0x35,
        ];
        let orig = b"initserver virtualserver_name=Server\\sder\\sVerplanten virtualserver_welcomemessage=This\\sis\\sSplamys\\sWorld virtualserver_platform=Linux virtualserver_version=3.0.13.8\\s[Build:\\s1500452811] virtualserver_maxclients=32 virtualserver_created=0 virtualserver_codec_encryption_mode=1 virtualserver_hostmessage=L\xc3\xa9\\sServer\\sde\\sSplamy virtualserver_hostmessage_mode=0 virtualserver_default_server_group=8 virtualserver_default_channel_group=8 virtualserver_hostbanner_url virtualserver_hostbanner_gfx_url virtualserver_hostbanner_gfx_interval=2000 virtualserver_priority_speaker_dimm_modificator=-18.0000 virtualserver_id=1 virtualserver_hostbutton_tooltip virtualserver_hostbutton_url virtualserver_hostbutton_gfx_url virtualserver_name_phonetic=mob virtualserver_icon_id=2568555213 virtualserver_ip=0.0.0.0,\\s:: virtualserver_ask_for_privilegekey=0 virtualserver_hostbanner_mode=0 virtualserver_channel_temp_delete_delay_default=10 acn=ItsMe aclid=1 pv=6 lt=0 client_talk_power=-1 client_needed_serverquery_view_power=75";

        let input = orig.to_vec();
        let com = compress(&input, CompressionLevel::Lvl1);
        assert_eq!(data.as_ref(), com.as_slice());
    }

    #[test]
    fn corrupt_string() {
        let data = [
            0x47, 0x56, 0x02, 0x00, 0x00, 0xf9, 0x03, 0x00, 0x00, 0x00, 0x00,
            0x04, 0x84, 0x69, 0x6e, 0x69, 0x74, 0x73, 0x65, 0x72, 0x76, 0x65,
            0x72, 0x20, 0x76, 0x69, 0x72, 0x74, 0x75, 0x61, 0x6c, 0x54, 0x25,
            0x5f, 0x6e, 0x61, 0x6d, 0x65, 0x3d, 0x53, 0x23, 0x50, 0x5c, 0x73,
            0x64, 0x65, 0x00, 0x20, 0x00, 0x80, 0x72, 0x5c, 0x73, 0x56, 0x65,
            0x72, 0x70, 0x6c, 0x61, 0x6e, 0x74, 0x65, 0x6e, 0x7d, 0x0b, 0x77,
            0x65, 0x6c, 0x63, 0x6f, 0x6d, 0x65, 0x6d, 0x65, 0x73, 0x73, 0x61,
            0x67, 0x65, 0x3d, 0x54, 0x68, 0x50, 0x82, 0x01, 0xb0, 0x69, 0x73,
            0x5c, 0x73, 0xe2, 0x6a, 0x53, 0x61, 0xa6, 0x6d, 0x79, 0x61, 0xb4,
            0x57, 0x6f, 0x72, 0x6c, 0x64, 0x7d, 0x0b, 0x61, 0xa6, 0x74, 0x66,
            0x6f, 0x72, 0x6d, 0x3d, 0x4c, 0x69, 0x6e, 0x75, 0x78, 0x7d, 0x0b,
            0x01, 0x25, 0x73, 0x00, 0x00, 0x00, 0x80, 0x69, 0x6f, 0x6e, 0x3d,
            0x33, 0x2e, 0x30, 0x2e, 0x31, 0x33, 0x2e, 0x38, 0x5c, 0x73, 0x5b,
            0x42, 0x75, 0x69, 0x6c, 0x64, 0x3a, 0x5c, 0x73, 0x31, 0x35, 0x30,
            0x30, 0x34, 0x35, 0x32, 0x38, 0x08, 0x00, 0x02, 0x88, 0x31, 0x31,
            0x5d, 0x7d, 0x0b, 0x6d, 0x61, 0x78, 0x63, 0x6c, 0x69, 0x65, 0x6e,
            0x74, 0x73, 0x3d, 0x33, 0x32, 0x7d, 0x0b, 0x63, 0x72, 0x65, 0x61,
            0x74, 0x65, 0x64, 0x3d, 0x30, 0x7d, 0x0b, 0x6e, 0x6f, 0x64, 0x00,
            0x24, 0x21, 0xb8, 0x65, 0x63, 0x5f, 0x65, 0x6e, 0x63, 0x72, 0x79,
            0x70, 0x74, 0xf1, 0x98, 0x5f, 0x6d, 0x91, 0x23, 0x3d, 0x31, 0x7d,
            0x0b, 0x68, 0x6f, 0x73, 0x74, 0xb6, 0x25, 0x4c, 0xc3, 0xa9, 0x5c,
            0x73, 0x58, 0x27, 0xb1, 0x66, 0x61, 0xa6, 0x6d, 0x1e, 0x20, 0xc0,
            0x80, 0x79, 0x7d, 0x0b, 0x89, 0x7b, 0x94, 0xba, 0x2e, 0x75, 0x64,
            0x65, 0x66, 0x61, 0x75, 0x6c, 0x74, 0x5f, 0x54, 0x25, 0x20, 0x67,
            0x72, 0x6f, 0x75, 0x70, 0x3d, 0x38, 0x7d, 0x0b, 0x26, 0x30, 0x63,
            0x68, 0x61, 0x6e, 0x6e, 0x65, 0x6c, 0x16, 0x1c, 0x11, 0x88, 0x5f,
            0x00, 0x49, 0x16, 0xe2, 0x85, 0x62, 0x72, 0x88, 0x72, 0x5f, 0x75,
            0x72, 0x6c, 0x7d, 0x0b, 0xe2, 0x85, 0xb5, 0x25, 0x67, 0x66, 0x78,
            0x80, 0x27, 0x22, 0x69, 0x6e, 0x74, 0x21, 0x50, 0x61, 0x6c, 0x3d,
            0x32, 0x30, 0x30, 0x2e, 0x75, 0x70, 0x72, 0x69, 0x00, 0x00, 0x04,
            0x80, 0x6f, 0x72, 0x69, 0x74, 0x79, 0x5f, 0x73, 0x70, 0x65, 0x61,
            0x6b, 0x65, 0x72, 0x5f, 0x64, 0x69, 0x6d, 0x6d, 0x92, 0xba, 0x69,
            0x66, 0x69, 0x63, 0x61, 0x74, 0x6f, 0x72, 0x3d, 0x2d, 0x31, 0x38,
            0x26, 0x00, 0xf0, 0x87, 0x2e, 0x31, 0x33, 0x2e, 0x75, 0x69, 0x64,
            0xe0, 0x33, 0x15, 0x62, 0x75, 0x74, 0x74, 0x6f, 0x6e, 0x5f, 0x74,
            0x6f, 0x6f, 0x6c, 0x74, 0x69, 0x70, 0x7d, 0x0b, 0x83, 0x7b, 0x24,
            0x33, 0x20, 0x4b, 0x17, 0x24, 0x33, 0x10, 0x1e, 0x16, 0x82, 0x7b,
            0x5f, 0x70, 0x68, 0x6f, 0x00, 0x32, 0x00, 0xe1, 0x6e, 0x65, 0x74,
            0x69, 0x63, 0x3d, 0x6d, 0x6f, 0x62, 0x7d, 0x0b, 0x69, 0x63, 0x91,
            0xb9, 0xf1, 0x7b, 0x32, 0x35, 0x36, 0x38, 0x35, 0x35, 0x35, 0x32,
            0x31, 0x33, 0x7d, 0x0b, 0x6e, 0x70, 0x3d, 0x30, 0xd1, 0x2c, 0x21,
            0xd3, 0x20, 0x14, 0xc0, 0x87, 0x2c, 0x5c, 0x73, 0x3a, 0x3a, 0x7d,
            0x0b, 0x61, 0x73, 0x6b, 0x5f, 0x01, 0x84, 0x5f, 0x71, 0x4e, 0x76,
            0x69, 0x6c, 0x65, 0x67, 0x65, 0x6b, 0x65, 0x79, 0xef, 0x23, 0xe9,
            0x85, 0xb3, 0x92, 0x2e, 0x75, 0x56, 0xe7, 0x74, 0x65, 0x6d, 0x70,
            0x80, 0x0c, 0x00, 0x8a, 0x5f, 0x64, 0x65, 0x6c, 0x65, 0x74, 0x65,
            0x92, 0x20, 0x61, 0x79, 0x91, 0x20, 0x03, 0x63, 0x3d, 0x31, 0x30,
            0x20, 0x61, 0x63, 0x6e, 0x3d, 0x49, 0x74, 0x73, 0x4d, 0x65, 0x61,
            0x71, 0x6c, 0xf1, 0x7b, 0x31, 0x20, 0x70, 0x00, 0x02, 0x80, 0x81,
            0x76, 0x3d, 0x36, 0x20, 0x6c, 0x74, 0x3d, 0x30, 0x20, 0x51, 0xaf,
            0x64, 0x3d, 0x31, 0x5f, 0x74, 0x61, 0x6c, 0x6b, 0x5f, 0x70, 0x6f,
            0x77, 0x65, 0x12, 0xfa, 0x66, 0x5e, 0x6e, 0x65, 0x65, 0x64, 0x65,
            0x64, 0x01, 0x00, 0x00, 0x80, 0x85, 0x50, 0x71, 0x75, 0x65, 0x72,
            0x79, 0x5f, 0x76, 0x69, 0x65, 0x77, 0x5f, 0x70, 0x6f, 0x77, 0x65,
            0x72, 0x3d, 0x37, 0x35,
        ];
        let orig = b"initserver virtualserver_name=Server\\sder\\sVerplanten virtualserver_welcomemessage=This\\sis\\sSplamys\\sWorld virtualserver_platform=Linux virtualserver_version=3.0.13.8\\s[Build:\\s1500452811] virtualserver_maxclients=32 virtualserver_created=0 virtualserver_nodec_encryption_mode=1 virtualserver_hostmessage=L\xc3\xa9\\sServer\\sde\\sSplamy virtualserver_name=Server_mode=0 virtualserver_default_server group=8 virtualserver_default_channel_group=8 virtualserver_hostbanner_url virtualserver_hostmessagegfx_url virtualserver_hostmessagegfx_interval=2000 virtualserver_priority_speaker_dimm_modificator=-18.0000 virtualserver_id=1 virtualserver_hostbutton_tooltip virtualserver_name=utton_url virtualserver_hostmutton_gfx_url virtualserver_name_phonetic=mob virtualserver_icon_id=2568555213 virtualserver_np=0.0.0.0,\\s:: virtualserver_ask_for_privilegekey=0 virtualserver_hostmessagemode=0 virtualserver_channel_temp_delete_delay_default=10 acn=ItsMe aclid=1 pv=6 lt=0 clid=1_talk_power=-1 clid=1_needed_serverquery_view_power=75";

        let mut r = Cursor::new(data.as_ref());
        let dec = decompress(&mut r, 1024).unwrap();
        assert_eq!(orig.as_ref(), dec.as_slice());
    }

    #[test]
    fn corrupt_enterview() {
        let data = [
            0x47, 0xf2, 0x1, 0, 0, 0xfe, 0x2, 0, 0, 0, 0x10, 0, 0x90, 0x6e,
            0x6f, 0x74, 0x69, 0x66, 0x79, 0x63, 0x6c, 0x69, 0x65, 0x6e, 0x74,
            0x32, 0x92, 0x72, 0x76, 0x69, 0x65, 0x77, 0x20, 0x63, 0x66, 0x69,
            0x64, 0x3d, 0x30, 0x20, 0x63, 0x74, 0xf1, 0x7b, 0x31, 0x20, 0x40,
            0x62, 0, 0x82, 0x72, 0x65, 0x61, 0x73, 0x6f, 0x6e, 0xf1, 0x7b,
            0x32, 0x20, 0x51, 0xaf, 0x64, 0x3d, 0x31, 0x62, 0x5e, 0x31, 0x92,
            0x5f, 0x75, 0x6e, 0x69, 0x71, 0x75, 0x65, 0x5f, 0x69, 0x64, 0x31,
            0x92, 0x69, 0x66, 0x69, 0x65, 0x72, 0, 0, 0, 0xa0, 0x3d, 0x6c,
            0x6b, 0x73, 0x37, 0x51, 0x4c, 0x35, 0x4f, 0x56, 0x4d, 0x4b, 0x6f,
            0x34, 0x70, 0x5a, 0x37, 0x39, 0x63, 0x45, 0x4f, 0x49, 0x35, 0x72,
            0x35, 0x6f, 0x45, 0x41, 0x3d, 0x66, 0x5e, 0x6e, 0, 0x8, 0xc0, 0x90,
            0x69, 0x63, 0x6b, 0x6e, 0x61, 0x6d, 0x65, 0x3d, 0x42, 0x6f, 0x74,
            0x66, 0x5e, 0x69, 0x6e, 0x70, 0x75, 0x74, 0x5f, 0x6d, 0x75, 0x74,
            0x65, 0x73, 0xe6, 0xa3, 0xf3, 0x5f, 0x6f, 0x75, 0x74, 0x70, 0x23,
            0x19, 0x6f, 0x6e, 0xc, 0x70, 0, 0x81, 0x6c, 0x79, 0x9e, 0xa0, 0xf4,
            0x96, 0x68, 0x61, 0x72, 0x64, 0x77, 0x61, 0x72, 0x65, 0xe2, 0x23,
            0xab, 0xf3, 0xe0, 0x64, 0x12, 0x6d, 0x65, 0x74, 0x61, 0x5f, 0x64,
            0x61, 0x74, 0x61, 0x67, 0x5e, 0x73, 0x5f, 0x72, 0x65, 0x63, 0x6f,
            0x60, 0x84, 0, 0xa0, 0x72, 0x64, 0x69, 0x6e, 0x67, 0xe8, 0x23,
            0x22, 0x62, 0x62, 0x61, 0x73, 0x2, 0x9f, 0x3d, 0x32, 0x33, 0x39,
            0x66, 0x5e, 0x63, 0x68, 0x61, 0x6e, 0x6e, 0x65, 0x6c, 0x5f, 0x67,
            0x72, 0x6f, 0x75, 0x70, 0x91, 0xf1, 0x3d, 0x2, 0x11, 0x6, 0x88,
            0x38, 0x66, 0x5e, 0x73, 0x65, 0x72, 0x76, 0x65, 0x72, 0x3, 0x49,
            0x73, 0x3d, 0x37, 0x66, 0x5e, 0x61, 0x77, 0x61, 0x79, 0xe8, 0x23,
            0x62, 0x17, 0x5f, 0x6d, 0x65, 0x73, 0x73, 0x61, 0x67, 0x65, 0x66,
            0x5e, 0x74, 0x79, 0x70, 0x1, 0x5, 0xb4, 0xe4, 0x69, 0xe6, 0x66,
            0x6c, 0x61, 0x67, 0x5f, 0x61, 0x76, 0x61, 0x27, 0x72, 0x67, 0x5e,
            0x61, 0x6c, 0x6b, 0x5f, 0x70, 0x6f, 0x77, 0x21, 0x1b, 0x35, 0x21,
            0x60, 0xa4, 0xf3, 0x74, 0x72, 0xad, 0x72, 0x65, 0x61, 0x32, 0x73,
            0x74, 0xe8, 0x23, 0x2a, 0x7b, 0x10, 0, 0xf1, 0x80, 0x5f, 0x6d,
            0x73, 0x67, 0x66, 0x5e, 0x64, 0x65, 0x73, 0x63, 0x72, 0x69, 0x70,
            0x74, 0x69, 0x6f, 0x6e, 0x66, 0x5e, 0x69, 0x73, 0x5f, 0x22, 0x7b,
            0x21, 0x1b, 0x27, 0x60, 0xe1, 0x69, 0x70, 0x72, 0x69, 0x6f, 0x72,
            0x69, 0x74, 0x80, 0xd4, 0, 0x82, 0x79, 0x5f, 0x73, 0x70, 0x65,
            0x61, 0x6b, 0x2a, 0x1b, 0x75, 0x6e, 0x41, 0x36, 0x64, 0x96, 0xb0,
            0x73, 0xe8, 0x23, 0x86, 0xf5, 0x5f, 0x70, 0x68, 0x6f, 0x6e, 0x65,
            0x74, 0x69, 0x63, 0x66, 0x5e, 0x6e, 0x65, 0x65, 0x64, 0x65, 0xc,
            0x13, 0xe, 0x88, 0x64, 0x5f, 0x54, 0x25, 0x61, 0x32, 0x72, 0x79,
            0x5f, 0x76, 0xf1, 0x21, 0x85, 0x6a, 0x37, 0x35, 0x66, 0x5e, 0x69,
            0x63, 0x6f, 0x6e, 0x92, 0xf1, 0x2a, 0x60, 0x56, 0xe7, 0x63, 0x6f,
            0x6d, 0x6d, 0x61, 0x6e, 0x64, 0x2a, 0x1b, 0x63, 0x6f, 0x75, 0x70,
            0xf0, 0, 0x80, 0x6e, 0x74, 0x72, 0x79, 0x66, 0x5e, 0x56, 0xe7, 0x3,
            0x49, 0x5f, 0x69, 0x6e, 0x68, 0x65, 0x41, 0xe3, 0x31, 0x19, 0x56,
            0xe7, 0xf1, 0x7b, 0x31, 0x20, 0x63, 0x6c, 0x69, 0x65, 0x6e, 0x74,
            0x5f, 0x62, 0x61, 0x64, 0x67, 0x65, 0x73,
        ];
        let orig = b"notifycliententerview cfid=0 ctid=1 reasonid=2 clid=1 client_unique_identifier=lks7QL5OVMKo4pZ79cEOI5r5oEA= client_nickname=Bot client_input_muted=0 client_output_muted=0 client_outputonly_muted=0 client_input_hardware=0 client_output_hardware=0 client_meta_data client_is_recording=0 client_database_id=239 client_channel_group_id=8 client_servergroups=7 client_away=0 client_away_message client_type=0 client_flag_avatar client_talk_power=50 client_talk_request=0 client_talk_request_msg client_description client_is_talker=0 client_is_priority_speaker=0 client_unread_messages=0 client_nickname_phonetic client_needed_serverquery_view_power=75 client_icon_id=0 client_is_channel_commander=0 client_country client_channel_group_inherited_channel_id=1 client_badges";

        let mut r = Cursor::new(data.as_ref());
        let dec = decompress(&mut r, 1024).unwrap();
        assert_eq!(orig.as_ref(), dec.as_slice());
    }

    #[test]
    fn roundtrip_lvl1() {
        let orig = b"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaabbbbbbbbbbbbbbbbbbbbbbbbbbbbbbaaaaaaaaaaaaaaaaa";

        let comp = compress(orig, CompressionLevel::Lvl1);
        let mut r = Cursor::new(comp.as_slice());
        let dec = decompress(&mut r, 1024).unwrap();

        assert_eq!(orig.as_ref(), dec.as_slice());
    }

    #[test]
    fn roundtrip_lvl3() {
        let orig = b"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaabbbbbbbbbbbbbbbbbbbbbbbbbbbbbbaaaaaaaaaaaaaaaaa";

        let comp = compress(orig, CompressionLevel::Lvl3);
        let mut r = Cursor::new(comp.as_slice());
        let dec = decompress(&mut r, 1024).unwrap();

        assert_eq!(orig.as_ref(), dec.as_slice());
    }

    #[test]
    fn data_compress_lvl3() {
        let data_s = [
            77, 26, 106, 136, 1, 0, 128, 97, 97, 97, 131, 154, 1, 0, 98, 98,
            98, 231, 1, 0, 234, 10, 97, 97, 97, 97,
        ];
        let data_l = [
            79, 32, 0, 0, 0, 106, 0, 0, 0, 136, 1, 0, 128, 97, 97, 97, 131,
            154, 1, 0, 98, 98, 98, 231, 1, 0, 234, 10, 97, 97, 97, 97,
        ];
        let orig = b"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaabbbbbbbbbbbbbbbbbbbbbbbbbbbbbbaaaaaaaaaaaaaaaaa";

        let input = orig.to_vec();
        let com = compress(&input, CompressionLevel::Lvl3);
        let comsl = com.as_slice();
        if comsl[0] & 2 != 0 {
            // long
            assert_eq!(data_l.as_ref(), com.as_slice());
        } else {
            // short
            assert_eq!(data_s.as_ref(), com.as_slice());
        }
    }
}

#[cfg(feature = "nightly")]
mod bench {
    use super::*;
    use test::Bencher;
    use std::fs::File;
    use super::*;

    #[bench]
    fn perf_compress_lvl1(b: &mut Bencher) {
        let mut f = File::open("bench/bench.txt").expect("file not found");

        let mut contents = vec![];
        f.read_to_end(&mut contents)
            .expect("something went wrong reading the file");

        b.iter(|| {
            compress(contents.as_slice(), CompressionLevel::Lvl1);
        });
    }

    #[bench]
    fn perf_compress_lvl3(b: &mut Bencher) {
        let mut f = File::open("bench/bench.txt").expect("file not found");

        let mut contents = vec![];
        f.read_to_end(&mut contents)
            .expect("something went wrong reading the file");

        b.iter(|| {
            compress(contents.as_slice(), CompressionLevel::Lvl3);
        });
    }
}
