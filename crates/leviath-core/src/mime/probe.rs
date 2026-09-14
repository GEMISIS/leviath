//! Header probes: dimensions and durations read from the first bytes of a file.
//!
//! These exist for the token estimate. An image is billed by its pixels, so
//! knowing `1024x768` turns a worst-case charge into a real one. Only the
//! common formats are read, from their headers alone, in a few dozen lines
//! each; nothing here decodes a file. Any type without a probe gets size only,
//! and its registry row's token rule must cope with that.

use super::MimeType;

/// Pixel width and height, for the image types a probe can read.
pub fn dimensions(mime_type: &MimeType, bytes: &[u8]) -> Option<(u32, u32)> {
    match mime_type.as_str() {
        "image/png" => png(bytes),
        "image/gif" => gif(bytes),
        "image/jpeg" => jpeg(bytes),
        "image/webp" => webp(bytes),
        _ => None,
    }
}

/// Duration in milliseconds, for the audio types a probe can read.
pub fn duration_ms(mime_type: &MimeType, bytes: &[u8]) -> Option<u64> {
    match mime_type.as_str() {
        "audio/wav" | "audio/x-wav" | "audio/wave" => wav(bytes),
        _ => None,
    }
}

/// Page count, for the document types a probe can read.
pub fn pages(mime_type: &MimeType, bytes: &[u8]) -> Option<u32> {
    match mime_type.as_str() {
        "application/pdf" => pdf_pages(bytes),
        _ => None,
    }
}

/// PDF: every page is an object whose dictionary says `/Type /Page`, and the
/// page tree's root says `/Count N`. Both are read off the raw bytes, and the
/// larger wins: a file whose page objects sit inside compressed object streams
/// shows only the count (or, with the tree compressed too, nothing at all,
/// and the caller falls back to a size guess), while a file that was updated
/// in place can carry a stale root beside its live pages. `/Pages` is the tree
/// node, not a page, so the byte after `/Page` has to end the name.
fn pdf_pages(b: &[u8]) -> Option<u32> {
    if !b.starts_with(b"%PDF") {
        return None;
    }
    let mut page_objects: u32 = 0;
    let mut count: u32 = 0;
    let mut i = 0;
    while i < b.len() {
        if b[i] != b'/' {
            i += 1;
            continue;
        }
        let rest = &b[i..];
        if let Some(after) = rest.strip_prefix(b"/Type") {
            let name = after.trim_ascii_start();
            if let Some(tail) = name.strip_prefix(b"/Page")
                && !tail.first().is_some_and(|c| c.is_ascii_alphanumeric())
            {
                page_objects = page_objects.saturating_add(1);
            }
        } else if let Some(after) = rest.strip_prefix(b"/Count") {
            let digits = after.trim_ascii_start();
            let end = digits
                .iter()
                .position(|c| !c.is_ascii_digit())
                .unwrap_or(digits.len());
            if let Some(n) = std::str::from_utf8(&digits[..end])
                .ok()
                .and_then(|s| s.parse::<u32>().ok())
            {
                count = count.max(n);
            }
        }
        i += 1;
    }
    match page_objects.max(count) {
        0 => None,
        n => Some(n),
    }
}

fn be32(b: &[u8], at: usize) -> Option<u32> {
    let s = b.get(at..at + 4)?;
    Some(u32::from_be_bytes([s[0], s[1], s[2], s[3]]))
}

