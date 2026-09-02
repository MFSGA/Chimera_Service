use nyanpasu_core_metadata::{LogField, LogTimestamp};

use super::{ParsedLine, common::{parse_level, parse_rfc3339_ms}};

pub(super) fn parse(line: &str) -> Option<ParsedLine> {
    let mut raw_time = None;
    let mut level = None;
    let mut message = None;
    let mut fields = Vec::new();
    for (key, value) in scan_logfmt(line)? {
        match key.as_str() {
            "time" => raw_time = Some(value),
            "level" => level = parse_level(&value),
            "msg" => message = Some(value),
            _ => fields.push(LogField { key, value }),
        }
    }
    let raw_time = raw_time?;
    Some(ParsedLine {
        timestamp: Some(LogTimestamp {
            unix_ms: parse_rfc3339_ms(&raw_time),
            raw: raw_time,
            inferred: false,
        }),
        level: level?,
        target: None,
        message: message?,
        fields,
    })
}

fn scan_logfmt(line: &str) -> Option<Vec<(String, String)>> {
    let bytes = line.as_bytes();
    let mut fields = Vec::new();
    let mut index = 0;
    while index < bytes.len() {
        while bytes.get(index).is_some_and(u8::is_ascii_whitespace) {
            index += 1;
        }
        if index == bytes.len() {
            break;
        }
        let key_start = index;
        while index < bytes.len() && bytes[index] != b'=' {
            if bytes[index].is_ascii_whitespace() {
                return None;
            }
            index += 1;
        }
        if index == key_start || index == bytes.len() {
            return None;
        }
        let key = line.get(key_start..index)?.to_owned();
        index += 1;

        let value = if bytes.get(index) == Some(&b'"') {
            index += 1;
            let mut value = Vec::new();
            let mut closed = false;
            while index < bytes.len() {
                match bytes[index] {
                    b'"' => {
                        index += 1;
                        closed = true;
                        break;
                    }
                    b'\\' => {
                        let escaped = *bytes.get(index + 1)?;
                        match escaped {
                            b'"' | b'\\' => value.push(escaped),
                            b'n' => value.push(b'\n'),
                            b'r' => value.push(b'\r'),
                            b't' => value.push(b'\t'),
                            _ => value.extend_from_slice(&[b'\\', escaped]),
                        }
                        index += 2;
                    }
                    byte => {
                        value.push(byte);
                        index += 1;
                    }
                }
            }
            if !closed
                || bytes
                    .get(index)
                    .is_some_and(|byte| !byte.is_ascii_whitespace())
            {
                return None;
            }
            String::from_utf8(value).ok()?
        } else {
            let value_start = index;
            while index < bytes.len() && !bytes[index].is_ascii_whitespace() {
                index += 1;
            }
            line.get(value_start..index)?.to_owned()
        };
        fields.push((key, value));
    }
    Some(fields)
}
