//! Length-prefixed postcard framing.
//!
//! Shared by every protocol, between machines over QUIC and locally over
//! the control socket: a message is a little-endian `u32` size followed by
//! its postcard encoding. Callers pass the size they are willing to
//! accept, because on an unauthenticated stream the prefix is written by
//! whoever is on the other end.

use color_eyre::eyre::{Result, WrapErr as _, ensure};
use serde::{Serialize, de::DeserializeOwned};
use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _};

/// Writes a length-prefixed message.
pub async fn write_message<T: Serialize>(
    send: &mut (impl AsyncWrite + Unpin),
    message: &T,
    max_size: u32,
) -> Result<()> {
    let bytes = postcard::to_stdvec(message).wrap_err("cannot encode message")?;
    let size = u32::try_from(bytes.len()).wrap_err("message too large")?;
    ensure!(size <= max_size, "message too large");

    send.write_all(&size.to_le_bytes()).await?;
    send.write_all(&bytes).await?;

    Ok(())
}

/// Reads a length-prefixed message, refusing sizes over `max_size` before
/// allocating anything.
pub async fn read_message<T: DeserializeOwned>(
    recv: &mut (impl AsyncRead + Unpin),
    max_size: u32,
) -> Result<T> {
    let mut size = [0u8; 4];
    recv.read_exact(&mut size).await?;
    let size = u32::from_le_bytes(size);
    ensure!(size <= max_size, "message too large");

    let mut bytes = vec![0u8; size as usize];
    recv.read_exact(&mut bytes).await?;

    let (message, rest) = postcard::take_from_bytes(&bytes).wrap_err("cannot decode message")?;
    ensure!(rest.is_empty(), "cannot decode message: trailing bytes");

    Ok(message)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn messages_round_trip() {
        let (mut client, mut server) = tokio::io::duplex(1024);

        write_message(&mut client, &("hello".to_owned(), 42u32), 256)
            .await
            .unwrap();
        let (text, num): (String, u32) = read_message(&mut server, 256).await.unwrap();
        assert_eq!((text.as_str(), num), ("hello", 42));
    }

    #[tokio::test]
    async fn sizes_are_bounded_on_both_sides() {
        let (mut client, mut server) = tokio::io::duplex(1024);

        assert!(
            write_message(&mut client, &vec![0u8; 300], 256)
                .await
                .is_err()
        );

        // An oversized length prefix is refused before allocating for it.
        client.write_all(&u32::MAX.to_le_bytes()).await.unwrap();
        let result: Result<Vec<u8>> = read_message(&mut server, 256).await;
        assert!(result.unwrap_err().to_string().contains("too large"));
    }
}
