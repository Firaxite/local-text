// Text file encodings.
//
// Files are decoded to a `String` on load and encoded back with the same encoding
// on save, so a non-UTF-8 file round-trips byte for byte. Detection order: a BOM
// (UTF-8, UTF-16 LE/BE), then valid UTF-8, then BOM-less UTF-16 (most code units have
// a zero high byte), and otherwise Windows-1252 (a superset of Latin-1 for printable
// text) — unless the bytes contain NULs, which means a binary file.

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Encoding {
    Utf8,
    Utf8Bom,
    Utf16Le,
    Utf16Be,
    Windows1252,
}

impl Encoding {
    pub fn label(self) -> &'static str {
        match self {
            Encoding::Utf8        => "UTF-8",
            Encoding::Utf8Bom     => "UTF-8 BOM",
            Encoding::Utf16Le     => "UTF-16 LE",
            Encoding::Utf16Be     => "UTF-16 BE",
            Encoding::Windows1252 => "Windows-1252",
        }
    }

    pub const ALL: [Encoding; 5] = [
        Encoding::Utf8, Encoding::Utf8Bom, Encoding::Utf16Le, Encoding::Utf16Be, Encoding::Windows1252,
    ];
}

/// Windows-1252 bytes 0x80..=0x9F. The five bytes the standard leaves undefined map
/// to the matching C1 control characters (as browsers do), so every byte decodes and
/// re-encodes to itself.
const CP1252_HIGH: [char; 32] = [
    '\u{20AC}', '\u{0081}', '\u{201A}', '\u{0192}', '\u{201E}', '\u{2026}', '\u{2020}', '\u{2021}',
    '\u{02C6}', '\u{2030}', '\u{0160}', '\u{2039}', '\u{0152}', '\u{008D}', '\u{017D}', '\u{008F}',
    '\u{0090}', '\u{2018}', '\u{2019}', '\u{201C}', '\u{201D}', '\u{2022}', '\u{2013}', '\u{2014}',
    '\u{02DC}', '\u{2122}', '\u{0161}', '\u{203A}', '\u{0153}', '\u{009D}', '\u{017E}', '\u{0178}',
];

#[derive(Debug, PartialEq)]
pub enum DecodeError {
    /// Looks like a binary file (NUL bytes outside UTF-16).
    Binary,
    /// The bytes are not valid in the requested encoding.
    Invalid(Encoding),
}

/// Detect the encoding of `bytes` and decode them.
pub fn decode_auto(bytes: &[u8]) -> Result<(String, Encoding), DecodeError> {
    if let Some(rest) = bytes.strip_prefix(b"\xEF\xBB\xBF") {
        return decode_as(rest, Encoding::Utf8).map(|s| (s, Encoding::Utf8Bom));
    }
    if let Some(rest) = bytes.strip_prefix(b"\xFF\xFE") {
        return decode_as(rest, Encoding::Utf16Le).map(|s| (s, Encoding::Utf16Le));
    }
    if let Some(rest) = bytes.strip_prefix(b"\xFE\xFF") {
        return decode_as(rest, Encoding::Utf16Be).map(|s| (s, Encoding::Utf16Be));
    }
    let has_nul = bytes.contains(&0);
    if !has_nul {
        if let Ok(s) = std::str::from_utf8(bytes) {
            return Ok((s.to_owned(), Encoding::Utf8));
        }
    }
    if has_nul {
        if let Some(enc) = guess_bomless_utf16(bytes) {
            if let Ok(s) = decode_as(bytes, enc) {
                if !s.contains('\0') { return Ok((s, enc)); }
            }
        }
        return Err(DecodeError::Binary);
    }
    decode_as(bytes, Encoding::Windows1252).map(|s| (s, Encoding::Windows1252))
}

/// UTF-16 without a BOM: text that is mostly ASCII has a zero byte in every other
/// position. Returns the byte order if at least 70% of the code units look like that.
fn guess_bomless_utf16(bytes: &[u8]) -> Option<Encoding> {
    if bytes.len() < 4 || bytes.len() % 2 != 0 { return None; }
    let sample = &bytes[..bytes.len().min(4096)];
    let units = sample.len() / 2;
    let (mut le, mut be) = (0, 0);
    for pair in sample.chunks_exact(2) {
        if pair[1] == 0 && pair[0] != 0 { le += 1; }
        if pair[0] == 0 && pair[1] != 0 { be += 1; }
    }
    if le * 10 >= units * 7 { Some(Encoding::Utf16Le) }
    else if be * 10 >= units * 7 { Some(Encoding::Utf16Be) }
    else { None }
}

