//! Tolerant text file reading.
//!
//! Every text file this wrapper reads is authored by someone else - mod
//! `DefaultInput.ini` files, the user's `FusionWrapper.ini`, and the user's own
//! `Input.ini` written by the engine. On Windows those arrive in whatever
//! encoding the producer felt like:
//!
//! - UE writes config files as UTF-16LE (with BOM) as soon as the content has a
//!   non-ANSI character, and as plain bytes otherwise, so a Russian keybind name
//!   is enough to flip `Input.ini` to UTF-16.
//! - PowerShell 5.1's `>` and `Out-File` default to UTF-16LE; several editors
//!   default to UTF-8 with a BOM.
//! - Legacy files may be in a single-byte codepage (cp1251 for Russian).
//!
//! `std::fs::read_to_string` accepts none of those but clean UTF-8, which turns
//! any of them into a config that is silently ignored. [`read`] decodes them all
//! and keeps the encoding, because appending UTF-8 to a UTF-16 `Input.ini` would
//! corrupt the user's keybinds.

use std::path::Path;

/// How a text file was encoded on disk, so an append can match it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Encoding {
    /// UTF-8, BOM or not: the BOM is stripped on read and an append never needs
    /// to reproduce one, since it goes after bytes that already carry it.
    Utf8,
    /// UTF-16, from a BOM or from NUL placement. Byte order has to be carried
    /// because appending in the wrong one turns the file into garbage.
    Utf16 { little_endian: bool },
}

/// A text file's contents plus the encoding it was stored in.
#[derive(Debug)]
pub struct Text {
    pub body: String,
    pub encoding: Encoding,
}

impl Text {
    /// Re-encode `s` the way this file was stored, for appending to it.
    pub fn encode(&self, s: &str) -> Vec<u8> {
        match self.encoding {
            Encoding::Utf8 => s.as_bytes().to_vec(),
            Encoding::Utf16 { little_endian } => s
                .encode_utf16()
                .flat_map(|u| {
                    let [a, b] = u.to_le_bytes();
                    match little_endian {
                        true => [a, b],
                        false => [b, a],
                    }
                })
                .collect(),
        }
    }
}

/// Read a text file, honouring a BOM and falling back to lossy UTF-8.
///
/// Malformed bytes become replacement characters rather than an error: a mod
/// with one bad byte in a comment should still contribute its keybinds, which
/// is also what the C# tool's `File.ReadAllText` does.
pub fn read(path: &Path) -> std::io::Result<Text> {
    Ok(decode(&std::fs::read(path)?))
}

fn decode(bytes: &[u8]) -> Text {
    match bytes {
        [0xFF, 0xFE, rest @ ..] => decode_utf16(rest, true),
        [0xFE, 0xFF, rest @ ..] => decode_utf16(rest, false),
        [0xEF, 0xBB, 0xBF, rest @ ..] => Text {
            body: String::from_utf8_lossy(rest).into_owned(),
            encoding: Encoding::Utf8,
        },
        // No BOM. Text files never legitimately contain NUL, so a NUL early on
        // means UTF-16 that lost its BOM; which half is zero gives the byte
        // order. Anything else is treated as UTF-8.
        _ => match nul_endianness(bytes) {
            Some(little_endian) => decode_utf16(bytes, little_endian),
            None => Text {
                body: String::from_utf8_lossy(bytes).into_owned(),
                encoding: Encoding::Utf8,
            },
        },
    }
}

/// `Some(little_endian)` when the NUL bytes in the first part of `bytes` sit
/// consistently on one side of each 16-bit pair.
fn nul_endianness(bytes: &[u8]) -> Option<bool> {
    let head = &bytes[..bytes.len().min(512)];
    if !head.contains(&0) {
        return None;
    }
    let (mut odd, mut even) = (0usize, 0usize);
    for (i, b) in head.iter().enumerate() {
        if *b == 0 {
            match i % 2 {
                0 => even += 1,
                _ => odd += 1,
            }
        }
    }
    // ASCII-ish UTF-16LE is `4D 00 65 00`: high bytes, and so the NULs, land on
    // odd indices. UTF-16BE is the mirror image.
    match (odd, even) {
        (o, 0) if o > 0 => Some(true),
        (0, e) if e > 0 => Some(false),
        _ => None,
    }
}

