//! Protobuf wire-format encoder (mirror of `pb.rs` decoding).
//!
//! GoodNotes page records, index files and events are protobuf messages
//! framed as a record stream (`varint length ‖ message`). This module
//! provides the minimal encoder needed to write them back.

/// Append a base-128 varint.
pub fn write_varint(out: &mut Vec<u8>, mut v: u64) {
    loop {
        let b = (v & 0x7f) as u8;
        v >>= 7;
        if v == 0 {
            out.push(b);
            return;
        }
        out.push(b | 0x80);
    }
}

/// Append a key tag: `(field_number << 3) | wire_type`.
fn write_tag(out: &mut Vec<u8>, num: u32, wire: u8) {
    write_varint(out, ((num as u64) << 3) | wire as u64);
}

/// Append a length-delimited field.
pub fn write_bytes_field(out: &mut Vec<u8>, num: u32, data: &[u8]) {
    write_tag(out, num, 2);
    write_varint(out, data.len() as u64);
    out.extend_from_slice(data);
}

/// Append a varint field.
pub fn write_varint_field(out: &mut Vec<u8>, num: u32, v: u64) {
    write_tag(out, num, 0);
    write_varint(out, v);
}

/// Append a fixed32 field (little-endian).
pub fn write_fixed32_field(out: &mut Vec<u8>, num: u32, bits: u32) {
    write_tag(out, num, 5);
    out.extend_from_slice(&bits.to_le_bytes());
}

/// Append a fixed64 field (little-endian).
pub fn write_fixed64_field(out: &mut Vec<u8>, num: u32, bits: u64) {
    write_tag(out, num, 1);
    out.extend_from_slice(&bits.to_le_bytes());
}

/// Encode an `f32` as a fixed32 field (the universal number encoding in
/// GoodNotes colour / frame / geometry messages).
pub fn write_f32_field(out: &mut Vec<u8>, num: u32, v: f32) {
    write_fixed32_field(out, num, v.to_bits());
}

/// Append a length-prefixed record to a record stream.
pub fn write_record(out: &mut Vec<u8>, msg: &[u8]) {
    write_varint(out, msg.len() as u64);
    out.extend_from_slice(msg);
}

/// Build a protobuf sub-message from a closure and wrap it as a field.
pub fn sub_msg(num: u32, build: impl FnOnce(&mut Vec<u8>)) -> impl FnOnce(&mut Vec<u8>) {
    move |out: &mut Vec<u8>| {
        let mut inner = Vec::new();
        build(&mut inner);
        write_bytes_field(out, num, &inner);
    }
}

/// Encode an f32 as its IEEE-754 bit pattern (for colour channels etc.).
pub fn f32_bits(v: f32) -> u32 {
    v.to_bits()
}
