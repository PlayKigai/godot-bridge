//! Content-Length message framing.

use std::fmt;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Errors returned while decoding or writing a framed message.
#[derive(Debug)]
pub enum FrameError {
    /// The declared body length is larger than the configured cap.
    Oversized(usize),
    /// The frame headers are not valid.
    Malformed(String),
    /// The underlying asynchronous I/O operation failed.
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

/// Incrementally decodes Content-Length framed messages.
pub struct FrameDecoder {
    cap: usize,
    buf: Vec<u8>,
}

impl FrameDecoder {
    /// Creates a decoder which rejects bodies larger than `cap` bytes.
    pub fn new(cap: usize) -> Self {
        Self {
            cap,
            buf: Vec::new(),
        }
    }

    /// Adds bytes to the decoder's input buffer.
    pub fn push(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }

    /// Returns the next complete body, if one is available.
    pub fn next_frame(&mut self) -> Result<Option<Vec<u8>>, FrameError> {
        let (header_end, delimiter_len) = match (
            self.buf.windows(4).position(|window| window == b"\r\n\r\n"),
            self.buf.windows(2).position(|window| window == b"\n\n"),
        ) {
            (Some(crlf_end), Some(lf_end)) if lf_end < crlf_end => (lf_end, 2),
            (Some(crlf_end), _) => (crlf_end, 4),
            (None, Some(lf_end)) => (lf_end, 2),
            (None, None) => return Ok(None),
        };

        let header = &self.buf[..header_end];
        let mut content_length = None;
        for line in header.split(|byte| *byte == b'\n') {
            let line = line.strip_suffix(b"\r").unwrap_or(line);
            let Some(separator) = line.iter().position(|byte| *byte == b':') else {
                return Err(FrameError::Malformed(
                    "header line has no colon".to_string(),
                ));
            };
            let name = &line[..separator];
            let value = line[separator + 1..]
                .iter()
                .copied()
                .skip_while(u8::is_ascii_whitespace)
                .collect::<Vec<_>>();
            if name.eq_ignore_ascii_case(b"Content-Length") {
                if content_length.is_some() {
                    return Err(FrameError::Malformed(
                        "duplicate Content-Length header".to_string(),
                    ));
                }
                let value = std::str::from_utf8(&value).map_err(|_| {
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
        if self.buf.len() < body_end {
            return Ok(None);
        }

        let body = self.buf[body_start..body_end].to_vec();
        self.buf.drain(..body_end);
        Ok(Some(body))
    }

    fn has_pending_bytes(&self) -> bool {
        !self.buf.is_empty()
    }
}

/// Reads Content-Length framed messages from an asynchronous reader.
pub struct FrameReader<R: AsyncRead> {
    reader: R,
    decoder: FrameDecoder,
}

impl<R: AsyncRead + Unpin> FrameReader<R> {
    /// Creates a reader which rejects bodies larger than `cap` bytes.
    pub fn new(reader: R, cap: usize) -> Self {
        Self {
            reader,
            decoder: FrameDecoder::new(cap),
        }
    }

    /// Reads the next body, returning `None` only at a clean frame boundary.
    pub async fn read_frame(&mut self) -> Result<Option<Vec<u8>>, FrameError> {
        let mut bytes = [0u8; 8192];
        loop {
            if let Some(frame) = self.decoder.next_frame()? {
                return Ok(Some(frame));
            }

            let count = self.reader.read(&mut bytes).await.map_err(FrameError::Io)?;
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

/// Encodes a body using the standard single-header LSP frame format.
pub fn encode_frame(body: &[u8]) -> Vec<u8> {
    let mut frame = format!("Content-Length: {}\r\n\r\n", body.len()).into_bytes();
    frame.extend_from_slice(body);
    frame
}

/// Writes one framed body, refusing bodies larger than `cap` bytes.
pub async fn write_frame<W: AsyncWrite + Unpin>(
    writer: &mut W,
    body: &[u8],
    cap: usize,
) -> Result<(), FrameError> {
    if body.len() > cap {
        return Err(FrameError::Oversized(body.len()));
    }
    writer
        .write_all(&encode_frame(body))
        .await
        .map_err(FrameError::Io)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{duplex, AsyncWriteExt};

    #[test]
    fn split_frame_across_pushes() {
        let mut decoder = FrameDecoder::new(64);
        decoder.push(b"Content-Length: 5\r\n");
        assert!(decoder.next_frame().unwrap().is_none());
        decoder.push(b"\r\nhello");
        assert_eq!(decoder.next_frame().unwrap(), Some(b"hello".to_vec()));
    }

    #[test]
    fn two_messages_in_one_read() {
        let mut decoder = FrameDecoder::new(64);
        decoder.push(b"Content-Length: 1\n\naContent-Length: 1\r\n\nb");
        assert_eq!(decoder.next_frame().unwrap(), Some(b"a".to_vec()));
        assert_eq!(decoder.next_frame().unwrap(), Some(b"b".to_vec()));
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

    #[tokio::test]
    async fn eof_mid_frame() {
        let (mut writer, reader) = duplex(64);
        writer
            .write_all(b"Content-Length: 4\r\n\r\nabc")
            .await
            .unwrap();
        writer.shutdown().await.unwrap();
        let mut reader = FrameReader::new(reader, 64);
        assert!(matches!(
            reader.read_frame().await,
            Err(FrameError::Malformed(_))
        ));
    }

    #[test]
    fn single_header_output_is_exact() {
        assert_eq!(encode_frame(b"hello"), b"Content-Length: 5\r\n\r\nhello");
    }
}
