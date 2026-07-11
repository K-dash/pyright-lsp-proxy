use crate::error::FramingError;
use crate::message::RpcMessage;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};

const CONTENT_LENGTH: &str = "Content-Length: ";

/// Maximum accepted `Content-Length` (message body size). Generous enough
/// for large `workspace/symbol` fan-out merges, but bounded so a corrupt or
/// misbehaving backend declaring a huge size can't make the proxy commit an
/// unbounded allocation (#106). Checked before `vec![0u8; content_length]`
/// is ever created.
const MAX_CONTENT_LENGTH: usize = 64 * 1024 * 1024; // 64 MiB

/// Maximum length of a single header line (including its `\r\n`
/// terminator). Real LSP header lines (`Content-Length: N`, an optional
/// `Content-Type: ...`) are a few dozen bytes; this is generous headroom
/// while still bounding a peer that streams a line with no `\n` from
/// growing the line buffer without limit.
const MAX_HEADER_LINE_LEN: usize = 8 * 1024; // 8 KiB

/// Maximum total size of the header block (sum of all header line lengths
/// before the terminating blank line). LSP frames carry at most two headers;
/// this bounds a peer that never sends the terminating blank line from
/// making `read_headers` accumulate lines forever.
const MAX_HEADER_BLOCK_LEN: usize = 64 * 1024; // 64 KiB

/// LSP frame reader
pub struct LspFrameReader<R> {
    reader: BufReader<R>,
}

impl<R: AsyncRead + Unpin> LspFrameReader<R> {
    pub fn new(reader: R) -> Self {
        Self {
            reader: BufReader::new(reader),
        }
    }

    /// Read one LSP message
    pub async fn read_message(&mut self) -> Result<RpcMessage, FramingError> {
        // 1. Read header section
        let content_length = self.read_headers().await?;

        // 2. Read content section
        let mut content = vec![0u8; content_length];
        self.reader.read_exact(&mut content).await?;

        // 3. Parse as JSON
        let message: RpcMessage = serde_json::from_slice(&content)?;

        Ok(message)
    }

    async fn read_headers(&mut self) -> Result<usize, FramingError> {
        let mut content_length: Option<usize> = None;
        let mut total_header_bytes: usize = 0;

        loop {
            let line = self.read_header_line().await?;

            // Detect EOF (empty line at a line boundary)
            if line.is_empty() {
                return Err(FramingError::Io(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "EOF while reading headers",
                )));
            }

            total_header_bytes += line.len();
            if total_header_bytes > MAX_HEADER_BLOCK_LEN {
                return Err(FramingError::HeaderBlockTooLarge {
                    limit: MAX_HEADER_BLOCK_LEN,
                });
            }

            // Empty line (\r\n only) marks end of headers
            if line == "\r\n" {
                break;
            }

            // Parse Content-Length header
            let line = line.trim();
            if let Some(len_str) = line.strip_prefix(CONTENT_LENGTH) {
                if content_length.is_some() {
                    return Err(FramingError::DuplicateContentLength);
                }
                let parsed: usize = len_str
                    .parse()
                    .map_err(|_| FramingError::InvalidContentLength)?;
                if parsed > MAX_CONTENT_LENGTH {
                    return Err(FramingError::ContentLengthTooLarge {
                        limit: MAX_CONTENT_LENGTH,
                        actual: parsed,
                    });
                }
                content_length = Some(parsed);
            }
            // Ignore Content-Type (assume UTF-8)
        }

        content_length.ok_or(FramingError::MissingContentLength)
    }

    /// Read one header line, including its trailing `\n`, capped at
    /// `MAX_HEADER_LINE_LEN` bytes. Returns an empty string on EOF at a line
    /// boundary (mirrors `AsyncBufReadExt::read_line`'s contract, which the
    /// caller relies on to detect a closed backend/client stream).
    ///
    /// Implemented directly against `fill_buf`/`consume` rather than
    /// `read_line` because `read_line` has no size cap: a peer that streams
    /// bytes with no `\n` would make it grow the `String` without bound.
    /// Each `fill_buf` call returns at most the `BufReader`'s internal
    /// capacity worth of bytes, so the cap below is enforced before a chunk
    /// is appended, not after the line has already grown past it.
    async fn read_header_line(&mut self) -> Result<String, FramingError> {
        let mut line = Vec::new();
        loop {
            let available = self.reader.fill_buf().await?;
            if available.is_empty() {
                break; // EOF
            }

            let newline_pos = available.iter().position(|&b| b == b'\n');
            let chunk_len = newline_pos.map_or(available.len(), |pos| pos + 1);

            if line.len() + chunk_len > MAX_HEADER_LINE_LEN {
                return Err(FramingError::HeaderLineTooLong {
                    limit: MAX_HEADER_LINE_LEN,
                });
            }
            line.extend_from_slice(&available[..chunk_len]);
            self.reader.consume(chunk_len);

            if newline_pos.is_some() {
                break;
            }
        }

        String::from_utf8(line)
            .map_err(|e| FramingError::Io(std::io::Error::new(std::io::ErrorKind::InvalidData, e)))
    }
}

/// LSP frame writer
pub struct LspFrameWriter<W> {
    writer: W,
}

impl<W: AsyncWrite + Unpin> LspFrameWriter<W> {
    pub fn new(writer: W) -> Self {
        Self { writer }
    }

