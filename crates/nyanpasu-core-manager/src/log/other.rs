use chrono::{DateTime, Duration, FixedOffset, NaiveDate, NaiveTime};
use nyanpasu_core_metadata::LogTimestamp;

use super::{
    ParsedLine,
    common::{local_unix_ms, parse_level, parse_rfc3339_ms, take_token},
};

pub(super) fn parse_meow(line: &str) -> Option<ParsedLine> {
    let (raw_time, rest) = take_token(line)?;
    let (level, rest) = take_token(rest)?;
    let level = parse_level(level)?;
    let (target, message) = rest.split_once(": ")?;
    if target.is_empty() {
        return None;
    }
    Some(ParsedLine {
        timestamp: Some(LogTimestamp {
            unix_ms: parse_rfc3339_ms(raw_time),
            raw: raw_time.to_owned(),
            inferred: false,
        }),
        level,
        target: Some(target.to_owned()),
        message: message.to_owned(),
        fields: Vec::new(),
    })
}

pub(super) fn parse_clash_rs(line: &str, observed_at: DateTime<FixedOffset>) -> Option<ParsedLine> {
    let timestamp_end = clash_rs_timestamp_end(line)?;
    let raw_time = line.get(..timestamp_end)?;
    let (level, rest) = take_token(line.get(timestamp_end..)?)?;
    let level = parse_level(level)?;
    let (target, message) = clash_rs_source(rest)?;
    Some(ParsedLine {
        timestamp: Some(LogTimestamp {
            unix_ms: clash_rs_unix_ms(raw_time, observed_at.offset()),
            raw: raw_time.to_owned(),
            inferred: true,
        }),
        level,
        target: Some(target.to_owned()),
        message: message.to_owned(),
        fields: Vec::new(),
    })
}

fn clash_rs_timestamp_end(line: &str) -> Option<usize> {
    const SEPARATORS: [(usize, u8); 6] = [
        (2, b'-'),
        (5, b'-'),
        (8, b' '),
        (11, b':'),
        (14, b':'),
        (17, b':'),
    ];
    const NUMBERS: [(usize, usize); 6] = [(0, 2), (3, 5), (6, 8), (9, 11), (12, 14), (15, 17)];
    let bytes = line.as_bytes();
    if bytes.len() < 20
        || !SEPARATORS
            .iter()
            .all(|(index, separator)| bytes.get(*index) == Some(separator))
        || !NUMBERS
            .iter()
            .all(|(start, end)| bytes[*start..*end].iter().all(u8::is_ascii_digit))
    {
        return None;
    }
    let digits = bytes[18..]
        .iter()
        .take_while(|byte| byte.is_ascii_digit())
        .count();
    ((1..=9).contains(&digits) && bytes.get(18 + digits) == Some(&b' ')).then_some(18 + digits)
}

fn clash_rs_unix_ms(raw: &str, offset: &FixedOffset) -> Option<i64> {
    let number = |range: std::ops::Range<usize>| raw.get(range)?.parse::<u32>().ok();
    let subsecond = raw.get(18..)?;
    let nanos = subsecond.parse::<u32>().ok()? * 10_u32.pow(9 - subsecond.len() as u32);
    let date = NaiveDate::from_ymd_opt(2000 + number(0..2)? as i32, number(3..5)?, number(6..8)?)?;
    let time =
        NaiveTime::from_hms_nano_opt(number(9..11)?, number(12..14)?, number(15..17)?, nanos)?;
    local_unix_ms(date, time, offset)
}

fn clash_rs_source(rest: &str) -> Option<(&str, &str)> {
    for (separator, _) in rest.match_indices(": ") {
        let head = &rest[..separator];
        let Some(colon) = head.rfind(':') else {
            continue;
        };
        if head[colon + 1..].is_empty()
            || !head[colon + 1..].bytes().all(|byte| byte.is_ascii_digit())
        {
            continue;
        }
        let start = head[..colon]
            .rfind(char::is_whitespace)
            .map_or(0, |index| index + 1);
        if start == colon {
            continue;
        }
        return Some((&head[start..], &rest[separator + 2..]));
    }
    None
}

pub(super) fn parse_premium(
    line: &str,
    observed_at: DateTime<FixedOffset>,
    clock: &mut Option<(NaiveDate, NaiveTime)>,
) -> Option<ParsedLine> {
    let bytes = line.as_bytes();
    if bytes.get(8) != Some(&b' ') || bytes.get(12) != Some(&b' ') {
        return None;
    }
    let raw_time = line.get(..8)?;
    let time = NaiveTime::parse_from_str(raw_time, "%H:%M:%S").ok()?;
    let level = parse_level(line.get(9..12)?)?;
    let (target, message) = premium_target(line.get(13..)?);
    Some(ParsedLine {
        timestamp: Some(LogTimestamp {
            unix_ms: premium_unix_ms(time, observed_at, clock),
            raw: raw_time.to_owned(),
            inferred: true,
        }),
        level,
        target,
        message,
        fields: Vec::new(),
    })
}

fn premium_target(body: &str) -> (Option<String>, String) {
    let tagged = body
        .strip_prefix('[')
        .and_then(|rest| rest.split_once("] "));
    match tagged {
        Some((tag, message)) if !tag.is_empty() && !tag.contains(' ') => {
            (Some(tag.to_owned()), message.to_owned())
        }
        _ => (None, body.to_owned()),
    }
}

fn premium_unix_ms(
    time: NaiveTime,
    observed_at: DateTime<FixedOffset>,
    clock: &mut Option<(NaiveDate, NaiveTime)>,
) -> Option<i64> {
    let date = match *clock {
        Some((date, previous)) if previous.signed_duration_since(time) > Duration::hours(12) => {
            date.succ_opt()?
        }
        Some((date, _)) => date,
        None => {
            let observed = observed_at.date_naive();
            if time.signed_duration_since(observed_at.time()) > Duration::hours(12) {
                observed.pred_opt()?
            } else {
                observed
            }
        }
    };
    *clock = Some((date, time));
    local_unix_ms(date, time, observed_at.offset())
}