fn le32(b: &[u8], at: usize) -> Option<u32> {
    let s = b.get(at..at + 4)?;
    Some(u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
}

fn le16(b: &[u8], at: usize) -> Option<u16> {
    let s = b.get(at..at + 2)?;
    Some(u16::from_le_bytes([s[0], s[1]]))
}

fn be16(b: &[u8], at: usize) -> Option<u16> {
    let s = b.get(at..at + 2)?;
    Some(u16::from_be_bytes([s[0], s[1]]))
}

/// PNG: signature, then the IHDR chunk carries width and height big-endian.
fn png(b: &[u8]) -> Option<(u32, u32)> {
    if b.get(..8)? != b"\x89PNG\r\n\x1a\n" || b.get(12..16)? != b"IHDR" {
        return None;
    }
    Some((be32(b, 16)?, be32(b, 20)?))
}

/// GIF: `GIF87a` or `GIF89a`, then the logical screen size little-endian.
fn gif(b: &[u8]) -> Option<(u32, u32)> {
    let sig = b.get(..6)?;
    if sig != b"GIF87a" && sig != b"GIF89a" {
        return None;
    }
    Some((u32::from(le16(b, 6)?), u32::from(le16(b, 8)?)))
}

/// JPEG: walk the marker segments to the first start-of-frame, which carries
/// height then width big-endian after the precision byte.
fn jpeg(b: &[u8]) -> Option<(u32, u32)> {
    if b.get(..2)? != [0xFF, 0xD8] {
        return None;
    }
    let mut i = 2;
    while i + 4 <= b.len() {
        if b[i] != 0xFF {
            return None;
        }
        let marker = b[i + 1];
        // Padding bytes between segments.
        if marker == 0xFF {
            i += 1;
            continue;
        }
        // The loop condition guarantees the marker and its length are present.
        let len = usize::from(u16::from_be_bytes([b[i + 2], b[i + 3]]));
        let is_sof = matches!(marker, 0xC0..=0xCF) && !matches!(marker, 0xC4 | 0xC8 | 0xCC);
        if is_sof {
            let h = be16(b, i + 5)?;
            let w = be16(b, i + 7)?;
            return Some((u32::from(w), u32::from(h)));
        }
        i += 2 + len;
    }
    None
}

/// WebP: `RIFF....WEBP`, then a `VP8 `, `VP8L` or `VP8X` chunk.
fn webp(b: &[u8]) -> Option<(u32, u32)> {
    if b.get(..4)? != b"RIFF" || b.get(8..12)? != b"WEBP" {
        return None;
    }
    match b.get(12..16)? {
        b"VP8 " => {
            // Frame tag (3 bytes), start code (3 bytes), then width and
            // height as 14-bit little-endian values.
            let w = u32::from(le16(b, 26)?) & 0x3FFF;
            let h = u32::from(le16(b, 28)?) & 0x3FFF;
            Some((w, h))
        }
        b"VP8L" => {
            let bits = le32(b, 21)?;
            let w = (bits & 0x3FFF) + 1;
            let h = ((bits >> 14) & 0x3FFF) + 1;
            Some((w, h))
        }
        b"VP8X" => {
            let w = (u32::from(le16(b, 24)?) | (u32::from(*b.get(26)?) << 16)) + 1;
            let h = (u32::from(le16(b, 27)?) | (u32::from(*b.get(29)?) << 16)) + 1;
            Some((w, h))
        }
        _ => None,
    }
}

/// WAV: walk RIFF chunks for `fmt ` (byte rate) and `data` (payload size).
fn wav(b: &[u8]) -> Option<u64> {
    if b.get(..4)? != b"RIFF" || b.get(8..12)? != b"WAVE" {
        return None;
    }
    let mut i = 12;
    let mut byte_rate: Option<u32> = None;
    let mut data_len: Option<u32> = None;
    while i + 8 <= b.len() {
        // The loop condition guarantees the chunk header is present.
        let id = &b[i..i + 4];
        let len = u32::from_le_bytes([b[i + 4], b[i + 5], b[i + 6], b[i + 7]]);
        match id {
            b"fmt " => byte_rate = Some(le32(b, i + 16)?),
            b"data" => {
                data_len = Some(len);
                break;
            }
            _ => {}
        }
        i += 8 + len as usize + (len as usize & 1);
    }
    let rate = byte_rate?;
    if rate == 0 {
        return None;
    }
    Some(u64::from(data_len?) * 1000 / u64::from(rate))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mt(s: &str) -> MimeType {
        MimeType::parse(s).unwrap()
    }

    fn png_bytes(w: u32, h: u32) -> Vec<u8> {
        let mut v = b"\x89PNG\r\n\x1a\n".to_vec();
        v.extend_from_slice(&[0, 0, 0, 13]);
        v.extend_from_slice(b"IHDR");
        v.extend_from_slice(&w.to_be_bytes());
        v.extend_from_slice(&h.to_be_bytes());
        v
    }

    #[test]
    fn png_dimensions() {
        assert_eq!(
            dimensions(&mt("image/png"), &png_bytes(640, 480)),
            Some((640, 480))
        );
        assert_eq!(dimensions(&mt("image/png"), &png_bytes(1, 1)[..20]), None);
        assert_eq!(dimensions(&mt("image/png"), &png_bytes(1, 1)[..18]), None);
        let mut bad = png_bytes(1, 1);
        bad[12] = b'X';
        assert_eq!(dimensions(&mt("image/png"), &bad), None);
        assert_eq!(dimensions(&mt("image/png"), b"nope"), None);
        assert_eq!(dimensions(&mt("image/bmp"), &png_bytes(1, 1)), None);
    }

    #[test]
    fn gif_dimensions() {
        let mut v = b"GIF89a".to_vec();
        v.extend_from_slice(&[0x20, 0x01, 0xF0, 0x00]);
        assert_eq!(dimensions(&mt("image/gif"), &v), Some((288, 240)));
        assert_eq!(dimensions(&mt("image/gif"), b"GIF89"), None);
        assert_eq!(
            dimensions(&mt("image/gif"), b"GIF00a\x01\x00\x01\x00"),
            None
        );
        assert_eq!(dimensions(&mt("image/gif"), b"GIF87a\x01"), None);
        assert_eq!(dimensions(&mt("image/gif"), b"GIF87a\x01\x00\x02"), None);
    }

    #[test]
    fn jpeg_dimensions() {
        // SOI, APP0 (len 16, empty), SOF0 with height 300 width 400.
        let mut v = vec![0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10];
        v.extend_from_slice(&[0; 14]);
        v.extend_from_slice(&[
            0xFF, 0xFF, 0xFF, 0xC0, 0x00, 0x11, 0x08, 0x01, 0x2C, 0x01, 0x90,
        ]);
        assert_eq!(dimensions(&mt("image/jpeg"), &v), Some((400, 300)));
        // DHT (0xC4) is not a frame marker and must be skipped.
        let mut dht = vec![0xFF, 0xD8, 0xFF, 0xC4, 0x00, 0x02];
        dht.extend_from_slice(&[0xFF, 0xC2, 0x00, 0x11, 0x08, 0x00, 0x10, 0x00, 0x20]);
        assert_eq!(dimensions(&mt("image/jpeg"), &dht), Some((32, 16)));
        assert_eq!(
            dimensions(&mt("image/jpeg"), &[0xFF, 0xD8, 0x00, 0x00, 0x00, 0x00]),
            None
        );
        assert_eq!(
            dimensions(&mt("image/jpeg"), &[0xFF, 0xD8, 0xFF, 0xE0]),
            None
        );
        assert_eq!(
            dimensions(&mt("image/jpeg"), &[0xFF, 0xD8, 0xFF, 0xE0, 0x00]),
            None
        );
        assert_eq!(
            dimensions(
                &mt("image/jpeg"),
                &[0xFF, 0xD8, 0xFF, 0xC0, 0x00, 0x11, 0x08, 0x01, 0x2C, 0x01]
            ),
            None
        );
        assert_eq!(
            dimensions(&mt("image/jpeg"), &[0xFF, 0xD8, 0xFF, 0xC0, 0x00, 0x11]),
            None
        );
        assert_eq!(dimensions(&mt("image/jpeg"), &[0x00]), None);
        assert_eq!(dimensions(&mt("image/jpeg"), &[0x00, 0x00]), None);
    }

    #[test]
    fn webp_dimensions() {
        let mut lossy = b"RIFF\0\0\0\0WEBPVP8 \0\0\0\0".to_vec();
        lossy.extend_from_slice(&[0, 0, 0, 0x9d, 0x01, 0x2a]);
        lossy.extend_from_slice(&[0x80, 0x02, 0xE0, 0x01]);
        assert_eq!(dimensions(&mt("image/webp"), &lossy), Some((640, 480)));
        let mut lossless = b"RIFF\0\0\0\0WEBPVP8L\0\0\0\0\x2f".to_vec();
        let bits: u32 = (639) | (479 << 14);
        lossless.extend_from_slice(&bits.to_le_bytes());
        assert_eq!(dimensions(&mt("image/webp"), &lossless), Some((640, 480)));
        let mut ext = b"RIFF\0\0\0\0WEBPVP8X\0\0\0\0\0\0\0\0".to_vec();
        ext.extend_from_slice(&[0x7F, 0x02, 0x00, 0xDF, 0x01, 0x00]);
        assert_eq!(dimensions(&mt("image/webp"), &ext), Some((640, 480)));
        assert_eq!(dimensions(&mt("image/webp"), b"RIFF\0\0\0\0WEBPZZZZ"), None);
        assert_eq!(dimensions(&mt("image/webp"), b"RIFF\0\0\0\0WEBM"), None);
        assert_eq!(dimensions(&mt("image/webp"), b"RIFF\0\0\0\0WE"), None);
        assert_eq!(dimensions(&mt("image/webp"), b"RIFF\0\0\0\0WEBP"), None);
        assert_eq!(dimensions(&mt("image/webp"), b"RIF"), None);
        assert_eq!(dimensions(&mt("image/webp"), b"RIFF\0\0\0\0WEBPVP8 "), None);
        assert_eq!(dimensions(&mt("image/webp"), &lossy[..28]), None);
        assert_eq!(dimensions(&mt("image/webp"), b"RIFF\0\0\0\0WEBPVP8L"), None);
        assert_eq!(dimensions(&mt("image/webp"), b"RIFF\0\0\0\0WEBPVP8X"), None);
        let mut short_w = b"RIFF\0\0\0\0WEBPVP8X\0\0\0\0\0\0\0\0".to_vec();
        short_w.extend_from_slice(&[0x7F, 0x02]);
        assert_eq!(dimensions(&mt("image/webp"), &short_w), None);
        short_w.push(0x00);
        assert_eq!(dimensions(&mt("image/webp"), &short_w), None);
        short_w.extend_from_slice(&[0xDF, 0x01]);
        assert_eq!(dimensions(&mt("image/webp"), &short_w), None);
    }

    fn wav_bytes(byte_rate: u32, data_len: u32) -> Vec<u8> {
        let mut v = b"RIFF\0\0\0\0WAVE".to_vec();
        v.extend_from_slice(b"fmt ");
        v.extend_from_slice(&16u32.to_le_bytes());
        v.extend_from_slice(&[1, 0, 1, 0]);
        v.extend_from_slice(&44100u32.to_le_bytes());
        v.extend_from_slice(&byte_rate.to_le_bytes());
        v.extend_from_slice(&[2, 0, 16, 0]);
        v.extend_from_slice(b"data");
        v.extend_from_slice(&data_len.to_le_bytes());
        v
    }

    /// Three page objects under a root that counts them; the count alone
    /// when the pages are inside object streams; a stale root beside four
    /// live pages; and nothing for a file with neither, or that is not a PDF.
    #[test]
    fn pdf_pages_from_page_objects_or_the_tree_count() {
        let three =
            b"%PDF-1.7\n1 0 obj << /Type /Pages /Count 3 /Kids [2 0 R 3 0 R 4 0 R] >> endobj\n\
                      2 0 obj << /Type /Page /Parent 1 0 R >> endobj\n\
                      3 0 obj <</Type/Page/Parent 1 0 R>> endobj\n\
                      4 0 obj << /Type\n/Page >> endobj\n";
        assert_eq!(pages(&mt("application/pdf"), three), Some(3));
        let count_only = b"%PDF-1.5\n1 0 obj << /Type /Pages /Count 12 >> endobj\n2 0 obj << /Type /ObjStm >> stream ... endstream endobj\n";
        assert_eq!(pages(&mt("application/pdf"), count_only), Some(12));
        let stale_root = b"%PDF-1.4\n<< /Type /Pages /Count 2 >>\n<< /Type /Page >> << /Type /Page >> << /Type /Page >> << /Type /Page >>\n";
        assert_eq!(pages(&mt("application/pdf"), stale_root), Some(4));
        // `/Count` also sizes other things (an outline, say); a bad value is skipped.
        let junk_count = b"%PDF-1.4\n<< /Count x >> << /Type /Page >> << /Count 99999999999 >>\n";
        assert_eq!(pages(&mt("application/pdf"), junk_count), Some(1));
        assert_eq!(
            pages(&mt("application/pdf"), b"%PDF-1.7\n<< /Type /Font >>"),
            None
        );
        assert_eq!(pages(&mt("application/pdf"), b"%PDF"), None);
        assert_eq!(
            pages(&mt("application/pdf"), b"not a pdf /Type /Page"),
            None
        );
        assert_eq!(pages(&mt("image/png"), three), None);
    }

    #[test]
    fn wav_duration() {
        assert_eq!(
            duration_ms(&mt("audio/wav"), &wav_bytes(88200, 176400)),
            Some(2000)
        );
        assert_eq!(duration_ms(&mt("audio/x-wav"), &wav_bytes(0, 10)), None);
        assert_eq!(duration_ms(&mt("audio/wav"), b"RIFF\0\0\0\0WAVEjunk"), None);
        assert_eq!(duration_ms(&mt("audio/wav"), b"RIFF\0\0\0\0WAVX"), None);
        assert_eq!(duration_ms(&mt("audio/wav"), b"RIFF\0\0\0\0WA"), None);
        assert_eq!(duration_ms(&mt("audio/wav"), b"RI"), None);
        assert_eq!(duration_ms(&mt("audio/mpeg"), &wav_bytes(1, 1)), None);
        // An odd-length unknown chunk before fmt is padded to even.
        let mut v = b"RIFF\0\0\0\0WAVE".to_vec();
        v.extend_from_slice(b"LIST");
        v.extend_from_slice(&3u32.to_le_bytes());
        v.extend_from_slice(&[0, 0, 0, 0]);
        v.extend_from_slice(&wav_bytes(1000, 500)[12..]);
        assert_eq!(duration_ms(&mt("audio/wave"), &v), Some(500));
        // fmt present but no data chunk.
        let no_data = &wav_bytes(1000, 500)[..36];
        assert_eq!(duration_ms(&mt("audio/wav"), no_data), None);
        // fmt chunk truncated before the byte rate.
        let mut short_fmt = b"RIFF\0\0\0\0WAVEfmt ".to_vec();
        short_fmt.extend_from_slice(&16u32.to_le_bytes());
        short_fmt.extend_from_slice(&[0; 6]);
        assert_eq!(duration_ms(&mt("audio/wav"), &short_fmt), None);
    }
}