    /// Escape hatch to the underlying writer, e.g. for batching multiple
    /// pre-framed messages into a single `write_all` call (the E2E harness
    /// uses this to saturate the proxy's client input for fairness testing;
    /// `write_message` alone, with its own `write_all`/`write_all`/`flush`
    /// per call, can't sustain that under per-call `.await` scheduling
    /// gaps).
    pub fn get_mut(&mut self) -> &mut W {
        &mut self.writer
    }

    /// Write LSP message
    pub async fn write_message(&mut self, message: &RpcMessage) -> Result<(), FramingError> {
        let content = serde_json::to_vec(message)?;

        let header = format!("Content-Length: {}\r\n\r\n", content.len());

        self.writer.write_all(header.as_bytes()).await?;
        self.writer.write_all(&content).await?;
        self.writer.flush().await?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_read_message() {
        let input =
            b"Content-Length: 46\r\n\r\n{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\"}";
        let mut reader = LspFrameReader::new(&input[..]);
        let msg = reader.read_message().await.unwrap();
        assert_eq!(msg.method_name(), Some("initialize"));
        assert!(msg.is_request());
    }

    #[tokio::test]
    async fn test_write_message() {
        let mut output = Vec::new();
        let mut writer = LspFrameWriter::new(&mut output);
        let msg = RpcMessage::request(crate::message::RpcId::Number(1), "test", None);
        writer.write_message(&msg).await.unwrap();
        assert!(output.starts_with(b"Content-Length: "));
    }

    /// A `Content-Length` above the limit must be rejected before the body
    /// buffer is ever allocated. `usize::MAX` is deliberately extreme: if
    /// the check were bypassed, `vec![0u8; content_length]` would panic
    /// with a capacity overflow immediately rather than slowly exhausting
    /// memory — a fast, deterministic way to prove the check runs first.
    #[tokio::test]
    async fn test_oversized_content_length_rejected_without_allocation() {
        let input = format!("Content-Length: {}\r\n\r\n", usize::MAX);
        let mut reader = LspFrameReader::new(input.as_bytes());
        let err = reader.read_message().await.unwrap_err();
        match err {
            FramingError::ContentLengthTooLarge { limit, actual } => {
                assert_eq!(limit, MAX_CONTENT_LENGTH);
                assert_eq!(actual, usize::MAX);
            }
            other => panic!("expected ContentLengthTooLarge, got {other:?}"),
        }
    }

    /// A frame declaring exactly `MAX_CONTENT_LENGTH` — the boundary the
    /// oversized check must NOT reject — still parses successfully.
    #[tokio::test]
    async fn test_content_length_at_limit_still_parses() {
        let prefix = r#"{"jsonrpc":"2.0","params":{"pad":""#;
        let suffix = r#""}}"#;
        let pad_len = MAX_CONTENT_LENGTH - prefix.len() - suffix.len();
        let content = format!("{prefix}{}{suffix}", "a".repeat(pad_len));
        assert_eq!(content.len(), MAX_CONTENT_LENGTH);

        let mut input = format!("Content-Length: {MAX_CONTENT_LENGTH}\r\n\r\n").into_bytes();
        input.extend_from_slice(content.as_bytes());

        let mut reader = LspFrameReader::new(&input[..]);
        let msg = reader.read_message().await.unwrap();
        assert_eq!(msg.jsonrpc, "2.0");
    }

    /// A single header line far beyond `MAX_HEADER_LINE_LEN` is rejected —
    /// well over any plausible `BufReader` chunk size, so the cap fires
    /// regardless of internal buffering boundaries.
    #[tokio::test]
    async fn test_oversized_header_line_rejected() {
        let long_line = format!("X-Padding: {}", "a".repeat(MAX_HEADER_LINE_LEN * 2));
        let input = format!("{long_line}\r\nContent-Length: 5\r\n\r\nhello");
        let mut reader = LspFrameReader::new(input.as_bytes());
        let err = reader.read_message().await.unwrap_err();
        match err {
            FramingError::HeaderLineTooLong { limit } => assert_eq!(limit, MAX_HEADER_LINE_LEN),
            other => panic!("expected HeaderLineTooLong, got {other:?}"),
        }
    }

    /// Many header lines, each individually under `MAX_HEADER_LINE_LEN`, but
    /// whose total exceeds `MAX_HEADER_BLOCK_LEN`, are rejected — proves the
    /// total-block cap is enforced independently of the per-line cap.
    #[tokio::test]
    async fn test_oversized_header_block_rejected() {
        let line = format!("X-Pad: {}\r\n", "a".repeat(64));
        assert!(line.len() < MAX_HEADER_LINE_LEN);
        let count = MAX_HEADER_BLOCK_LEN / line.len() + 2;

        let mut input = String::with_capacity(line.len() * count);
        for _ in 0..count {
            input.push_str(&line);
        }

        let mut reader = LspFrameReader::new(input.as_bytes());
        let err = reader.read_message().await.unwrap_err();
        match err {
            FramingError::HeaderBlockTooLarge { limit } => assert_eq!(limit, MAX_HEADER_BLOCK_LEN),
            other => panic!("expected HeaderBlockTooLarge, got {other:?}"),
        }
    }

    /// A second `Content-Length` header is an explicit error, not a
    /// last-wins/first-wins implicit resolution.
    #[tokio::test]
    async fn test_duplicate_content_length_rejected() {
        let input = b"Content-Length: 10\r\nContent-Length: 20\r\n\r\n0123456789";
        let mut reader = LspFrameReader::new(&input[..]);
        let err = reader.read_message().await.unwrap_err();
        assert!(matches!(err, FramingError::DuplicateContentLength));
    }
}
