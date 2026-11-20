//! The UI websocket's frames, with a byte-to-character reader
//! and a character-count header before UTF-8 encoding. A standards-normalizing
//! WebSocket implementation changes both of those observable behaviors.

use std::io;
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

// The write-readiness timeout: the 1000 of the timeout setting, taken as seconds.
pub(super) const WRITE_READY_TIMEOUT: Duration = Duration::from_secs(1000);

pub(super) struct Socket<S> {
    stream: S,
    input: Vec<u8>,
    fragments: String,
    eof: bool,
}

impl<S: AsyncRead + AsyncWrite + Unpin> Socket<S> {
    pub(super) fn new(stream: S) -> Self {
        Self { stream, input: Vec::new(), fragments: String::new(), eof: false }
    }

    /// Reads the next message: each payload byte becomes one character, fragments are
    /// concatenated until FIN, and opcode/RSV are ignored. URL unquoting follows in
    /// client::parse_incoming. Keep read state here across select cancellation.
    pub(super) async fn recv(&mut self) -> io::Result<Option<String>> {
        loop {
            match self.frame()? {
                Some((finished, payload, consumed)) => {
                    self.input.drain(..consumed);
                    self.fragments.extend(payload.into_iter().map(char::from));
                    if finished {
                        return Ok(Some(std::mem::take(&mut self.fragments)));
                    }
                }
                None if self.eof => return Ok(None),
                None => {
                    let mut bytes = [0u8; 8192];
                    let count = self.stream.read(&mut bytes).await?;
                    self.input.extend_from_slice(&bytes[..count]);
                    self.eof = count == 0;
                }
            }
        }
    }

    fn frame(&self) -> io::Result<Option<(bool, Vec<u8>, usize)>> {
        let bytes = &self.input;
        if bytes.len() < 2 {
            return Ok(None);
        }
        let finished = bytes[0] & 0x80 != 0;
        let masked = bytes[1] & 0x80 != 0;
        let mut cursor = 2;
        let short_length = bytes[1] & 0x7f;
        let extra = match short_length {
            126 => 2,
            127 => 8,
            _ => 0,
        };
        if bytes.len() < cursor + extra {
            return if self.eof { Err(invalid_frame("incomplete extended length")) } else { Ok(None) };
        }
        let length = if extra == 0 {
            u64::from(short_length)
        } else {
            let length = bytes[cursor..cursor + extra].iter().fold(0u64, |n, byte| (n << 8) | u64::from(*byte));
            cursor += extra;
            length
        };
        // The read length is a signed pointer-sized integer.
        if length > isize::MAX as u64 {
            return Err(invalid_frame("frame length exceeds signed platform size"));
        }
        let mask_end = cursor + if masked { 4 } else { 0 };
        if bytes.len() < mask_end && !self.eof {
            return Ok(None);
        }
        let mask = &bytes[cursor..mask_end.min(bytes.len())];
        cursor = mask_end.min(bytes.len());
        let available = bytes.len() - cursor;
        if available < length as usize && !self.eof {
            return Ok(None);
        }
        // At EOF the read returns the remaining bytes. The message handler is still
        // invoked for a truncated final frame (including no bytes).
        let payload_length = available.min(length as usize);
        let mut payload = bytes[cursor..cursor + payload_length].to_vec();
        if masked {
            for (index, byte) in payload.iter_mut().enumerate() {
                let Some(mask) = mask.get(index % 4) else { return Err(invalid_frame("incomplete mask")) };
                *byte ^= mask;
            }
        }
        Ok(Some((finished, payload, cursor + payload_length)))
    }

    pub(super) async fn send_text(&mut self, message: &str) -> io::Result<bool> {
        self.send_with_timeout(message, WRITE_READY_TIMEOUT).await
    }

    pub(super) async fn send_with_timeout(&mut self, message: &str, timeout: Duration) -> io::Result<bool> {
        let frame = text_frame(message)?;
        // Only readiness has a timeout. Once writable, the send has no
        // timeout; keep that boundary for a partially accepted write.
        let first = match tokio::time::timeout(timeout, self.stream.write(&frame)).await {
            Ok(result) => result?,
            Err(_) => return Ok(false),
        };
        if first == 0 {
            return Err(io::Error::new(io::ErrorKind::WriteZero, "websocket write returned zero"));
        }
        self.stream.write_all(&frame[first..]).await?;
        Ok(true)
    }
}

fn invalid_frame(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

fn text_frame(message: &str) -> io::Result<Vec<u8>> {
    if message.is_empty() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "string index out of range"));
    }
    let length = message.chars().count() as u64;
    let mut frame = Vec::with_capacity(message.len() + 10);
    frame.push(0x81);
    if length <= 125 {
        frame.push(length as u8);
    } else if length <= 65535 {
        frame.push(126);
        frame.extend_from_slice(&(length as u16).to_be_bytes());
    } else {
        frame.push(127);
        frame.extend_from_slice(&length.to_be_bytes());
    }
    frame.extend_from_slice(message.as_bytes());
    Ok(frame)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn interrupted_receive_retains_fragment_and_partial_header_state() {
        let (stream, mut peer) = tokio::io::duplex(64);
        let mut socket = Socket::new(stream);
        peer.write_all(&[0x01, 2, b'a', b'b', 0x80]).await.unwrap();
        assert!(tokio::time::timeout(Duration::from_millis(5), socket.recv()).await.is_err());
        peer.write_all(&[2, b'c', b'd']).await.unwrap();
        assert_eq!(socket.recv().await.unwrap().as_deref(), Some("abcd"));
    }

    #[tokio::test]
    async fn unwritable_timeout_returns_false_but_a_partial_write_keeps_waiting() {
        let (stream, mut peer) = tokio::io::duplex(1);
        let mut socket = Socket::new(stream);
        // First byte fits, so the send continues beyond the readiness timeout.
        let mut send = Box::pin(socket.send_with_timeout("hello", Duration::from_millis(2)));
        assert!(tokio::time::timeout(Duration::from_millis(10), &mut send).await.is_err());
        let mut frame = [0; 7];
        let (sent, read) = tokio::join!(&mut send, peer.read_exact(&mut frame));
        assert!(sent.unwrap());
        read.unwrap();
        assert_eq!(frame, *b"\x81\x05hello");
        drop(send);
        socket.stream.write_all(&[0]).await.unwrap();
        assert!(!socket.send_with_timeout("3", Duration::from_millis(2)).await.unwrap());
    }
}
