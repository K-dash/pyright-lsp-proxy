use crate::error::FramingError;
use crate::message::RpcMessage;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};

const CONTENT_LENGTH: &str = "Content-Length: ";

/// LSP フレームリーダー
pub struct LspFrameReader<R> {
    reader: BufReader<R>,
    debug: bool,
}

impl<R: AsyncRead + Unpin> LspFrameReader<R> {
    pub fn new(reader: R) -> Self {
        Self {
            reader: BufReader::new(reader),
            debug: false,
        }
    }

    /// 1つの LSP メッセージを読み取る
    pub async fn read_message(&mut self) -> Result<RpcMessage, FramingError> {
        // 1. ヘッダー部を読み取る
        let content_length = self.read_headers().await?;

        // 2. コンテンツ部を読み取る
        let mut content = vec![0u8; content_length];
        self.reader.read_exact(&mut content).await?;

        // デバッグ出力
        if self.debug {
            eprintln!("[DEBUG RX] {}", String::from_utf8_lossy(&content));
        }

        // 3. JSON としてパース
        let message: RpcMessage = serde_json::from_slice(&content)?;

        Ok(message)
    }

    async fn read_headers(&mut self) -> Result<usize, FramingError> {
        let mut content_length: Option<usize> = None;

        loop {
            let mut line = String::new();
            let bytes_read = self.reader.read_line(&mut line).await?;

            // EOF 検知（read_line が 0 を返す）
            if bytes_read == 0 {
                return Err(FramingError::Io(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "EOF while reading headers",
                )));
            }

            // 空行（\r\n のみ）はヘッダー終了
            if line == "\r\n" {
                break;
            }

            // Content-Length ヘッダーをパース
            let line = line.trim();
            if let Some(len_str) = line.strip_prefix(CONTENT_LENGTH) {
                content_length = Some(
                    len_str
                        .parse()
                        .map_err(|_| FramingError::InvalidContentLength)?,
                );
            }
            // Content-Type は無視（UTF-8 前提）
        }

        content_length.ok_or(FramingError::MissingContentLength)
    }
}

/// LSP フレームライター
pub struct LspFrameWriter<W> {
    writer: W,
    debug: bool,
}

impl<W: AsyncWrite + Unpin> LspFrameWriter<W> {
    pub fn new(writer: W) -> Self {
        Self {
            writer,
            debug: false,
        }
    }

    /// LSP メッセージを書き込む
    pub async fn write_message(&mut self, message: &RpcMessage) -> Result<(), FramingError> {
        let content = serde_json::to_vec(message)?;

        // デバッグ出力
        if self.debug {
            eprintln!("[DEBUG TX] {}", String::from_utf8_lossy(&content));
        }

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
        let msg = RpcMessage {
            jsonrpc: "2.0".to_string(),
            id: Some(crate::message::RpcId::Number(1)),
            method: Some("test".to_string()),
            params: None,
            result: None,
            error: None,
        };
        writer.write_message(&msg).await.unwrap();
        assert!(output.starts_with(b"Content-Length: "));
    }
}