/// Decode `bytes` (without a BOM) as `enc`.
pub fn decode_as(bytes: &[u8], enc: Encoding) -> Result<String, DecodeError> {
    match enc {
        Encoding::Utf8 | Encoding::Utf8Bom => {
            let bytes = if enc == Encoding::Utf8Bom { bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(bytes) } else { bytes };
            String::from_utf8(bytes.to_vec()).map_err(|_| DecodeError::Invalid(enc))
        }
        Encoding::Utf16Le | Encoding::Utf16Be => {
            if bytes.len() % 2 != 0 { return Err(DecodeError::Invalid(enc)); }
            let units = bytes.chunks_exact(2).map(|p| {
                if enc == Encoding::Utf16Le { u16::from_le_bytes([p[0], p[1]]) } else { u16::from_be_bytes([p[0], p[1]]) }
            });
            char::decode_utf16(units).collect::<Result<String, _>>().map_err(|_| DecodeError::Invalid(enc))
        }
        Encoding::Windows1252 => Ok(bytes.iter().map(|&b| match b {
            0x80..=0x9F => CP1252_HIGH[(b - 0x80) as usize],
            _ => b as char,
        }).collect()),
    }
}

/// Decode a whole file's bytes as `enc`, accepting (and dropping) a BOM that matches.
pub fn decode_file_as(bytes: &[u8], enc: Encoding) -> Result<String, DecodeError> {
    let body = match enc {
        Encoding::Utf8 | Encoding::Utf8Bom => bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(bytes),
        Encoding::Utf16Le => bytes.strip_prefix(b"\xFF\xFE").unwrap_or(bytes),
        Encoding::Utf16Be => bytes.strip_prefix(b"\xFE\xFF").unwrap_or(bytes),
        Encoding::Windows1252 => bytes,
    };
    decode_as(body, enc)
}

/// Encode `text` as `enc` (with a BOM for the BOM/UTF-16 encodings). Fails with the
/// first character the encoding can't represent.
pub fn encode(text: &str, enc: Encoding) -> Result<Vec<u8>, char> {
    match enc {
        Encoding::Utf8 => Ok(text.as_bytes().to_vec()),
        Encoding::Utf8Bom => {
            let mut out = Vec::with_capacity(text.len() + 3);
            out.extend_from_slice(b"\xEF\xBB\xBF");
            out.extend_from_slice(text.as_bytes());
            Ok(out)
        }
        Encoding::Utf16Le | Encoding::Utf16Be => {
            let mut out = Vec::with_capacity(text.len() * 2 + 2);
            out.extend_from_slice(if enc == Encoding::Utf16Le { b"\xFF\xFE" } else { b"\xFE\xFF" });
            for unit in text.encode_utf16() {
                out.extend_from_slice(&if enc == Encoding::Utf16Le { unit.to_le_bytes() } else { unit.to_be_bytes() });
            }
            Ok(out)
        }
        Encoding::Windows1252 => text.chars().map(|ch| {
            let c = ch as u32;
            if c < 0x80 || (0xA0..=0xFF).contains(&c) { return Ok(c as u8); }
            CP1252_HIGH.iter().position(|&h| h == ch).map(|i| 0x80 + i as u8).ok_or(ch)
        }).collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_and_round_trips_every_encoding() {
        let text = "héllo €uro\r\nline 2\n";
        for enc in Encoding::ALL {
            let bytes = encode(text, enc).unwrap();
            let (back, found) = decode_auto(&bytes).unwrap();
            assert_eq!(back, text, "{enc:?}");
            // Plain UTF-8 and UTF-8 with BOM are told apart by the BOM.
            assert_eq!(found, enc, "{enc:?}");
            assert_eq!(encode(&back, found).unwrap(), bytes, "{enc:?} re-encodes identically");
        }
    }

    #[test]
    fn every_cp1252_byte_round_trips() {
        let bytes: Vec<u8> = (1u8..=255).collect();
        let s = decode_as(&bytes, Encoding::Windows1252).unwrap();
        assert_eq!(encode(&s, Encoding::Windows1252).unwrap(), bytes);
    }

    #[test]
    fn bomless_utf16_is_detected_and_binary_is_refused() {
        let utf16: Vec<u8> = "plain ascii text".encode_utf16().flat_map(|u| u.to_le_bytes()).collect();
        assert_eq!(decode_auto(&utf16).unwrap(), ("plain ascii text".to_owned(), Encoding::Utf16Le));
        assert_eq!(decode_auto(b"\x7fELF\x02\x01\x01\x00\x00\x00\x00\x00\x00\x00\x00\x00\x03\x00"), Err(DecodeError::Binary));
    }

    #[test]
    fn unrepresentable_characters_are_reported() {
        assert_eq!(encode("ok 😀", Encoding::Windows1252), Err('😀'));
    }
}
