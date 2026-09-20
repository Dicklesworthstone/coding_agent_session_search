//! Consume JSON without building a Value tree or retaining discarded content.
//!
//! Use ordinary deserialize_any calls, not IgnoredAny's optimized skip path:
//! validation must check string encoding/escapes and numeric ranges just as the
//! real Value parser does. Serde JSON still owns syntax and recursion limits.

use std::fmt;

use serde::de::{MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer};

pub(super) struct CheckedJson;

impl<'de> Deserialize<'de> for CheckedJson {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(JsonVisitor)
    }
}

struct JsonVisitor;

impl<'de> Visitor<'de> for JsonVisitor {
    type Value = CheckedJson;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a JSON value")
    }

    fn visit_bool<E>(self, _value: bool) -> Result<CheckedJson, E> {
        Ok(CheckedJson)
    }

    fn visit_i64<E>(self, _value: i64) -> Result<CheckedJson, E> {
        Ok(CheckedJson)
    }

    fn visit_u64<E>(self, _value: u64) -> Result<CheckedJson, E> {
        Ok(CheckedJson)
    }

    fn visit_f64<E>(self, _value: f64) -> Result<CheckedJson, E> {
        Ok(CheckedJson)
    }

    fn visit_str<E>(self, _value: &str) -> Result<CheckedJson, E> {
        Ok(CheckedJson)
    }

    fn visit_unit<E>(self) -> Result<CheckedJson, E> {
        Ok(CheckedJson)
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut sequence: A) -> Result<CheckedJson, A::Error> {
        while sequence.next_element::<CheckedJson>()?.is_some() {}
        Ok(CheckedJson)
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<CheckedJson, A::Error> {
        while map.next_key::<CheckedJson>()?.is_some() {
            map.next_value::<CheckedJson>()?;
        }
        Ok(CheckedJson)
    }
}

#[cfg(test)]
mod tests {
    use super::super::validate_legacy;
    use super::*;
    use std::io::{self, BufReader, Cursor, Read};

    #[test]
    fn validation_matches_value_parser_for_strings_numbers_and_nested_data() {
        let cases: &[&[u8]] = &[
            b"null",
            b"true",
            b"false",
            b"0",
            b"-0.5e2",
            b"18446744073709551615",
            b"{\"items\":[null,true,1,{\"text\":\"hello\"}]}",
            b"\"\\ud83d\\ude00\"",
            b"{\"escaped\\u006bey\":\"value\"}",
            b"1e400",
            b"01",
            b"\"\\ud800\"",
            b"{\"\\ud800\":null}",
            b"\"\xff\"",
            b"{\"\xff\":1}",
            b"{\"items\":[1,]}",
            b"{} trailing",
            b"{",
            b"",
        ];
        for bytes in cases {
            let expected = serde_json::from_slice::<serde_json::Value>(bytes).is_ok();
            let actual = serde_json::from_reader::<_, CheckedJson>(Cursor::new(bytes)).is_ok();
            assert_eq!(actual, expected, "{bytes:?}");
        }
        let too_deep = format!("{}null{}", "[".repeat(150), "]".repeat(150));
        assert!(serde_json::from_str::<CheckedJson>(&too_deep).is_err());
        assert_eq!(std::mem::size_of::<CheckedJson>(), 0);
    }

    #[test]
    fn legacy_validation_handles_short_reads_and_bom_without_buffering_the_document() {
        struct OneByte<R>(R);
        impl<R: Read> Read for OneByte<R> {
            fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
                let count = buffer.len().min(1);
                self.0.read(&mut buffer[..count])
            }
        }
        for bytes in [
            b"{\"items\":[]}".as_slice(),
            b"\xef\xbb\xbf{\"items\":[]}\r\n".as_slice(),
        ] {
            let mut reader = BufReader::with_capacity(1, OneByte(Cursor::new(bytes)));
            validate_legacy(&mut reader).unwrap();
        }
    }

    #[test]
    fn legacy_reader_fault_after_a_complete_document_is_not_successful_eof() {
        struct FailingRead(Cursor<Vec<u8>>);
        impl Read for FailingRead {
            fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
                if self.0.position() == self.0.get_ref().len() as u64 {
                    Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "private reader detail",
                    ))
                } else {
                    self.0.read(buffer)
                }
            }
        }
        let mut reader = BufReader::new(FailingRead(Cursor::new(b"{\"items\":[]}".to_vec())));
        let error = validate_legacy(&mut reader).unwrap_err();
        assert_eq!(
            error.downcast_ref::<io::Error>().unwrap().kind(),
            io::ErrorKind::PermissionDenied
        );
        assert!(!error.to_string().contains("private reader detail"));
    }
}
