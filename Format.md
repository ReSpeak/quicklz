# QuickLZ compression algorithm
Version 1.5.0

- Numbers are stored in **little** endian
- Streaming mode is not covered
- Single bytes are marked by `╥` in the diagrams, bits by `┬`
- The end of ranges as array indices are exclusive. That means `a[1..3]` is
indexed by `[1;3)` and will return an array of size 2: `a[1], a[2]`

# Data Format
Flags:

- 0x01: Compressed if set
- 0x02: Header length 9 if set, else 3
- 0x0c: Compression level (2 bits)
- 0x10: Streaming buffer = 100 000
- 0x20: Streaming buffer = 1 000 000
- 0x30: Streaming buffer > 1 000 000
- 0x40: Always set

The compressed size includes the header size.

## Short Header
Used if the data size is less than 216.

```
┌─────╥───────────╥───────────┐
│Flags║ Comp size ║ Dec sizc  │
└─────╨───────────╨───────────┘
```

## Long Header
```
┌─────╥──╥──╥──╥──╥──╥──╥──╥──┐
│Flags║ Comp size ║ Dec size  │
└─────╨──╨──╨──╨──╨──╨──╨──╨──┘
```

## Compression
The source size has to fullfill 0 &lt; size &lt; (u32::MAX - 400),
the destination buffer should be `source.length + 400` bytes long.

### Compression Level 1

- Initialize `control: u32 = 1 << 31`
- Initialize `control_pos: usize = header.length`
- Initialize `dest_pos = header.length + 4`
- Initialize `lits: u32 = 0`
- `hashtable: [u32; 4096]`
- `hash_counter: [bool; 4096]` (could be a bit vector)
- `cachetable: [u32; 4096]`
- While `source_pos < source.length - 10`
	- If the least significant bit of `control` is set
		- Check if the compression is too inefficient: `source_pos > 3 *
		(source.length / 4) && dest_pos > source_pos - (source_pos / 32)`
			- Return source data with the uncompressed flag set in the header
		- Write the control word: `dest[control_pos .. +4] = (control >> 1) | (1 << 31)`
		- Reserve next position: `control_pos = dest_pos; dest_pos += 4`
		- Reset control word to `1 << 31`
	- Read next 3 byte: `next = source[source_pos .. +3]`
	- Compute hash and get hash- and cachetable values
	```
	hash = hash(next)
	offset = hashtable[hash]
	cache = cachetable[hash]
	counter = hash_counter[hash]
	cachetable[hash] = next
	hashtable[hash] = source_pos
	```
	- If `cache == next && counter && (source_pos - offset >= 3 || source_pos == offset + 1 && lits >= 3 && source_pos > 3 && is_eq(source[source_pos - 3 .. source_pos + 3]))`
		- We found a match, so write a reference
		- `control = (control >> 1) | (1 << 31)`
		- Compute match length: `matchlen = 3`
		- `remainder = minimum(source.length - 4 - source_pos, 0xff)`
		- While `source[offset + matchlen] == source[source_pos + matchlen] && matchlen < remainder`
			- `matchlen++`
		- If `matchlen < 18`
			- Write short reference (see Decompression Level 1)
		- Else
			- Write long reference
		- Skip match: `source_pos += matchlen`
		- `lits = 0`
	- Else, no match was found
		```
		lits++
		hash_counter[hash] = true
		```
		- Read one byte from the source buffer and write it into the destination buffer
		- `control >>= 1`
- Copy the rest: While `source_pos < source.length`
	- If the least significant bit of `control` is set
		- Write the control word: `dest[control_pos .. +4] = (control >> 1) | (1 << 31)`
		- Reserve next position: `control_pos = dest_pos; dest_pos += 4`
		- Reset control word to `1 << 31`
	- Read one byte from the source buffer and write it into the destination buffer
	- `control >>= 1`
- Shift `control >>= 1` until the least significant bit is 1
- Write the control word: `dest[control_pos .. +4] = (control >> 1) | (1 << 31)`
- Write the header

### Compression Level 3

