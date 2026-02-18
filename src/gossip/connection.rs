//! Secure transport — SHS handshake + Box Stream over TCP.
//!
//! `SecureConnection` wraps a TCP stream with authenticated encryption.
//! After construction (via `connect` for outgoing or `accept` for incoming),
//! all data is encrypted with per-direction ChaCha20-Poly1305 keys derived
//! from the handshake.
//!
//! The handshake verifies both peers share the same network key and proves
//! their Ed25519 identity. After that, `send`/`recv` handle Box Stream
//! framing transparently. Connection closes with a goodbye frame.
//!
//! Used by gossip/client.rs (outgoing sync) and gossip/server.rs (incoming).

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use crate::crypto::box_stream::{BoxStreamReader, BoxStreamWriter};
use crate::crypto::handshake::{
    ClientHandshake, HandshakeOutcome, ServerHandshake,
};
use crate::error::{EgreError, Result};
use crate::identity::Identity;

const HELLO_SIZE: usize = 64;

/// An authenticated, encrypted connection.
pub struct SecureConnection {
    stream: TcpStream,
    writer: BoxStreamWriter,
    reader: BoxStreamReader,
    pub remote_public_key: ed25519_dalek::VerifyingKey,
}

impl SecureConnection {
    /// Perform client-side handshake on an outgoing connection.
    pub async fn connect(
        mut stream: TcpStream,
        network_key: [u8; 32],
        identity: Identity,
    ) -> Result<Self> {
        let client = ClientHandshake::new(network_key, identity);

        // Step 1: Send hello
        let hello = client.step1_hello();
        stream.write_all(&hello).await?;

        // Step 2: Receive server hello
        let mut server_hello = [0u8; HELLO_SIZE];
        stream.read_exact(&mut server_hello).await?;

        // Step 3: Authenticate
        let (step3, client_auth) = client.step3_authenticate(&server_hello)?;
        // Send length-prefixed auth
        let len = (client_auth.len() as u32).to_be_bytes();
        stream.write_all(&len).await?;
        stream.write_all(&client_auth).await?;

        // Step 4: Receive server auth
        let mut server_auth_len = [0u8; 4];
        stream.read_exact(&mut server_auth_len).await?;
        let auth_len = u32::from_be_bytes(server_auth_len) as usize;
        if auth_len > 4096 {
            return Err(EgreError::HandshakeFailed {
                reason: "server auth too large".into(),
            });
        }
        let mut server_auth = vec![0u8; auth_len];
        stream.read_exact(&mut server_auth).await?;

        let outcome = step3.step4_verify(&server_auth)?;
        Ok(Self::from_outcome(stream, outcome))
    }

    /// Perform server-side handshake on an incoming connection.
    pub async fn accept(
        mut stream: TcpStream,
        network_key: [u8; 32],
        identity: Identity,
    ) -> Result<Self> {
        let server = ServerHandshake::new(network_key, identity);

        // Step 1: Receive client hello
        let mut client_hello = [0u8; HELLO_SIZE];
        stream.read_exact(&mut client_hello).await?;

        // Step 2: Process and send server hello
        let (step2, server_hello) = server.step2_hello(&client_hello)?;
        stream.write_all(&server_hello).await?;

        // Step 3: Receive client auth
        let mut client_auth_len = [0u8; 4];
        stream.read_exact(&mut client_auth_len).await?;
        let auth_len = u32::from_be_bytes(client_auth_len) as usize;
        if auth_len > 4096 {
            return Err(EgreError::HandshakeFailed {
                reason: "client auth too large".into(),
            });
        }
        let mut client_auth = vec![0u8; auth_len];
        stream.read_exact(&mut client_auth).await?;

        // Step 4: Verify and respond
        let (outcome, server_auth) = step2.step3_verify_and_respond(&client_auth)?;
        let len = (server_auth.len() as u32).to_be_bytes();
        stream.write_all(&len).await?;
        stream.write_all(&server_auth).await?;

        Ok(Self::from_outcome(stream, outcome))
    }

