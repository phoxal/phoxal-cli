//! Pure length-prefixed JSON framing.

use anyhow::{Context, Result};
use serde::Serialize;
use serde::de::DeserializeOwned;

pub mod async_io;
pub mod blocking_io;

pub fn encode_payload<T: Serialize>(value: &T, maximum: usize) -> Result<Vec<u8>> {
    let bytes = serde_json::to_vec(value).context("encode protocol payload")?;
    anyhow::ensure!(
        bytes.len() <= maximum,
        "encoded protocol payload is {} bytes; limit is {maximum}",
        bytes.len()
    );
    Ok(bytes)
}

pub fn decode_payload<T: DeserializeOwned>(bytes: &[u8], maximum: usize) -> Result<T> {
    anyhow::ensure!(
        bytes.len() <= maximum,
        "protocol payload is {} bytes; limit is {maximum}",
        bytes.len()
    );
    serde_json::from_slice(bytes).context("decode protocol payload")
}

pub fn encode_frame<T: Serialize>(value: &T, maximum: usize) -> Result<Vec<u8>> {
    let payload = encode_payload(value, maximum)?;
    let length = u32::try_from(payload.len()).context("protocol frame length exceeds u32")?;
    let mut frame = Vec::with_capacity(4 + payload.len());
    frame.extend_from_slice(&length.to_be_bytes());
    frame.extend_from_slice(&payload);
    Ok(frame)
}

pub fn decode_frame<T: DeserializeOwned>(frame: &[u8], maximum: usize) -> Result<T> {
    anyhow::ensure!(
        frame.len() >= 4,
        "protocol frame is missing its length header"
    );
    let length = decode_length(frame[..4].try_into().expect("four-byte slice"), maximum)?;
    anyhow::ensure!(
        frame.len() == 4 + length,
        "protocol frame declares {length} bytes but contains {}",
        frame.len().saturating_sub(4)
    );
    decode_payload(&frame[4..], maximum)
}

pub(crate) fn decode_length(header: [u8; 4], maximum: usize) -> Result<usize> {
    let length = u32::from_be_bytes(header) as usize;
    anyhow::ensure!(
        length <= maximum,
        "protocol frame declares {length} bytes; limit is {maximum}"
    );
    Ok(length)
}