- Initialize `control: u32 = 1 << 31`
- Initialize `control_pos: usize = header.length`
- Initialize `dest_pos = header.length + 4`
- `hashtable: [[u32; 4096]; 16]` (or less than 16)
- `hash_counter: [u8; 4096]`
- While `source_pos < source.length - 10`
	- If the least significant bit of `control` is set
		- Check if the compression is too inefficient: `source_pos > 3 *
		(source.length / 4) && dest_pos > source_pos - (source_pos / 32)`
			- Return source data with the uncompressed flag set in the header
		- Write the control word: `dest[control_pos .. +4] = (control >> 1) | (1 << 31)`
		- Reserve next position: `control_pos = dest_pos; dest_pos += 4`
		- Reset control word to `1 << 31`
	- Read next 3 byte: `next = source[source_pos .. +3]`
	- `remainder = minimum(source.length - 4 - source_pos, 0xff)`
	- `counter = hash_counter[hash(next)]`
	- `matchlen = 0; offset = 0; best = 0`
	- For each `index, hasht` in `hashtable` && `index < counter`
		- `o = hasht[hash]`
		- If `source[o .. +3] == next && o < source_pos - 2`
			- Compute match length: `m = 3`
			- While `source[o + m] == source[source_pos + m] && m < remainder`
				- `m++`
			- Pick the best match: If `m > matchlen || (m == matchlen && o > offset)`
				```
				offset = o
				matchlen = m
				best = index
				```
	- `hashtable[counter % hashtable.length][hash] = source_pos`
	- `hash_counter[hash] = counter + 1`
	- If a match was found: `matchlen >= 3 && source_pos - offset < 0x1ffff`
		- Compute relative offset: `offset = source_pos - offset`
		- Update hash table: For `u = 1; u < matchlen; u++`
			- `next = source[source_pos + u .. +3]`
			```
			hash = hash(next)
			counter = hash_counter[hash]
			hash_counter[hash]++
			hashtable[counter % hashtable.length][hash] = source_pos + u
			```
		- Skip match: `source_pos += matchlen`
		- `control = (control >> 1) | (1 << 31)`
		- Write a reference with the matching possibility (see Decompress Level 3)
	- Else, no match was found
		- Read one byte from the source buffer and write it into the destination buffer
		- `control >>= 1`
- Copy the rest: While `source_pos < source.length`
	- If the least significant bit of `control` is set
		- Write the control word: `dest[control_pos .. +4] = (control >> 1) | (1 << 31)`
		- Reserve next position: `control_pos = dest_pos; dest_pos += 4`
		- Reset control word to `1 << 31`
	- Read one byte from the source buffer and write it into the destination buffer
	- `control >>= 1`
- Shift `control >>= 1` until the least significant bit is 1
- Write the control word: `dest[control_pos .. +4] = (control >> 1) | (1 << 31)`
- Write the header

## Decompression
If the compressed flag is not set, just return the input without header.

### Decompress Level 1

- Initialize `control: u32 = 1`
- Initialize `next_hashed = 0`
- `hashtable: [u32; 4096]`
- If `control == 1`
	- Read `control`
- If the least significant bit of `control` is set, a reference to previous data is following
	- `control >>= 1`
	- Read next 1-3 bytes, according to the following schema:
	```
	matchlen = matchle + 2
	hash = hash1 | (hash2 << 4)
	┌─┬─┬─┬─┬─┬─┬─┬─╥─┬─┬─┬─┬─┬─┬─┬─┐
	│  hash1│matchle║     hash2     │
	└─┴─┴─┴─┴─┴─┴─┴─╨─┴─┴─┴─┴─┴─┴─┴─┘

	hash = hash1 | (hash2 << 4)
	┌─┬─┬─┬─┬─┬─┬─┬─╥─┬─┬─┬─┬─┬─┬─┬─╥─┬─┬─┬─┬─┬─┬─┬─┐
	│  hash1│0 0 0 0║     hash2     ║    matchlen   │
	└─┴─┴─┴─┴─┴─┴─┴─╨─┴─┴─┴─┴─┴─┴─┴─╨─┴─┴─┴─┴─┴─┴─┴─┘
	```
	- `offset = hashtable[hash]`
	- The offset is an absolute position in the **destination** buffer, `matchlen` bytes should be copied from there.
	- `update_hashtable(start = next_hashed, end = dest.length() + 1 - matchlen)`
	- `next_hashed = dest.length()`
