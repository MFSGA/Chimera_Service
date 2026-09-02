use nyanpasu_core_metadata::{LogFrame, LogStream};

const MAX_CONTINUATION_LINES: usize = 16;
const MAX_LOG_TEXT_BYTES: usize = 16 * 1024;
const MAX_LOG_TARGET_BYTES: usize = 2048;
const MAX_LOG_TIMESTAMP_RAW_BYTES: usize = 256;
const MAX_LOG_FIELDS: usize = 64;
const MAX_LOG_FIELD_TEXT_BYTES: usize = 1024;

pub(super) struct Pending {
    pub(super) frame: LogFrame,
    continuations: usize,
}

impl Pending {
    pub(super) fn new(frame: LogFrame) -> Self {
        Self {
            frame,
            continuations: 0,
        }
    }

    pub(super) fn append(&mut self, line: &str) {
        if self.continuations == MAX_CONTINUATION_LINES {
            self.frame.truncated = true;
            return;
        }
        let cut = append_bounded(&mut self.frame.raw, line, MAX_LOG_TEXT_BYTES)
            | append_bounded(&mut self.frame.message, line, MAX_LOG_TEXT_BYTES);
        if cut {
            self.frame.truncated = true;
            self.continuations = MAX_CONTINUATION_LINES;
        } else {
            self.continuations += 1;
        }
    }
}

pub(super) fn bound_frame(frame: &mut LogFrame) {
    let mut cut = truncate_text(&mut frame.message, MAX_LOG_TEXT_BYTES);
    cut |= truncate_text(&mut frame.raw, MAX_LOG_TEXT_BYTES);
    if let Some(target) = frame.target.as_mut() {
        cut |= truncate_text(target, MAX_LOG_TARGET_BYTES);
    }
    if let Some(timestamp) = frame.timestamp.as_mut() {
        cut |= truncate_text(&mut timestamp.raw, MAX_LOG_TIMESTAMP_RAW_BYTES);
    }
    if frame.fields.len() > MAX_LOG_FIELDS {
        frame.fields.truncate(MAX_LOG_FIELDS);
        cut = true;
    }
    for field in &mut frame.fields {
        cut |= truncate_text(&mut field.key, MAX_LOG_FIELD_TEXT_BYTES);
        cut |= truncate_text(&mut field.value, MAX_LOG_FIELD_TEXT_BYTES);
    }
    frame.truncated |= cut;
}

pub(super) fn stream_index(stream: LogStream) -> usize {
    match stream {
        LogStream::Stdout => 0,
        LogStream::Stderr => 1,
    }
}

pub(super) fn strip_ansi(line: String) -> String {
    if !line.as_bytes().contains(&0x1b) {
        return line;
    }
    let bytes = line.into_bytes();
    let mut output = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] != 0x1b {
            output.push(bytes[index]);
            index += 1;
            continue;
        }
        if bytes.get(index + 1) != Some(&b'[') {
            index += 1;
            continue;
        }
        index += 2;
        while index < bytes.len() && !(0x40..=0x7e).contains(&bytes[index]) {
            index += 1;
        }
        index = (index + 1).min(bytes.len());
    }
    String::from_utf8(output).expect("dropping ASCII escapes preserves UTF-8")
}

fn append_bounded(text: &mut String, line: &str, max_bytes: usize) -> bool {
    if text.len() >= max_bytes {
        return true;
    }
    if line.is_empty() {
        text.push('\n');
        return false;
    }
    let budget = max_bytes - text.len() - 1;
    let end = char_boundary_at_or_below(line, budget.min(line.len()));
    if end == 0 {
        return true;
    }
    text.push('\n');
    text.push_str(&line[..end]);
    end < line.len()
}

fn truncate_text(text: &mut String, max_bytes: usize) -> bool {
    if text.len() <= max_bytes {
        return false;
    }
    text.truncate(char_boundary_at_or_below(text, max_bytes));
    true
}

fn char_boundary_at_or_below(text: &str, max_bytes: usize) -> usize {
    let mut end = max_bytes;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    end
}
