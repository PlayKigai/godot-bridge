use std::fmt;

use crate::json::Value;
use std::io::{self, IoSlice, Read, Write};
use std::sync::mpsc::SyncSender;
use std::thread::{self, JoinHandle};
use std::time::Duration;

const HEADER_CAP: usize = 16 * 1024;
const COMPACT_THRESHOLD: usize = 64 * 1024;
const READ_CHUNK_SIZE: usize = 64 * 1024;

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
    header_scan: usize,
    content_length: Option<usize>,
    body_start: Option<usize>,
}

impl FrameDecoder {
    pub fn new(cap: usize) -> Self {
        Self {
            cap,
            buf: Vec::new(),
            consumed: 0,
            header_scan: 0,
            content_length: None,
            body_start: None,
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
        if self.body_start.is_none() {
            let scan_limit = input.len().min(HEADER_CAP + 1);
            let mut delimiter = None;
            while self.header_scan < scan_limit {
                let index = self.header_scan;
                if input.get(index..index + 4) == Some(b"\r\n\r\n") {
                    delimiter = Some((index, 4));
                    break;
                }
                if input.get(index..index + 2) == Some(b"\n\n") {
                    delimiter = Some((index, 2));
                    break;
                }
                self.header_scan += 1;
            }
            let Some((header_end, delimiter_len)) = delimiter else {
                self.header_scan = input.len().saturating_sub(3);
                if input.len() > HEADER_CAP {
                    return Err(FrameError::Malformed("frame header is too long".to_owned()));
                }
                return Ok(None);
            };
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

            let length = content_length.ok_or_else(|| {
                FrameError::Malformed("missing Content-Length header".to_string())
            })?;
            if length > self.cap {
                return Err(FrameError::Oversized(length));
            }
            self.content_length = Some(length);
            self.body_start = Some(header_end + delimiter_len);
        }

        let length = self.content_length.expect("parsed frame length");
        let body_start = self.body_start.expect("parsed frame body start");
        let body_end = body_start
            .checked_add(length)
            .ok_or(FrameError::Oversized(usize::MAX))?;
        if input.len() < body_end {
            return Ok(None);
        }

        let body_start = self.consumed + body_start;
        let body_end = self.consumed + body_end;
        self.consumed = body_end;
        self.header_scan = 0;
        self.content_length = None;
        self.body_start = None;
        Ok(Some(&self.buf[body_start..body_end]))
    }

    pub(crate) fn has_pending_bytes(&self) -> bool {
        self.consumed < self.buf.len()
    }

    fn compact(&mut self) {
        if self.consumed == self.buf.len() {
            self.buf.clear();
            if self.buf.capacity() > 1024 * 1024 {
                self.buf.shrink_to(2 * READ_CHUNK_SIZE);
            }
        } else {
            self.buf.copy_within(self.consumed.., 0);
            self.buf.truncate(self.buf.len() - self.consumed);
        }
        self.consumed = 0;
    }
}

pub(crate) type ReadEvent = Result<Option<Vec<u8>>, std::io::Error>;

pub(crate) struct Connection {
    pub(crate) socket: std::net::TcpStream,
    pub(crate) reader_thread: Option<JoinHandle<()>>,
    pub(crate) writer: std::net::TcpStream,
}

impl Connection {
    pub(crate) fn close(&mut self) {
        let _ = self.socket.shutdown(std::net::Shutdown::Both);
        if let Some(reader_thread) = self.reader_thread.take() {
            let _ = reader_thread.join();
        }
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        self.close();
    }
}

pub(crate) fn connect_with_reader<T, F>(
    stream: std::net::TcpStream,
    name: &str,
    sender: SyncSender<T>,
    map: F,
) -> std::io::Result<Connection>
where
    T: Send + 'static,
    F: Fn(ReadEvent) -> T + Send + 'static,
{
    stream.set_write_timeout(Some(Duration::from_secs(10)))?;
    let reader_stream = stream.try_clone()?;
    let writer = stream.try_clone()?;
    let reader_thread = spawn_frame_reader(name, reader_stream, sender, map)?;
    Ok(Connection {
        socket: stream,
        reader_thread: Some(reader_thread),
        writer,
    })
}

pub(crate) fn spawn_frame_reader<R, T, F>(
    name: &str,
    mut reader: R,
    sender: SyncSender<T>,
    map: F,
) -> std::io::Result<JoinHandle<()>>
where
    R: Read + Send + 'static,
    T: Send + 'static,
    F: Fn(ReadEvent) -> T + Send + 'static,
{
    let name = name.to_owned();
    thread::Builder::new()
        .name(name)
        .stack_size(256 * 1024)
        .spawn(move || {
            let mut chunk = vec![0; READ_CHUNK_SIZE];
            loop {
                let event = match reader.read(&mut chunk) {
                    Ok(0) => Ok(None),
                    Ok(len) => Ok(Some(chunk[..len].to_vec())),
                    Err(error) => Err(error),
                };
                let done = matches!(&event, Ok(None) | Err(_));
                if sender.send(map(event)).is_err() {
                    return;
                }
                if done {
                    return;
                }
            }
        })
}

pub fn write_frame<W: Write>(writer: &mut W, body: &[u8], cap: usize) -> Result<(), FrameError> {
    if body.len() > cap {
        return Err(FrameError::Oversized(body.len()));
    }
    let mut header = [0u8; 64];
    let mut header_writer = io::Cursor::new(&mut header[..]);
    write!(header_writer, "Content-Length: {}\r\n\r\n", body.len()).map_err(FrameError::Io)?;
    let header_len = header_writer.position() as usize;
    let mut header_offset = 0;
    let mut body_offset = 0;
    while header_offset < header_len || body_offset < body.len() {
        let header_remaining = header_len - header_offset;
        let written = if header_remaining != 0 {
            let slices = [
                IoSlice::new(&header[header_offset..header_len]),
                IoSlice::new(&body[body_offset..]),
            ];
            writer.write_vectored(&slices).map_err(FrameError::Io)?
        } else {
            writer.write(&body[body_offset..]).map_err(FrameError::Io)?
        };
        if written == 0 {
            return Err(FrameError::Io(io::Error::new(
                io::ErrorKind::WriteZero,
                "failed to write frame",
            )));
        }
        if written <= header_remaining {
            header_offset += written;
        } else {
            header_offset = header_len;
            body_offset += written - header_remaining;
        }
    }
    Ok(())
}

pub fn parse_json_object(body: &[u8], protocol: &str) -> Result<Value, String> {
    let value: Value = crate::json::from_slice(body)
        .map_err(|error| format!("invalid {protocol} JSON: {error}"))?;
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
    fn consumed_prefix_is_compacted_after_threshold() {
        let frame = frame_for_test(b"x");
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
        use std::net::TcpListener;
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
            let benchmark = |optimized| {
                let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
                let address = listener.local_addr().unwrap();
                let drain = thread::spawn(move || {
                    let (mut stream, _) = listener.accept().unwrap();
                    let mut bytes = [0; 16 * 1024];
                    while stream.read(&mut bytes).unwrap_or(0) != 0 {}
                });
                let mut writer = std::net::TcpStream::connect(address).unwrap();
                let (sender, receiver) = std::sync::mpsc::sync_channel::<ReadEvent>(1);
                let start = std::time::Instant::now();
                let mut checksum = 0usize;
                for _ in 0..10_000 {
                    sender.send(Ok(Some(body.to_vec()))).unwrap();
                    let body = receiver.recv().unwrap().unwrap().unwrap();
                    let body = if optimized {
                        crate::json::scan_top_level(&body).unwrap();
                        body
                    } else {
                        let value = parse_json_object(&body, "LSP").unwrap();
                        crate::json::to_vec(&value)
                    };
                    write_frame(&mut writer, &body, 8 * 1024 * 1024).unwrap();
                    checksum = checksum.wrapping_add(body.len());
                }
                drop(writer);
                drain.join().unwrap();
                (start.elapsed().as_nanos() / 10_000, checksum)
            };
            let (reference, first_checksum) = benchmark(false);
            let (optimized, second_checksum) = benchmark(true);
            if std::env::var_os("GODOT_BRIDGE_BENCH").is_some() {
                println!(
                    "proxy {name}: reference={reference} ns/frame optimized={optimized} ns/frame"
                );
            }
            assert_ne!(first_checksum, 0);
            assert_eq!(first_checksum, second_checksum);
        }
    }

    #[test]
    fn single_header_output_is_exact() {
        assert_eq!(frame_for_test(b"hello"), b"Content-Length: 5\r\n\r\nhello");
    }

    fn frame_for_test(body: &[u8]) -> Vec<u8> {
        let mut frame = Vec::new();
        write_frame(&mut frame, body, 8 * 1024 * 1024).unwrap();
        frame
    }
}
