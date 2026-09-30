pub mod pdf;
pub mod png;
pub mod svg;
pub mod text;

pub fn image_mime(bytes: &[u8]) -> Option<&'static str> {
    if bytes.len() >= 3 && bytes[..3] == [0xff, 0xd8, 0xff] {
        Some("image/jpeg")
    } else if bytes.len() >= 8 && bytes[..8] == [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a] {
        Some("image/png")
    } else {
        None
    }
}
