use crate::utf8_bytes;

use base64::prelude::BASE64_STANDARD;
use base64::Engine;
use chrono::{DateTime, FixedOffset};
use quoted_printable;
use regex::bytes::Regex;
use std::io;
use std::sync::OnceLock;
use std::vec::Vec;

#[derive(Default)]
pub struct MhtmlPiece {
    pub content_type: String,
    pub location: String,
    pub bytes: Vec<u8>,
}

#[derive(Default)]
pub struct MhtmlDoc {
    pub subject: String,
    pub date: DateTime<FixedOffset>,
    pub location: String,
    pub pieces: Vec<MhtmlPiece>,
}

// Duplicating this function for now.
fn invalid_data_err(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

fn decode_base64_containing_whitespace(data: &[u8]) -> Vec<u8> {
    let mut copy = Vec::from(data);
    copy.retain(|b| !b.is_ascii_whitespace());
    BASE64_STANDARD.decode(copy).unwrap()
}

fn parse_mhtml_piece(text: &[u8]) -> Result<MhtmlPiece, io::Error> {
    static SECTION_RE_LOCK: OnceLock<Regex> = OnceLock::new();
    let section_re = SECTION_RE_LOCK.get_or_init(|| {
        Regex::new(
            r#"(?x)^Content-Type:\s(?P<content_type>\S+)\s*
(?:Content-ID:\s\S+\s+)?
Content-Transfer-Encoding:\s(?P<encoding>\S+)\s*
Content-Location:\s(?P<location>\S+)\s*"#,
        )
        .unwrap()
    });
    let Some(captures) = section_re.captures(text) else {
        println!("Problem parsing: <<{}>>", utf8_bytes::to_str(text));
        return Err(invalid_data_err("MHTML piece doesn't have expected header"));
    };
    let mut piece: MhtmlPiece = Default::default();
    piece.content_type = utf8_bytes::to_string(&captures["content_type"]);
    piece.location = utf8_bytes::to_string(&captures["location"]);
    let remainder = &text[captures.get(0).unwrap().end()..];
    let encoding = utf8_bytes::to_str(&captures["encoding"]);
    piece.bytes = match encoding {
        "base64" => decode_base64_containing_whitespace(remainder),
        "quoted-printable" => {
            quoted_printable::decode(remainder, quoted_printable::ParseMode::Strict).unwrap()
        }
        _ => panic!("Unknown encoding {} for {}", &encoding, &piece.location),
    };

    Ok(piece)
}

pub fn parse(contents: &[u8]) -> Result<MhtmlDoc, io::Error> {
    static HEADER_RE_LOCK: OnceLock<Regex> = OnceLock::new();
    let header_re = HEADER_RE_LOCK.get_or_init(|| {
        Regex::new(
            r#"(?x)^From:\s[^\r\n]+\s*
Snapshot-Content-Location:\s(?P<location>[^\r\n]+)\s*
Subject:\s(?P<subject>[^\r\n]+)\s*
Date:\s(?<date>[^\r\n]+)\s*
MIME-Version:\s[^\r\n]+\s*
Content-Type:\s[^\r\n]+\s*
\s+type=[^\r\n]+
\s+boundary="(?P<boundary>[^"]+)""#,
        )
        .unwrap()
    });

    let mut contents_slice = contents;
    let mut doc: MhtmlDoc = Default::default();
    let Some(header_captures) = header_re.captures(contents_slice) else {
        return Err(invalid_data_err("MHTML doesn't have expected header"));
    };
    doc.subject = utf8_bytes::to_string(&header_captures["subject"]);
    doc.date = DateTime::parse_from_rfc2822(utf8_bytes::to_str(&header_captures["date"])).unwrap();
    doc.location = utf8_bytes::to_string(&header_captures["location"]);

    // Skip past the header, matched by the Regex.
    let full_match = header_captures.get(0).unwrap();
    contents_slice = &contents_slice[full_match.end()..];

    let mut boundary_pattern: Vec<u8> = Vec::new();
    header_captures.expand(br#"[\r\n]*-*$boundary-*[\r\n]*"#, &mut boundary_pattern);
    let boundary_re = Regex::new(utf8_bytes::to_str(&boundary_pattern)).unwrap();

    for raw_piece in boundary_re.split(contents_slice).filter(|x| !x.is_empty()) {
        doc.pieces.push(self::parse_mhtml_piece(raw_piece)?);
    }
    Ok(doc)
}
