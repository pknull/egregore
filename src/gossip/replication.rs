use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::error::Result;
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

const BATCH_SIZE: u32 = 50;

/// Run replication as the initiator (client).
pub async fn replicate_as_client(
    conn: &mut SecureConnection,
    engine: &Arc<FeedEngine>,
) -> Result<()> {
    // Send our "have" state
    let feeds = engine.store().get_all_feeds()?;
    let have = GossipMessage::Have {
        feeds: feeds
            .iter()
            .map(|(author, seq)| FeedState {
                author: author.clone(),
                latest_sequence: *seq,
            })
            .collect(),
    };
    let have_json = serde_json::to_vec(&have)?;
    conn.send(&have_json).await?;

    // Receive server's "have"
    if let Some(data) = conn.recv().await? {
        let server_have: GossipMessage = serde_json::from_slice(&data)?;
        if let GossipMessage::Have { feeds: server_feeds } = server_have {
            // Figure out what we want from them
            let mut requests = Vec::new();
            for feed in &server_feeds {
                let our_seq = engine.store().get_latest_sequence(&feed.author)?;
                if feed.latest_sequence > our_seq {
                    requests.push(FeedRequest {
                        author: feed.author.clone(),
                        after_sequence: our_seq,
                    });
                }
            }

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
) -> Result<()> {
    // Receive client's "have"
    let client_have = match conn.recv().await? {
        Some(data) => serde_json::from_slice::<GossipMessage>(&data)?,
        None => return Ok(()),
    };

    // Send our "have"
    let feeds = engine.store().get_all_feeds()?;
    let have = GossipMessage::Have {
        feeds: feeds
            .iter()
            .map(|(author, seq)| FeedState {
                author: author.clone(),
                latest_sequence: *seq,
            })
            .collect(),
    };
    conn.send(&serde_json::to_vec(&have)?).await?;

    // Handle client's wants
    handle_peer_wants(conn, engine).await?;

    // Figure out what we want from client's have
    if let GossipMessage::Have { feeds: client_feeds } = client_have {
        let mut requests = Vec::new();
        for feed in &client_feeds {
            let our_seq = engine.store().get_latest_sequence(&feed.author)?;
            if feed.latest_sequence > our_seq {
                requests.push(FeedRequest {
                    author: feed.author.clone(),
                    after_sequence: our_seq,
                });
            }
        }

        // Always send Want (even if empty) so client's handle_peer_wants unblocks
        let want = GossipMessage::Want { requests };
        conn.send(&serde_json::to_vec(&want)?).await?;

        // Receive messages until Done
        receive_messages(conn, engine).await?;
    }

    Ok(())
}

/// Receive messages from peer until Done.
async fn receive_messages(
    conn: &mut SecureConnection,
    engine: &Arc<FeedEngine>,
) -> Result<()> {
    loop {
        match conn.recv().await? {
            Some(data) => {
                let msg: GossipMessage = serde_json::from_slice(&data)?;
                match msg {
                    GossipMessage::Messages { messages } => {
                        for m in &messages {
                            if let Err(e) = engine.ingest(m) {
                                tracing::warn!(
                                    author = %m.author,
                                    seq = m.sequence,
                                    error = %e,
                                    "failed to ingest message"
                                );
                            }
                        }
                    }
                    GossipMessage::Done => break,
                    _ => break,
                }
            }
            None => break,
        }
    }
    Ok(())
}

/// Handle incoming want requests and send the requested messages.
async fn handle_peer_wants(
    conn: &mut SecureConnection,
    engine: &Arc<FeedEngine>,
) -> Result<()> {
    if let Some(data) = conn.recv().await? {
        let msg: GossipMessage = serde_json::from_slice(&data)?;
        if let GossipMessage::Want { requests } = msg {
            for req in &requests {
                let messages = engine
                    .store()
                    .get_messages_after(&req.author, req.after_sequence, BATCH_SIZE)?;
                if !messages.is_empty() {
                    let response = GossipMessage::Messages { messages };
                    conn.send(&serde_json::to_vec(&response)?).await?;
                }
            }
        }
        conn.send(&serde_json::to_vec(&GossipMessage::Done)?).await?;
    }
    Ok(())
}
