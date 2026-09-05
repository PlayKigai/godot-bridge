use std::fmt;

use crate::json::Value;
use std::io::{Read, Write};
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, TryRecvError};
use std::thread::{self, JoinHandle};
use std::time::Duration;

const HEADER_CAP: usize = 16 * 1024;
const COMPACT_THRESHOLD: usize = 64 * 1024;
const READ_CHUNK_SIZE: usize = 8192;

#[derive(Debug)]
pub enum FrameError {
    Oversized(usize),
    Malformed(String),
    Io(std::io::Error),
}

impl fmt::Display for FrameError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Oversized(size) => write!(formatter, "frame body is oversized: {size} bytes"),
            Self::Malformed(message) => write!(formatter, "malformed frame: {message}"),
            Self::Io(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for FrameError {}

pub struct FrameDecoder {
    cap: usize,
    buf: Vec<u8>,
    consumed: usize,
}

impl FrameDecoder {
    pub fn new(cap: usize) -> Self {
        Self {
            cap,
            buf: Vec::new(),
            consumed: 0,
        }
    }

    pub fn push(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }

    pub fn next_frame(&mut self) -> Result<Option<&[u8]>, FrameError> {
        if self.consumed >= COMPACT_THRESHOLD {
            self.compact();
        }
        let input = &self.buf[self.consumed..];
        let (header_end, delimiter_len) = match (
            input.windows(4).position(|window| window == b"\r\n\r\n"),
            input.windows(2).position(|window| window == b"\n\n"),
        ) {
            (Some(crlf_end), Some(lf_end)) if lf_end < crlf_end => (lf_end, 2),
            (Some(crlf_end), _) => (crlf_end, 4),
            (None, Some(lf_end)) => (lf_end, 2),
            (None, None) => {
                if input.len() > HEADER_CAP {
                    return Err(FrameError::Malformed("frame header is too long".to_owned()));
                }
                return Ok(None);
            }
        };

        if header_end > HEADER_CAP {
            return Err(FrameError::Malformed("frame header is too long".to_owned()));
        }

        let header = &input[..header_end];
        let mut content_length = None;
        for line in header.split(|byte| *byte == b'\n') {
            let line = line.strip_suffix(b"\r").unwrap_or(line);
            let Some(separator) = line.iter().position(|byte| *byte == b':') else {
                return Err(FrameError::Malformed(
                    "header line has no colon".to_string(),
                ));
            };
            let name = &line[..separator];
            let value_start = line[separator + 1..]
                .iter()
                .position(|byte| !byte.is_ascii_whitespace())
                .map_or(line.len(), |offset| separator + 1 + offset);
            let value = &line[value_start..];
            if name.eq_ignore_ascii_case(b"Content-Length") {
                if content_length.is_some() {
                    return Err(FrameError::Malformed(
                        "duplicate Content-Length header".to_string(),
                    ));
                }
                let value = std::str::from_utf8(value).map_err(|_| {
                    FrameError::Malformed("Content-Length is not ASCII".to_string())
                })?;
                let length = value.trim().parse::<usize>().map_err(|_| {
                    FrameError::Malformed("Content-Length is not a number".to_string())
                })?;
                content_length = Some(length);
            }
        }

        let length = content_length
            .ok_or_else(|| FrameError::Malformed("missing Content-Length header".to_string()))?;
        if length > self.cap {
            return Err(FrameError::Oversized(length));
        }

        let body_start = header_end + delimiter_len;
        let body_end = body_start
            .checked_add(length)
            .ok_or(FrameError::Oversized(usize::MAX))?;
        if input.len() < body_end {
            return Ok(None);
        }

        let body_start = self.consumed + body_start;
        let body_end = self.consumed + body_end;
        self.consumed = body_end;
        Ok(Some(&self.buf[body_start..body_end]))
    }

    fn has_pending_bytes(&self) -> bool {
        self.consumed < self.buf.len()
    }

    fn compact(&mut self) {
        if self.consumed == self.buf.len() {
            self.buf.clear();
        } else {
            self.buf.copy_within(self.consumed.., 0);
            self.buf.truncate(self.buf.len() - self.consumed);
        }
        self.consumed = 0;
    }
}

pub struct FrameReader<R: Read> {
    reader: R,
    decoder: FrameDecoder,
}

pub struct ReadChunk {
    bytes: [u8; READ_CHUNK_SIZE],
    len: usize,
}

pub(crate) type ReadEvent = Result<Option<ReadChunk>, std::io::Error>;

pub fn spawn_frame_reader<R: Read + Send + 'static>(
    name: &str,
    mut reader: R,
    _cap: usize,
    sender: SyncSender<ReadEvent>,
) -> std::io::Result<JoinHandle<()>> {
    let name = name.to_owned();
    thread::Builder::new()
        .name(name)
        .stack_size(256 * 1024)
        .spawn(move || {
            loop {
                let mut chunk = ReadChunk {
                    bytes: [0; READ_CHUNK_SIZE],
                    len: 0,
                };
                let event = match reader.read(&mut chunk.bytes) {
                    Ok(0) => Ok(None),
                    Ok(len) => {
                        chunk.len = len;
                        Ok(Some(chunk))
                    }
                    Err(error) => Err(error),
                };
                let done = matches!(&event, Ok(None) | Err(_));
                if sender.send(event).is_err() {
                    return;
                }
                if done {
                    return;
                }
            }
        })
}