fn decode_utf16(bytes: &[u8], little_endian: bool) -> Text {
    let units: Vec<u16> = bytes
        .chunks_exact(2)
        .map(|p| match little_endian {
            true => u16::from_le_bytes([p[0], p[1]]),
            false => u16::from_be_bytes([p[0], p[1]]),
        })
        .collect();
    Text {
        body: String::from_utf16_lossy(&units),
        encoding: Encoding::Utf16 { little_endian },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn body(bytes: &[u8]) -> (String, Encoding) {
        let t = decode(bytes);
        (t.body, t.encoding)
    }

    #[test]
    fn plain_utf8_is_unchanged() {
        assert_eq!(
            body(b"merge_incompatible=all"),
            ("merge_incompatible=all".into(), Encoding::Utf8)
        );
    }

    #[test]
    fn utf8_bom_is_stripped() {
        // A surviving BOM lands in the first key, so `FusionWrapper.ini` written
        // by a BOM-emitting editor parses as an unknown key and is ignored.
        let mut bytes = vec![0xEF, 0xBB, 0xBF];
        bytes.extend_from_slice(b"merge_incompatible=all");
        assert_eq!(
            body(&bytes),
            ("merge_incompatible=all".into(), Encoding::Utf8)
        );
    }

    #[test]
    fn utf16le_bom_decodes() {
        // What PowerShell's `>` produces, and what UE writes for configs with
        // non-ANSI characters.
        let mut bytes = vec![0xFF, 0xFE];
        bytes.extend(
            "merge_incompatible=МодНазвание"
                .encode_utf16()
                .flat_map(u16::to_le_bytes),
        );
        assert_eq!(
            body(&bytes),
            (
                "merge_incompatible=МодНазвание".into(),
                Encoding::Utf16 {
                    little_endian: true
                }
            )
        );
    }

    #[test]
    fn utf16be_bom_decodes() {
        let mut bytes = vec![0xFE, 0xFF];
        bytes.extend("Тест".encode_utf16().flat_map(u16::to_be_bytes));
        assert_eq!(
            body(&bytes),
            (
                "Тест".into(),
                Encoding::Utf16 {
                    little_endian: false
                }
            )
        );
    }

    #[test]
    fn bomless_utf16_detected_from_nuls() {
        let bytes: Vec<u8> = "[/Script/Engine.InputSettings]"
            .encode_utf16()
            .flat_map(u16::to_le_bytes)
            .collect();
        assert_eq!(
            body(&bytes),
            (
                "[/Script/Engine.InputSettings]".into(),
                Encoding::Utf16 {
                    little_endian: true
                }
            )
        );
    }

    #[test]
    fn single_byte_codepage_survives_as_lossy_utf8() {
        // cp1251 "Тест" - invalid UTF-8. The line must still be usable rather
        // than failing the whole file.
        let bytes = [b'x', b'=', 0xD2, 0xE5, 0xF1, 0xF2];
        let (body, encoding) = body(&bytes);
        assert!(body.starts_with("x="));
        assert_eq!(encoding, Encoding::Utf8);
    }

    #[test]
    fn round_trips_through_encode() {
        for bom in [vec![0xFFu8, 0xFE], vec![0xEF, 0xBB, 0xBF]] {
            let mut bytes = bom;
            let is_utf16 = bytes[0] == 0xFF;
            match is_utf16 {
                true => bytes.extend("ключ=да".encode_utf16().flat_map(u16::to_le_bytes)),
                false => bytes.extend("ключ=да".as_bytes()),
            }
            let text = decode(&bytes);
            let appended = text.encode("\nключ2=нет");
            let mut whole = bytes.clone();
            whole.extend_from_slice(&appended);
            assert_eq!(decode(&whole).body, "ключ=да\nключ2=нет");
        }
    }
}
