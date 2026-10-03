//! A reader of server-sent events (the `text/event-stream` format), one event at a time.

use std::io::{self, BufRead};

/// One server-sent event: its `event:` name, if any, and its `data:` lines joined by newlines.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SseEvent {
    pub event: Option<String>,
    pub data: String,
}

/// Reads events from a byte stream as they arrive.
pub(crate) struct SseReader<R> {
    reader: R,
    line: Vec<u8>,
    finished: bool,
}

impl<R: BufRead> SseReader<R> {
    pub fn new(reader: R) -> SseReader<R> {
        SseReader { reader, line: Vec::new(), finished: false }
    }

    /// The next event, `Ok(None)` at the end of the stream.
    pub fn next_event(&mut self) -> io::Result<Option<SseEvent>> {
        let mut event: Option<String> = None;
        let mut data: Option<String> = None;
        loop {
            if self.finished {
                return Ok(None);
            }
            self.line.clear();
            let read = self.reader.read_until(b'\n', &mut self.line)?;
            if read == 0 {
                self.finished = true;
                // A last event without its blank line is still an event.
                return Ok(data.map(|data| SseEvent { event, data }));
            }
            let text = String::from_utf8_lossy(&self.line);
            let line = text.trim_end_matches(['\n', '\r']);
            if line.is_empty() {
                match data.take() {
                    Some(data) => return Ok(Some(SseEvent { event, data })),
                    None => {
                        event = None;
                        continue;
                    }
                }
            }
            if line.starts_with(':') {
                continue;
            }
            let (field, value) = match line.split_once(':') {
                Some((field, value)) => (field, value.strip_prefix(' ').unwrap_or(value)),
                None => (line, ""),
            };
            match field {
                "event" => event = Some(value.to_string()),
                "data" => match &mut data {
                    Some(existing) => {
                        existing.push('\n');
                        existing.push_str(value);
                    }
                    None => data = Some(value.to_string()),
                },
                _ => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn events(text: &str) -> Vec<SseEvent> {
        let mut reader = SseReader::new(text.as_bytes());
        let mut out = vec![];
        while let Some(event) = reader.next_event().unwrap() {
            out.push(event);
        }
        out
    }

    #[test]
    fn events_are_split_on_blank_lines() {
        let read = events("event: a\ndata: {\"x\":1}\n\n: keep-alive\n\nevent: b\r\ndata: one\r\ndata: two\r\n\r\n");
        assert_eq!(
            read,
            vec![
                SseEvent { event: Some("a".into()), data: "{\"x\":1}".into() },
                SseEvent { event: Some("b".into()), data: "one\ntwo".into() },
            ]
        );
    }

    #[test]
    fn a_last_event_without_a_blank_line_is_kept() {
        assert_eq!(events("data:no-space\n"), vec![SseEvent { event: None, data: "no-space".into() }]);
        assert_eq!(events(""), vec![]);
        assert_eq!(events("event: lonely\n\n"), vec![], "an event with no data is not dispatched");
    }
}