pub enum FramePoll<T> {
    Frame(T),
    Empty,
    End,
}

pub struct FrameInput {
    receiver: Receiver<ReadEvent>,
    decoder: FrameDecoder,
    eof: bool,
}

impl FrameInput {
    pub fn new(receiver: Receiver<ReadEvent>, cap: usize) -> Self {
        Self {
            receiver,
            decoder: FrameDecoder::new(cap),
            eof: false,
        }
    }

    pub fn with_next_frame<T>(
        &mut self,
        callback: impl FnOnce(&[u8]) -> T,
    ) -> Result<Option<T>, FrameError> {
        loop {
            if let Some(body) = self.decoder.next_frame()? {
                return Ok(Some(callback(body)));
            }
            if self.eof {
                return Ok(None);
            }
            match self.receiver.recv() {
                Ok(Ok(Some(chunk))) => self.decoder.push(&chunk.bytes[..chunk.len]),
                Ok(Ok(None)) => self.eof = true,
                Ok(Err(error)) => return Err(FrameError::Io(error)),
                Err(_) => return Err(FrameError::Io(std::io::Error::other("reader is closed"))),
            }
        }
    }

    pub fn try_with_next_frame<T>(
        &mut self,
        callback: impl FnOnce(&[u8]) -> T,
    ) -> Result<FramePoll<T>, FrameError> {
        loop {
            if let Some(body) = self.decoder.next_frame()? {
                return Ok(FramePoll::Frame(callback(body)));
            }
            if self.eof {
                return Ok(FramePoll::End);
            }
            match self.receiver.try_recv() {
                Ok(Ok(Some(chunk))) => self.decoder.push(&chunk.bytes[..chunk.len]),
                Ok(Ok(None)) => self.eof = true,
                Ok(Err(error)) => return Err(FrameError::Io(error)),
                Err(TryRecvError::Empty) => return Ok(FramePoll::Empty),
                Err(TryRecvError::Disconnected) => {
                    return Err(FrameError::Io(std::io::Error::other("reader is closed")))
                }
            }
        }
    }