- Else if the least significant bit of `control` is not set and we are writing the last 10 bytes (`dest_pos >= dest.length - 10`)
	- If `control == 1`
		- Skip 4 bytes from the source
	- `control >>= 1`
	- Read one byte from the source buffer and write it into the destination buffer
	- Repeat until the end of the buffer is reached, then return the result
- Else, we write one byte
	- Read one byte from the source buffer and write it into the destination buffer
	- `control >>= 1`
	- `end = dest.length() - 2`
	- `update_hashtable(start = next_hashed, end)`
	- `next_hashed = max(next_hashed, end)`
- Repeat, starting at the first if

`hash(value) = ((value >> 12) ^ value) & 0xfff`

`update_hashtable(start, end)`:
- `for i in [start;end)`
	- `value = dest[i .. i + 3]`
	- `hashtable[hash(value)] = i`

### Decompress Level 3

- Initialize `control: u32 = 1`
- If `control == 1`
	- Read `control`
- If the least significant bit of `control` is set, a reference to previous data is following
	- `control >>= 1`
	- Read next 1-4 bytes, according to the following schema:

```
matchlen = 3
┌─┬─┬─┬─┬─┬─┬─┬─┐
│    offset │0 0│
└─┴─┴─┴─┴─┴─┴─┴─┘

matchlen = 3
offset = offset1 | (offset2 << 6)
┌─┬─┬─┬─┬─┬─┬─┬─╥─┬─┬─┬─┬─┬─┬─┬─┐
│   offset1 │0 1║    offset2    │
└─┴─┴─┴─┴─┴─┴─┴─╨─┴─┴─┴─┴─┴─┴─┴─┘

matchlen = 3 + matchle
offset = off | (offset2 << 2)
┌─┬─┬─┬─┬─┬─┬─┬─╥─┬─┬─┬─┬─┬─┬─┬─┐
│off│matchle│1 0║    offset2    │
└─┴─┴─┴─┴─┴─┴─┴─╨─┴─┴─┴─┴─┴─┴─┴─┘

matchlen = 2 + matchle
offset = o | (offset2 << 1) | (offset3 << 9)
┌─┬─┬─┬─┬─┬─┬─┬─╥─┬─┬─┬─┬─┬─┬─┬─╥─┬─┬─┬─┬─┬─┬─┬─┐
│o│ matchle  1 1║    offset2    ║    offset3    │
└─┴─┴─┴─┴─┴─┴─┴─╨─┴─┴─┴─┴─┴─┴─┴─╨─┴─┴─┴─┴─┴─┴─┴─┘

matchlen = 3 + (m | (matchlen1 << 1))
offset = o | (offset2 << 1) | (offset3 << 9)
┌─┬─┬─┬─┬─┬─┬─┬─╥─┬─┬─┬─┬─┬─┬─┬─╥─┬─┬─┬─┬─┬─┬─┬─╥─┬─┬─┬─┬─┬─┬─┬─┐
│m│0 0 0 0 0 1 1║o│ matchlen1   ║    offset2    ║    offset3    │
└─┴─┴─┴─┴─┴─┴─┴─╨─┴─┴─┴─┴─┴─┴─┴─╨─┴─┴─┴─┴─┴─┴─┴─╨─┴─┴─┴─┴─┴─┴─┴─┘
```

The offset is relative to the current position in the **destination** buffer, `matchlen` bytes should be copied from there.

- Else if the least significant bit of `control` is not set and we are writing the last 10 bytes (`dest_pos >= dest.length - 10`)
	- If `control == 1`
		- Skip 4 bytes from the source
	- `control >>= 1`
	- Read one byte from the source buffer and write it into the destination buffer
	- Repeat until the end of the buffer is reached, then return the result
- Else, we write one byte
	- Read one byte from the source buffer and write it into the destination buffer
	- `control >>= 1`
- Repeat, starting at the first if