    fn from_outcome(stream: TcpStream, outcome: HandshakeOutcome) -> Self {
        Self {
            writer: BoxStreamWriter::new(outcome.encrypt_key, outcome.encrypt_nonce),
            reader: BoxStreamReader::new(outcome.decrypt_key, outcome.decrypt_nonce),
            remote_public_key: outcome.remote_public_key,
            stream,
        }
    }

    /// Maximum payload per frame (Box Stream limit minus continuation byte)
    const MAX_CHUNK_SIZE: usize = 4095;

    /// Send data, fragmenting across multiple frames if needed.
    ///
    /// Uses a 1-byte prefix: 0x00 = final/only frame, 0x01 = continuation.
    pub async fn send(&mut self, data: &[u8]) -> Result<()> {
        let chunks: Vec<&[u8]> = data.chunks(Self::MAX_CHUNK_SIZE).collect();
        let total_chunks = chunks.len();

        for (i, chunk) in chunks.into_iter().enumerate() {
            let is_final = i == total_chunks - 1;
            let prefix = if is_final { 0x00u8 } else { 0x01u8 };

            // Prepend continuation byte
            let mut payload = Vec::with_capacity(1 + chunk.len());
            payload.push(prefix);
            payload.extend_from_slice(chunk);

            let frame = self.writer.encrypt_frame(&payload)?;
            let len = (frame.len() as u32).to_be_bytes();
            self.stream.write_all(&len).await?;
            self.stream.write_all(&frame).await?;
        }
        Ok(())
    }

    /// Receive data, reassembling fragmented frames.
    ///
    /// Reads frames until a final frame (prefix 0x00) is received.
    /// Returns None on goodbye.
    pub async fn recv(&mut self) -> Result<Option<Vec<u8>>> {
        let mut assembled = Vec::new();

        loop {
            let mut len_buf = [0u8; 4];
            if self.stream.read_exact(&mut len_buf).await.is_err() {
                return Ok(None); // Connection closed
            }
            let frame_len = u32::from_be_bytes(len_buf) as usize;
            if frame_len > 65536 {
                return Err(EgreError::Peer {
                    reason: "frame too large".into(),
                });
            }
            let mut frame = vec![0u8; frame_len];
            self.stream.read_exact(&mut frame).await?;

            // Minimum frame size: encrypted header (34 bytes)
            if frame.len() < 34 {
                return Err(EgreError::Peer {
                    reason: "frame too small".into(),
                });
            }

            let header: [u8; 34] = frame[..34].try_into().unwrap();
            match self.reader.decrypt_header(&header)? {
                None => return Ok(None), // Goodbye
                Some((_body_len, body_mac)) => {
                    let body_ct = &frame[34..];
                    let plaintext = self.reader.decrypt_body(body_ct, &body_mac)?;

                    // Check continuation prefix
                    if plaintext.is_empty() {
                        return Err(EgreError::Peer {
                            reason: "empty frame payload".into(),
                        });
                    }

                    let prefix = plaintext[0];
                    let data = &plaintext[1..];
                    assembled.extend_from_slice(data);

                    if prefix == 0x00 {
                        // Final frame
                        return Ok(Some(assembled));
                    } else if prefix != 0x01 {
                        return Err(EgreError::Peer {
                            reason: format!("invalid continuation prefix: {:#x}", prefix),
                        });
                    }
                    // Continue reading frames
                }
            }
        }
    }

    /// Send goodbye and close.
    pub async fn close(mut self) -> Result<()> {
        let frame = self.writer.goodbye()?;
        let len = (frame.len() as u32).to_be_bytes();
        let _ = self.stream.write_all(&len).await;
        let _ = self.stream.write_all(&frame).await;
        let _ = self.stream.shutdown().await;
        Ok(())
    }
}
