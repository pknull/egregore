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
//! For persistent connections, `into_split()` separates the connection into
//! independent `SecureReader` and `SecureWriter` halves for concurrent I/O.
//!
//! Used by gossip/client.rs (outgoing sync) and gossip/server.rs (incoming).

use std::net::SocketAddr;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::TcpStream;

use crate::crypto::box_stream::{BoxStreamReader, BoxStreamWriter};
use crate::crypto::handshake::{
    ClientHandshake, HandshakeOutcome, ServerHandshake,
};
use crate::error::{EgreError, Result};
use crate::identity::Identity;

const HELLO_SIZE: usize = 64;

/// Frame prefix indicating final (or only) frame in a fragmented message.
const FRAME_PREFIX_FINAL: u8 = 0x00;
/// Frame prefix indicating more frames follow.
const FRAME_PREFIX_CONTINUATION: u8 = 0x01;
/// Maximum size for a reassembled message (128 KB).
/// Prevents memory exhaustion from malicious continuation frames.
const MAX_ASSEMBLED_SIZE: usize = 128 * 1024;
/// Maximum frame size accepted from peer.
const MAX_FRAME_SIZE: usize = 65536;
/// Read timeout per frame (prevents slowloris-style connection exhaustion).
const FRAME_READ_TIMEOUT: Duration = Duration::from_secs(15);

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
    /// Uses a 1-byte prefix: FRAME_PREFIX_FINAL (0x00) for final/only frame,
    /// FRAME_PREFIX_CONTINUATION (0x01) for continuation frames.
    pub async fn send(&mut self, data: &[u8]) -> Result<()> {
        // Handle empty data: send a single frame with just the final prefix
        if data.is_empty() {
            let payload = vec![FRAME_PREFIX_FINAL];
            let frame = self.writer.encrypt_frame(&payload)?;
            let len = (frame.len() as u32).to_be_bytes();
            self.stream.write_all(&len).await?;
            self.stream.write_all(&frame).await?;
            return Ok(());
        }

        // Calculate total chunks without intermediate allocation
        let total_chunks = data.len().div_ceil(Self::MAX_CHUNK_SIZE);

        for (i, chunk) in data.chunks(Self::MAX_CHUNK_SIZE).enumerate() {
            let is_final = i == total_chunks - 1;
            let prefix = if is_final {
                FRAME_PREFIX_FINAL
            } else {
                FRAME_PREFIX_CONTINUATION
            };

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
    /// Reads frames until a final frame (FRAME_PREFIX_FINAL) is received.
    /// Returns None on graceful connection close or goodbye frame.
    /// Returns error on protocol violations or size limits exceeded.
    pub async fn recv(&mut self) -> Result<Option<Vec<u8>>> {
        let mut assembled = Vec::new();

        loop {
            let mut len_buf = [0u8; 4];
            match tokio::time::timeout(FRAME_READ_TIMEOUT, self.stream.read_exact(&mut len_buf))
                .await
            {
                Ok(Ok(_)) => {}
                Ok(Err(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                    // Clean connection close
                    return Ok(None);
                }
                Ok(Err(e)) => {
                    return Err(EgreError::Io(e));
                }
                Err(_) => {
                    return Err(EgreError::Peer {
                        reason: "read timeout".into(),
                    });
                }
            }

            let frame_len = u32::from_be_bytes(len_buf) as usize;
            if frame_len == 0 {
                return Err(EgreError::Peer {
                    reason: "zero-length frame".into(),
                });
            }
            if frame_len > MAX_FRAME_SIZE {
                return Err(EgreError::Peer {
                    reason: format!("frame too large: {} > {}", frame_len, MAX_FRAME_SIZE),
                });
            }

            let mut frame = vec![0u8; frame_len];
            tokio::time::timeout(FRAME_READ_TIMEOUT, self.stream.read_exact(&mut frame))
                .await
                .map_err(|_| EgreError::Peer {
                    reason: "read timeout".into(),
                })??;

            // Minimum frame size: encrypted header (34 bytes)
            if frame.len() < 34 {
                return Err(EgreError::Peer {
                    reason: "frame too small".into(),
                });
            }

            let header: [u8; 34] = frame[..34].try_into().unwrap();
            match self.reader.decrypt_header(&header)? {
                None => return Ok(None), // Goodbye
                Some((body_len, body_mac)) => {
                    let body_ct = &frame[34..];

                    // Validate body length matches header
                    if body_ct.len() != body_len as usize {
                        return Err(EgreError::Peer {
                            reason: format!(
                                "body length mismatch: header={}, actual={}",
                                body_len,
                                body_ct.len()
                            ),
                        });
                    }

                    let plaintext = self.reader.decrypt_body(body_ct, &body_mac)?;

                    // Check continuation prefix
                    if plaintext.is_empty() {
                        return Err(EgreError::Peer {
                            reason: "empty frame payload".into(),
                        });
                    }

                    let prefix = plaintext[0];
                    let data = &plaintext[1..];

                    // Check assembled size limit before extending
                    if assembled.len() + data.len() > MAX_ASSEMBLED_SIZE {
                        return Err(EgreError::Peer {
                            reason: format!(
                                "assembled message too large: {} > {}",
                                assembled.len() + data.len(),
                                MAX_ASSEMBLED_SIZE
                            ),
                        });
                    }

                    assembled.extend_from_slice(data);

                    if prefix == FRAME_PREFIX_FINAL {
                        return Ok(Some(assembled));
                    } else if prefix != FRAME_PREFIX_CONTINUATION {
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

    /// Get the peer's socket address.
    pub fn peer_addr(&self) -> std::io::Result<SocketAddr> {
        self.stream.peer_addr()
    }

    /// Split into independent reader and writer halves for concurrent I/O.
    ///
    /// After splitting, the original `SecureConnection` is consumed. The halves
    /// can be used independently in separate tasks for bidirectional persistent
    /// connections.
    pub fn into_split(self) -> (SecureReader, SecureWriter) {
        let (read_half, write_half) = self.stream.into_split();
        (
            SecureReader {
                stream: read_half,
                reader: self.reader,
                remote_public_key: self.remote_public_key,
            },
            SecureWriter {
                stream: write_half,
                writer: self.writer,
            },
        )
    }
}

/// Read half of a split secure connection.
///
/// Handles Box Stream decryption and frame reassembly for incoming data.
pub struct SecureReader {
    stream: OwnedReadHalf,
    reader: BoxStreamReader,
    pub remote_public_key: ed25519_dalek::VerifyingKey,
}

impl SecureReader {
    /// Receive data, reassembling fragmented frames.
    ///
    /// Returns None on graceful connection close or goodbye frame.
    /// Returns error on protocol violations or size limits exceeded.
    pub async fn recv(&mut self) -> Result<Option<Vec<u8>>> {
        let mut assembled = Vec::new();

        loop {
            let mut len_buf = [0u8; 4];
            match tokio::time::timeout(FRAME_READ_TIMEOUT, self.stream.read_exact(&mut len_buf))
                .await
            {
                Ok(Ok(_)) => {}
                Ok(Err(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                    return Ok(None);
                }
                Ok(Err(e)) => {
                    return Err(EgreError::Io(e));
                }
                Err(_) => {
                    return Err(EgreError::Peer {
                        reason: "read timeout".into(),
                    });
                }
            }

            let frame_len = u32::from_be_bytes(len_buf) as usize;
            if frame_len == 0 {
                return Err(EgreError::Peer {
                    reason: "zero-length frame".into(),
                });
            }
            if frame_len > MAX_FRAME_SIZE {
                return Err(EgreError::Peer {
                    reason: format!("frame too large: {} > {}", frame_len, MAX_FRAME_SIZE),
                });
            }

            let mut frame = vec![0u8; frame_len];
            tokio::time::timeout(FRAME_READ_TIMEOUT, self.stream.read_exact(&mut frame))
                .await
                .map_err(|_| EgreError::Peer {
                    reason: "read timeout".into(),
                })??;

            if frame.len() < 34 {
                return Err(EgreError::Peer {
                    reason: "frame too small".into(),
                });
            }

            let header: [u8; 34] = frame[..34].try_into().unwrap();
            match self.reader.decrypt_header(&header)? {
                None => return Ok(None),
                Some((body_len, body_mac)) => {
                    let body_ct = &frame[34..];

                    if body_ct.len() != body_len as usize {
                        return Err(EgreError::Peer {
                            reason: format!(
                                "body length mismatch: header={}, actual={}",
                                body_len,
                                body_ct.len()
                            ),
                        });
                    }

                    let plaintext = self.reader.decrypt_body(body_ct, &body_mac)?;

                    if plaintext.is_empty() {
                        return Err(EgreError::Peer {
                            reason: "empty frame payload".into(),
                        });
                    }

                    let prefix = plaintext[0];
                    let data = &plaintext[1..];

                    if assembled.len() + data.len() > MAX_ASSEMBLED_SIZE {
                        return Err(EgreError::Peer {
                            reason: format!(
                                "assembled message too large: {} > {}",
                                assembled.len() + data.len(),
                                MAX_ASSEMBLED_SIZE
                            ),
                        });
                    }

                    assembled.extend_from_slice(data);

                    if prefix == FRAME_PREFIX_FINAL {
                        return Ok(Some(assembled));
                    } else if prefix != FRAME_PREFIX_CONTINUATION {
                        return Err(EgreError::Peer {
                            reason: format!("invalid continuation prefix: {:#x}", prefix),
                        });
                    }
                }
            }
        }
    }
}

/// Write half of a split secure connection.
///
/// Handles Box Stream encryption and frame fragmentation for outgoing data.
pub struct SecureWriter {
    stream: OwnedWriteHalf,
    writer: BoxStreamWriter,
}

impl SecureWriter {
    /// Maximum payload per frame (Box Stream limit minus continuation byte)
    const MAX_CHUNK_SIZE: usize = 4095;

    /// Send data, fragmenting across multiple frames if needed.
    pub async fn send(&mut self, data: &[u8]) -> Result<()> {
        if data.is_empty() {
            let payload = vec![FRAME_PREFIX_FINAL];
            let frame = self.writer.encrypt_frame(&payload)?;
            let len = (frame.len() as u32).to_be_bytes();
            self.stream.write_all(&len).await?;
            self.stream.write_all(&frame).await?;
            return Ok(());
        }

        let total_chunks = data.len().div_ceil(Self::MAX_CHUNK_SIZE);

        for (i, chunk) in data.chunks(Self::MAX_CHUNK_SIZE).enumerate() {
            let is_final = i == total_chunks - 1;
            let prefix = if is_final {
                FRAME_PREFIX_FINAL
            } else {
                FRAME_PREFIX_CONTINUATION
            };

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

    /// Send goodbye frame to signal clean stream termination.
    pub async fn goodbye(&mut self) -> Result<()> {
        let frame = self.writer.goodbye()?;
        let len = (frame.len() as u32).to_be_bytes();
        let _ = self.stream.write_all(&len).await;
        let _ = self.stream.write_all(&frame).await;
        Ok(())
    }

    /// Shutdown the write half of the connection.
    pub async fn shutdown(&mut self) -> Result<()> {
        self.stream.shutdown().await?;
        Ok(())
    }
}