    pub fn recv_timeout_with_frame<T>(
        &mut self,
        timeout: Duration,
        callback: impl FnOnce(&[u8]) -> T,
    ) -> Result<FramePoll<T>, FrameError> {
        loop {
            if let Some(body) = self.decoder.next_frame()? {
                return Ok(FramePoll::Frame(callback(body)));
            }
            if self.eof {
                return Ok(FramePoll::End);
            }
            match self.receiver.recv_timeout(timeout) {
                Ok(Ok(Some(chunk))) => self.decoder.push(&chunk.bytes[..chunk.len]),
                Ok(Ok(None)) => self.eof = true,
                Ok(Err(error)) => return Err(FrameError::Io(error)),
                Err(RecvTimeoutError::Timeout) => return Ok(FramePoll::Empty),
                Err(RecvTimeoutError::Disconnected) => {
                    return Err(FrameError::Io(std::io::Error::other("reader is closed")))
                }
            }
        }
    }
}

impl<R: Read> FrameReader<R> {
    pub fn new(reader: R, cap: usize) -> Self {
        Self {
            reader,
            decoder: FrameDecoder::new(cap),
        }
    }

    pub fn into_inner(self) -> R {
        self.reader
    }

    /// Reads the next body, returning `None` only at a clean frame boundary.
    pub fn read_frame(&mut self) -> Result<Option<Vec<u8>>, FrameError> {
        let mut bytes = [0u8; 8192];
        loop {
            if let Some(frame) = self.decoder.next_frame()? {
                return Ok(Some(frame.to_owned()));
            }

            let count = self.reader.read(&mut bytes).map_err(FrameError::Io)?;

            if count == 0 {
                if self.decoder.has_pending_bytes() {
                    return Err(FrameError::Malformed("unexpected EOF".to_string()));
                }
                return Ok(None);
            }
            self.decoder.push(&bytes[..count]);
        }
    }
}

pub fn encode_frame(body: &[u8]) -> Vec<u8> {
    let mut frame = format!("Content-Length: {}\r\n\r\n", body.len()).into_bytes();
    frame.extend_from_slice(body);
    frame
}

pub fn write_frame<W: Write>(writer: &mut W, body: &[u8], cap: usize) -> Result<(), FrameError> {
    if body.len() > cap {
        return Err(FrameError::Oversized(body.len()));
    }
    let header = format!("Content-Length: {}\r\n\r\n", body.len());
    writer
        .write_all(header.as_bytes())
        .map_err(FrameError::Io)?;
    writer.write_all(body).map_err(FrameError::Io)
}

pub fn parse_json_object(body: &[u8], protocol: &str) -> Result<Value, String> {
    let value: Value = crate::json::from_slice(body).map_err(|error| {
        if protocol.is_empty() {
            format!("invalid JSON: {error}")
        } else {
            format!("invalid {protocol} JSON: {error}")
        }
    })?;
    if !value.is_object() {
        return Err(format!("{protocol} message is not an object"));
    }
    Ok(value)
}

