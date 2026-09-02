use std::{
    borrow::Cow,
    sync::{Arc, OnceLock},
};

use bounded_vec_deque::BoundedVecDeque;
use parking_lot::Mutex;
use tracing_subscriber::fmt::MakeWriter;

const LOG_BUFFER_CAPACITY: usize = 100;

/// In-memory service-log tail used by `/logs/retrieve` and `/logs/inspect`.
pub struct Logger<'n> {
    buffer: Arc<Mutex<BoundedVecDeque<Cow<'n, str>>>>,
}

impl Clone for Logger<'_> {
    fn clone(&self) -> Self {
        Self {
            buffer: self.buffer.clone(),
        }
    }
}

impl<'n> Logger<'n> {
    pub fn new() -> Self {
        Self {
            buffer: Arc::new(Mutex::new(BoundedVecDeque::new(LOG_BUFFER_CAPACITY))),
        }
    }

    pub fn global() -> &'static Logger<'static> {
        static INSTANCE: OnceLock<Logger> = OnceLock::new();
        INSTANCE.get_or_init(Logger::new)
    }

    pub fn retrieve_logs(&self) -> Vec<Cow<'n, str>> {
        self.buffer.lock().drain(..).collect()
    }

    pub fn inspect_logs(&self) -> Vec<Cow<'n, str>> {
        self.buffer.lock().iter().cloned().collect()
    }
}

impl<'n> Default for Logger<'n> {
    fn default() -> Self {
        Self::new()
    }
}

impl std::io::Write for Logger<'_> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let msg = String::from_utf8_lossy(buf);
        self.buffer.lock().push_back(Cow::Owned(msg.into_owned()));
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for Logger<'static> {
    type Writer = Logger<'static>;

    fn make_writer(&'a self) -> Self::Writer {
        Self {
            buffer: self.buffer.clone(),
        }
    }
}
