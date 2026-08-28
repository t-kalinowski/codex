use serde::Serialize;
use serde::de::DeserializeOwned;
use std::io;
use tokio::io::AsyncRead;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWrite;
use tokio::io::AsyncWriteExt;

use crate::MAX_FRAME_SIZE;

#[derive(Debug, thiserror::Error)]
pub enum FrameError {
    #[error("control channel closed")]
    Closed,
    #[error("truncated control frame length")]
    TruncatedLength,
    #[error("control frame length {length} exceeds the {maximum}-byte limit")]
    Oversized { length: usize, maximum: usize },
    #[error("truncated control frame payload: expected {expected} bytes, received {received}")]
    TruncatedPayload { expected: usize, received: usize },
    #[error("malformed JSON control frame: {0}")]
    MalformedJson(#[from] serde_json::Error),
    #[error("control channel I/O failed: {0}")]
    Io(#[source] io::Error),
}

pub async fn read_frame<R, T>(reader: &mut R) -> Result<T, FrameError>
where
    R: AsyncRead + Unpin,
    T: DeserializeOwned,
{
    let mut length = [0_u8; 4];
    let mut read = 0;
    while read < length.len() {
        match reader.read(&mut length[read..]).await {
            Ok(0) if read == 0 => return Err(FrameError::Closed),
            Ok(0) => return Err(FrameError::TruncatedLength),
            Ok(count) => read += count,
            Err(source) => return Err(FrameError::Io(source)),
        }
    }
    let length = u32::from_be_bytes(length) as usize;
    if length > MAX_FRAME_SIZE {
        return Err(FrameError::Oversized {
            length,
            maximum: MAX_FRAME_SIZE,
        });
    }
    let mut payload = vec![0_u8; length];
    let mut read = 0;
    while read < payload.len() {
        match reader.read(&mut payload[read..]).await {
            Ok(0) => {
                return Err(FrameError::TruncatedPayload {
                    expected: length,
                    received: read,
                });
            }
            Ok(count) => read += count,
            Err(source) => return Err(FrameError::Io(source)),
        }
    }
    serde_json::from_slice(&payload).map_err(FrameError::MalformedJson)
}

pub async fn write_frame<W, T>(writer: &mut W, value: &T) -> Result<(), FrameError>
where
    W: AsyncWrite + Unpin,
    T: Serialize,
{
    let payload = serde_json::to_vec(value).map_err(FrameError::MalformedJson)?;
    if payload.len() > MAX_FRAME_SIZE {
        return Err(FrameError::Oversized {
            length: payload.len(),
            maximum: MAX_FRAME_SIZE,
        });
    }
    let length = u32::try_from(payload.len()).map_err(|_| FrameError::Oversized {
        length: payload.len(),
        maximum: MAX_FRAME_SIZE,
    })?;
    writer
        .write_all(&length.to_be_bytes())
        .await
        .map_err(FrameError::Io)?;
    writer.write_all(&payload).await.map_err(FrameError::Io)?;
    writer.flush().await.map_err(FrameError::Io)
}
