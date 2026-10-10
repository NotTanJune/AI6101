//! Incremental Server-Sent Events parser.
//!
//! Feed it raw byte chunks (which may split lines, multi-byte characters and `\r\n` pairs
//! anywhere) and it returns complete events. Comment lines (starting with `:`) are ignored.
//! A `data: [DONE]` event ends the stream; later input is ignored.

/// One parsed SSE item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SseItem {
    /// A complete event.
    Event(SseEvent),
    /// `data: [DONE]`: the stream is finished.
    Done,
}

/// A complete SSE event.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SseEvent {
    /// Value of the `event:` field, when present.
    pub event: Option<String>,
    /// `data:` lines joined with `\n`.
    pub data: String,
}

/// Incremental SSE parser.
#[derive(Debug, Default)]
pub struct SseParser {
    line: Vec<u8>,
    event: Option<String>,
    data: Vec<String>,
    skip_lf: bool,
    done: bool,
}

impl SseParser {
    /// New parser.
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether `[DONE]` has been seen.
    pub fn is_done(&self) -> bool {
        self.done
    }

    /// Feed a chunk and return the items completed by it.
    pub fn push(&mut self, chunk: &[u8]) -> Vec<SseItem> {
        let mut out = Vec::new();
        for &byte in chunk {
            if self.done {
                break;
            }
            if self.skip_lf {
                self.skip_lf = false;
                if byte == b'\n' {
                    continue;
                }
            }
            match byte {
                b'\r' => {
                    self.skip_lf = true;
                    self.end_line(&mut out);
                }
                b'\n' => self.end_line(&mut out),
                other => self.line.push(other),
            }
        }
        out
    }

    /// Signal end of input. A trailing partial line is processed and a pending event with data
    /// is dispatched.
    pub fn finish(&mut self) -> Vec<SseItem> {
        let mut out = Vec::new();
        if self.done {
            return out;
        }
        if !self.line.is_empty() {
            self.end_line(&mut out);
        }
        self.dispatch(&mut out);
        out
    }

    fn end_line(&mut self, out: &mut Vec<SseItem>) {
        let raw = std::mem::take(&mut self.line);
        if raw.is_empty() {
            self.dispatch(out);
            return;
        }
        let line = String::from_utf8_lossy(&raw);
        if line.starts_with(':') {
            return;
        }
        let (field, value) = match line.split_once(':') {
            Some((field, value)) => (field, value.strip_prefix(' ').unwrap_or(value)),
            None => (line.as_ref(), ""),
        };
        match field {
            "event" => self.event = Some(value.to_owned()),
            "data" => self.data.push(value.to_owned()),
            _ => {}
        }
    }

    fn dispatch(&mut self, out: &mut Vec<SseItem>) {
        let event = self.event.take();
        if self.data.is_empty() {
            return;
        }
        let data = self.data.join("\n");
        self.data.clear();
        if data == "[DONE]" {
            self.done = true;
            out.push(SseItem::Done);
        } else {
            out.push(SseItem::Event(SseEvent { event, data }));
        }
    }
}
