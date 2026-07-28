//! Tokio I/O adapters for the pure protocol codec.

use std::time::Duration;

use anyhow::{Context, Result};
use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use super::{decode_length, decode_payload, encode_frame};

pub async fn write_frame<W, T>(
    writer: &mut W,
    value: &T,
    maximum: usize,
    deadline: Duration,
) -> Result<()>
where
    W: AsyncWrite + Unpin,
    T: Serialize,
{
    let frame = encode_frame(value, maximum)?;
    tokio::time::timeout(deadline, async {
        writer.write_all(&frame).await?;
        writer.flush().await
    })
    .await
    .context("protocol frame write timed out")??;
    Ok(())
}

pub async fn read_frame<R, T>(reader: &mut R, maximum: usize, deadline: Duration) -> Result<T>
where
    R: AsyncRead + Unpin,
    T: DeserializeOwned,
{
    let mut header = [0_u8; 4];
    tokio::time::timeout(deadline, reader.read_exact(&mut header))
        .await
        .context("protocol frame length read timed out")??;
    read_body(reader, header, maximum, deadline).await
}

/// Wait indefinitely for an idle stream's first byte, then bound the partial
/// header and body reads.
pub async fn read_frame_after_idle<R, T>(
    reader: &mut R,
    maximum: usize,
    deadline: Duration,
) -> Result<T>
where
    R: AsyncRead + Unpin,
    T: DeserializeOwned,
{
    let mut header = [0_u8; 4];
    reader
        .read_exact(&mut header[..1])
        .await
        .context("read protocol frame first length byte")?;
    tokio::time::timeout(deadline, reader.read_exact(&mut header[1..]))
        .await
        .context("protocol frame length read timed out")??;
    read_body(reader, header, maximum, deadline).await
}

async fn read_body<R, T>(
    reader: &mut R,
    header: [u8; 4],
    maximum: usize,
    deadline: Duration,
) -> Result<T>
where
    R: AsyncRead + Unpin,
    T: DeserializeOwned,
{
    let length = decode_length(header, maximum)?;
    let mut payload = vec![0; length];
    tokio::time::timeout(deadline, reader.read_exact(&mut payload))
        .await
        .context("protocol frame body read timed out")??;
    decode_payload(&payload, maximum)
}
