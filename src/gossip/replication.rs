use std::collections::HashSet;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::error::{EgreError, Result};
use crate::feed::engine::FeedEngine;
use crate::feed::models::Message;
use crate::gossip::connection::SecureConnection;
use crate::identity::PublicId;

/// Gossip protocol messages sent over the encrypted connection.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum GossipMessage {
    /// "I have these feeds at these sequences"
    Have { feeds: Vec<FeedState> },
    /// "I want messages from these feeds after these sequences"
    Want { requests: Vec<FeedRequest> },
    /// "Here are messages you requested"
    Messages { messages: Vec<Message> },
    /// "Replication complete"
    Done,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct FeedState {
    pub author: PublicId,
    pub latest_sequence: u64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct FeedRequest {
    pub author: PublicId,
    pub after_sequence: u64,
}

/// Controls what gets replicated.
#[derive(Default)]
pub struct ReplicationConfig {
    /// If Some, only replicate feeds from these authors.
    /// If None, replicate all feeds.
    pub follows: Option<HashSet<PublicId>>,
}

impl ReplicationConfig {
    /// Check if we want messages from this author.
    fn wants_author(&self, author: &PublicId) -> bool {
        match &self.follows {
            Some(follows) => follows.contains(author),
            None => true,
        }
    }
}

const BATCH_SIZE: u32 = 50;
const MAX_MESSAGES_PER_SESSION: usize = 10_000;
const MAX_MESSAGES_PER_FRAME: usize = 200;
const MAX_WANT_REQUESTS: usize = 500;

/// Target payload size for message batches before connection-layer fragmentation.
/// Set below 4096 to minimize frame fragmentation for typical message batches.
/// Note: Individual large messages will be fragmented by SecureConnection::send().
const MAX_BATCH_PAYLOAD: usize = 3800;

/// Approximate JSON envelope overhead for GossipMessage::Messages wrapper.
/// Accounts for: {"type":"messages","messages":[...]}
const MESSAGE_ENVELOPE_OVERHEAD: usize = 40;

/// Run replication as the initiator (client).
pub async fn replicate_as_client(
    conn: &mut SecureConnection,
    engine: &Arc<FeedEngine>,
    config: &ReplicationConfig,
) -> Result<()> {
    // Send our "have" state
    let have = build_have_message(engine).await?;
    conn.send(&serde_json::to_vec(&have)?).await?;

    // Receive server's "have"
    if let Some(data) = conn.recv().await? {
        let server_have: GossipMessage = serde_json::from_slice(&data)?;
        if let GossipMessage::Have { feeds: server_feeds } = server_have {
            let requests = build_want_requests(engine, &server_feeds, config).await?;

            // Always send Want (even if empty) so server's handle_peer_wants unblocks
            let want = GossipMessage::Want { requests };
            conn.send(&serde_json::to_vec(&want)?).await?;

            // Receive messages until Done
            receive_messages(conn, engine).await?;
        }
    }

    // Respond to server's wants
    handle_peer_wants(conn, engine).await?;

    Ok(())
}

/// Run replication as the responder (server).
pub async fn replicate_as_server(
    conn: &mut SecureConnection,
    engine: &Arc<FeedEngine>,
    config: &ReplicationConfig,
) -> Result<()> {
    // Receive client's "have"
    let client_have = match conn.recv().await? {
        Some(data) => serde_json::from_slice::<GossipMessage>(&data)?,
        None => return Ok(()),
    };

    // Send our "have"
    let have = build_have_message(engine).await?;
    conn.send(&serde_json::to_vec(&have)?).await?;

    // Handle client's wants
    handle_peer_wants(conn, engine).await?;

    // Figure out what we want from client's have (filtered by follows)
    if let GossipMessage::Have { feeds: client_feeds } = client_have {
        let requests = build_want_requests(engine, &client_feeds, config).await?;

        // Always send Want (even if empty) so client's handle_peer_wants unblocks
        let want = GossipMessage::Want { requests };
        conn.send(&serde_json::to_vec(&want)?).await?;

        // Receive messages until Done
        receive_messages(conn, engine).await?;
    }

    Ok(())
}

/// Build a Have message from local feed state (spawn_blocking for DB access).
async fn build_have_message(engine: &Arc<FeedEngine>) -> Result<GossipMessage> {
    let eng = engine.clone();
    let feeds = tokio::task::spawn_blocking(move || eng.store().get_all_feeds())
        .await
        .map_err(|e| EgreError::Io(std::io::Error::new(std::io::ErrorKind::Other, e)))??;

    let feed_states: Vec<FeedState> = feeds
        .into_iter()
        .map(|(author, seq)| FeedState {
            author,
            latest_sequence: seq,
        })
        .collect();

    tracing::debug!(
        feed_count = feed_states.len(),
        feeds = ?feed_states.iter().map(|f| (&f.author.0[..12], f.latest_sequence)).collect::<Vec<_>>(),
        "built Have message"
    );

    Ok(GossipMessage::Have { feeds: feed_states })
}

/// Build Want requests by comparing remote feeds against local state.
async fn build_want_requests(
    engine: &Arc<FeedEngine>,
    remote_feeds: &[FeedState],
    config: &ReplicationConfig,
) -> Result<Vec<FeedRequest>> {
    tracing::debug!(
        remote_feed_count = remote_feeds.len(),
        remote_feeds = ?remote_feeds.iter().map(|f| (&f.author.0[..12], f.latest_sequence)).collect::<Vec<_>>(),
        "received peer Have"
    );

    // Filter by follows first (no DB needed)
    let candidates: Vec<(PublicId, u64)> = remote_feeds
        .iter()
        .filter(|f| config.wants_author(&f.author))
        .map(|f| (f.author.clone(), f.latest_sequence))
        .collect();

    if candidates.is_empty() {
        tracing::debug!("no candidate feeds after follow filter");
        return Ok(Vec::new());
    }

    let eng = engine.clone();
    tokio::task::spawn_blocking(move || {
        let store = eng.store();
        let mut requests = Vec::new();
        for (author, remote_seq) in &candidates {
            // Use contiguous sequence to detect gaps needing backfill
            let our_contiguous = store.get_latest_contiguous_sequence(author)?;
            tracing::debug!(
                author = &author.0[..12],
                remote_seq = remote_seq,
                our_contiguous = our_contiguous,
                "comparing feed state"
            );
            if *remote_seq > our_contiguous {
                requests.push(FeedRequest {
                    author: author.clone(),
                    after_sequence: our_contiguous,
                });
            }
        }
        tracing::debug!(
            request_count = requests.len(),
            requests = ?requests.iter().map(|r| (&r.author.0[..12], r.after_sequence)).collect::<Vec<_>>(),
            "built Want requests"
        );
        Ok(requests)
    })
    .await
    .map_err(|e| EgreError::Io(std::io::Error::new(std::io::ErrorKind::Other, e)))?
}

/// Receive messages from peer until Done, with per-frame and per-session limits.
async fn receive_messages(
    conn: &mut SecureConnection,
    engine: &Arc<FeedEngine>,
) -> Result<()> {
    tracing::debug!("waiting for messages from peer");
    let mut total_received: usize = 0;
    while let Some(data) = conn.recv().await? {
        let msg: GossipMessage = serde_json::from_slice(&data)?;
        match msg {
            GossipMessage::Messages { messages } => {
                tracing::debug!(batch_size = messages.len(), "received message batch");
                if messages.len() > MAX_MESSAGES_PER_FRAME {
                    tracing::warn!(
                        count = messages.len(),
                        "peer sent too many messages in one frame, truncating"
                    );
                }
                let batch: Vec<Message> =
                    messages.into_iter().take(MAX_MESSAGES_PER_FRAME).collect();
                let batch_len = batch.len();

                let eng = engine.clone();
                tokio::task::spawn_blocking(move || {
                    for m in &batch {
                        if let Err(e) = eng.ingest(m) {
                            tracing::warn!(
                                author = %m.author,
                                seq = m.sequence,
                                error = %e,
                                "failed to ingest message"
                            );
                        }
                    }
                })
                .await
                .unwrap_or_else(|e| {
                    tracing::warn!(error = %e, "message ingestion task failed");
                });

                total_received += batch_len;
                if total_received >= MAX_MESSAGES_PER_SESSION {
                    tracing::warn!(
                        total = total_received,
                        "session message limit reached, stopping"
                    );
                    break;
                }
            }
            GossipMessage::Done => break,
            _ => break,
        }
    }
    Ok(())
}

/// Handle incoming want requests and send the requested messages.
/// Paginates through each feed until exhausted or session limit reached.
async fn handle_peer_wants(
    conn: &mut SecureConnection,
    engine: &Arc<FeedEngine>,
) -> Result<()> {
    tracing::debug!("waiting for peer Want message");
    if let Some(data) = conn.recv().await? {
        let msg: GossipMessage = serde_json::from_slice(&data)?;
        if let GossipMessage::Want { requests } = msg {
            tracing::debug!(
                request_count = requests.len(),
                requests = ?requests.iter().map(|r| (&r.author.0[..12], r.after_sequence)).collect::<Vec<_>>(),
                "received peer Want"
            );
            let mut total_sent: usize = 0;
            for req in requests.iter().take(MAX_WANT_REQUESTS) {
                let mut after_seq = req.after_sequence;
                loop {
                    let eng = engine.clone();
                    let author = req.author.clone();
                    let seq = after_seq;
                    let messages = tokio::task::spawn_blocking(move || {
                        eng.store().get_messages_after(&author, seq, BATCH_SIZE)
                    })
                    .await
                    .map_err(|e| {
                        EgreError::Io(std::io::Error::new(std::io::ErrorKind::Other, e))
                    })??;

                    let batch_len = messages.len();
                    tracing::debug!(
                        author = &req.author.0[..12],
                        after_seq = after_seq,
                        batch_len = batch_len,
                        "fetched messages for Want"
                    );
                    if !messages.is_empty() {
                        after_seq = messages.last().map(|m| m.sequence).unwrap_or(after_seq);

                        // Chunk messages to minimize frame fragmentation.
                        // Connection layer handles oversized chunks via fragmentation,
                        // but batching small messages together is more efficient.
                        let mut chunk: Vec<Message> = Vec::new();
                        let mut chunk_payload = MESSAGE_ENVELOPE_OVERHEAD;

                        for m in messages {
                            let msg_size = match serde_json::to_string(&m) {
                                Ok(json) => json.len() + 2, // +2 for comma/brackets
                                Err(e) => {
                                    tracing::warn!(
                                        seq = m.sequence,
                                        error = %e,
                                        "failed to serialize message, skipping"
                                    );
                                    continue;
                                }
                            };

                            // Flush chunk if adding this message would exceed target size
                            // (but always allow at least one message per chunk)
                            if chunk_payload + msg_size > MAX_BATCH_PAYLOAD && !chunk.is_empty() {
                                let response = GossipMessage::Messages { messages: chunk };
                                conn.send(&serde_json::to_vec(&response)?).await?;
                                chunk = Vec::new();
                                chunk_payload = MESSAGE_ENVELOPE_OVERHEAD;
                            }

                            chunk_payload += msg_size;
                            chunk.push(m);
                        }

                        // Send remaining chunk
                        if !chunk.is_empty() {
                            let response = GossipMessage::Messages { messages: chunk };
                            conn.send(&serde_json::to_vec(&response)?).await?;
                        }
                        total_sent += batch_len;
                    }

                    if batch_len < BATCH_SIZE as usize || total_sent >= MAX_MESSAGES_PER_SESSION {
                        break;
                    }
                }
                if total_sent >= MAX_MESSAGES_PER_SESSION {
                    break;
                }
            }
            tracing::debug!(total_sent = total_sent, "sending Done");
        } else {
            tracing::warn!(msg_type = ?msg, "expected Want but got something else");
        }
        conn.send(&serde_json::to_vec(&GossipMessage::Done)?).await?;
    } else {
        tracing::debug!("no Want message received");
    }
    Ok(())
}