pub fn write_json<W: Write>(
    writer: &mut W,
    message: &Value,
    cap: usize,
    flush: bool,
) -> Result<(), FrameError> {
    let body = crate::json::to_vec(message);
    write_frame(writer, &body, cap)?;
    if flush {
        writer.flush().map_err(FrameError::Io)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn split_frame_across_pushes() {
        let mut decoder = FrameDecoder::new(64);
        decoder.push(b"Content-Length: 5\r\n");
        assert!(decoder.next_frame().unwrap().is_none());
        decoder.push(b"\r\nhello");
        assert_eq!(
            decoder.next_frame().unwrap().map(|body| body.to_vec()),
            Some(b"hello".to_vec())
        );
    }

    #[test]
    fn two_messages_in_one_read() {
        let mut decoder = FrameDecoder::new(64);
        decoder.push(b"Content-Length: 1\n\naContent-Length: 1\r\n\nb");
        assert_eq!(
            decoder.next_frame().unwrap().map(|body| body.to_vec()),
            Some(b"a".to_vec())
        );
        assert_eq!(
            decoder.next_frame().unwrap().map(|body| body.to_vec()),
            Some(b"b".to_vec())
        );
    }

    #[test]
    fn oversized_body() {
        let mut decoder = FrameDecoder::new(3);
        decoder.push(b"Content-Length: 4\r\n\r\n");
        assert!(matches!(
            decoder.next_frame(),
            Err(FrameError::Oversized(4))
        ));
    }

    #[test]
    fn malformed_header() {
        let mut decoder = FrameDecoder::new(64);
        decoder.push(b"Content-Length 4\r\n\r\nbody");
        assert!(matches!(
            decoder.next_frame(),
            Err(FrameError::Malformed(_))
        ));
    }

    #[test]
    fn missing_content_length() {
        let mut decoder = FrameDecoder::new(64);
        decoder.push(b"Content-Type: application/json\r\n\r\nbody");
        assert!(matches!(
            decoder.next_frame(),
            Err(FrameError::Malformed(_))
        ));
    }

    #[test]
    fn eof_mid_frame() {
        let mut reader = FrameReader::new(Cursor::new(b"Content-Length: 4\r\n\r\nabc"), 64);
        assert!(matches!(reader.read_frame(), Err(FrameError::Malformed(_))));
    }

    #[test]
    fn consumed_prefix_is_compacted_after_threshold() {
        let frame = encode_frame(b"x");
        let mut decoder = FrameDecoder::new(64);
        decoder.push(&frame);
        assert_eq!(decoder.next_frame().unwrap(), Some(&b"x"[..]));
        assert!(decoder.consumed < COMPACT_THRESHOLD);

        let frames = frame.repeat(COMPACT_THRESHOLD / frame.len() + 1);
        decoder.push(&frames);
        let mut count = 0;
        while decoder.next_frame().unwrap().is_some() {
            count += 1;
        }
        assert!(count > COMPACT_THRESHOLD / frame.len());
        assert!(decoder.consumed < COMPACT_THRESHOLD);
    }

    #[test]
    fn proxy_forwarding_benchmark() {
        let frames = [
            (
                "didChange",
                br#"{"jsonrpc":"2.0","method":"textDocument/didChange","params":{"textDocument":{"uri":"file:///tmp/main.gd","version":2},"contentChanges":[{"text":"extends Node\n"}]}}"#
                    .as_slice(),
            ),
            (
                "publishDiagnostics",
                br#"{"jsonrpc":"2.0","method":"textDocument/publishDiagnostics","params":{"uri":"file:///tmp/main.gd","diagnostics":[]}}"#
                    .as_slice(),
            ),
            (
                "completionResponse",
                br#"{"jsonrpc":"2.0","id":17,"result":{"isIncomplete":false,"items":[{"label":"ready","kind":3}]}}"#
                    .as_slice(),
            ),
        ];
        for (name, body) in frames {
            let mut output = Vec::new();
            let mut checksum = 0usize;
            let start = std::time::Instant::now();
            for _ in 0..10_000 {
                let value = parse_json_object(body, "LSP").unwrap();
                let serialized = crate::json::to_vec(&value);
                write_frame(&mut output, &serialized, 64 * 1024 * 1024).unwrap();
                checksum = checksum.wrapping_add(output.len());
                output.clear();
            }
            let reference = start.elapsed().as_nanos() / 10_000;
            let start = std::time::Instant::now();
            for _ in 0..10_000 {
                crate::json::scan_top_level(body).unwrap();
                write_frame(&mut output, body, 64 * 1024 * 1024).unwrap();
                checksum = checksum.wrapping_add(output.len());
                output.clear();
            }
            let optimized = start.elapsed().as_nanos() / 10_000;
            println!("proxy {name}: reference={reference} ns/frame optimized={optimized} ns/frame");
            std::hint::black_box(checksum);
            assert_ne!(checksum, 0);
        }
    }

    #[test]
    fn single_header_output_is_exact() {
        assert_eq!(encode_frame(b"hello"), b"Content-Length: 5\r\n\r\nhello");
    }
}
