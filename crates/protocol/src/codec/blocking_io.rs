//! Blocking I/O adapters for the pure protocol codec.
//!
//! Generic [`Read`] and [`Write`] do not expose deadline setters. Callers
//! therefore configure OS read/write deadlines on their stream before invoking
//! these adapters; the resident bootstrap does this on both socketpair ends.
//! Framing, length validation, and decoding remain shared with async I/O.

use std::io::{Read, Write};

use anyhow::Result;
use serde::Serialize;
use serde::de::DeserializeOwned;

use super::{decode_length, decode_payload, encode_frame};

pub fn write_frame<W: Write, T: Serialize>(
    writer: &mut W,
    value: &T,
    maximum: usize,
) -> Result<()> {
    let frame = encode_frame(value, maximum)?;
    writer.write_all(&frame)?;
    writer.flush()?;
    Ok(())
}

pub fn read_frame<R: Read, T: DeserializeOwned>(reader: &mut R, maximum: usize) -> Result<T> {
    let mut header = [0_u8; 4];
    reader.read_exact(&mut header)?;
    let length = decode_length(header, maximum)?;
    let mut payload = vec![0; length];
    reader.read_exact(&mut payload)?;
    decode_payload(&payload, maximum)
}
