use std::sync::Arc;
use std::time::Duration;

use egregore::feed::content_types::Content;
use egregore::feed::engine::FeedEngine;
use egregore::feed::store::FeedStore;
use egregore::gossip::replication::ReplicationConfig;
use egregore::identity::Identity;

const NETWORK_KEY: [u8; 32] = [42u8; 32];

fn test_content(title: &str) -> serde_json::Value {
    Content::Insight {
        title: title.into(),
        context: Some("test".into()),
        observation: "obs".into(),
        evidence: Some("ev".into()),
        guidance: Some("guid".into()),
        confidence: Some(0.5),
        tags: vec![],
    }
    .to_value()
}

/// Spin up two daemons (A and B), publish to A, replicate to B via gossip.
#[tokio::test]
async fn two_instance_gossip_replication() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("egregore=debug")
        .try_init();

    // --- Setup instance A ---
    let identity_a = Identity::generate();
    let store_a = FeedStore::open_memory().unwrap();
    let engine_a = Arc::new(FeedEngine::new(store_a));

    engine_a
        .publish(&identity_a, test_content("Test insight"))
        .unwrap();

    // Verify A has 1 message
    let a_feed = engine_a.store().get_all_feeds().unwrap();
    assert_eq!(a_feed.len(), 1);
    assert_eq!(a_feed[0].1, 1);

    // --- Setup instance B ---
    let identity_b = Identity::generate();
    let store_b = FeedStore::open_memory().unwrap();
    let engine_b = Arc::new(FeedEngine::new(store_b));

    assert!(engine_b.store().get_all_feeds().unwrap().is_empty());

    // --- Start A's gossip server on random port ---
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let server_addr = listener.local_addr().unwrap();

    let server_identity = identity_a.clone();
    let server_engine = engine_a.clone();
    let server_handle = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut conn = egregore::gossip::connection::SecureConnection::accept(
            stream,
            NETWORK_KEY,
            server_identity,
        )
        .await
        .unwrap();
        let config = ReplicationConfig::default();
        egregore::gossip::replication::replicate_as_server(&mut conn, &server_engine, &config)
            .await
            .unwrap();
        let _ = conn.close().await;
    });

    tokio::time::sleep(Duration::from_millis(50)).await;

    // --- B connects to A as client ---
    let stream = tokio::net::TcpStream::connect(server_addr).await.unwrap();
    let mut conn = egregore::gossip::connection::SecureConnection::connect(
        stream,
        NETWORK_KEY,
        identity_b.clone(),
    )
    .await
    .unwrap();

    let config = ReplicationConfig::default();
    egregore::gossip::replication::replicate_as_client(&mut conn, &engine_b, &config)
        .await
        .unwrap();
    let _ = conn.close().await;

    tokio::time::timeout(Duration::from_secs(5), server_handle)
        .await
        .expect("server timed out")
        .expect("server panicked");

    // --- Verify B now has A's message ---
    let b_feed = engine_b.store().get_all_feeds().unwrap();
    assert_eq!(b_feed.len(), 1, "B should have one feed after replication");
    assert_eq!(b_feed[0].1, 1, "B should have sequence 1");

    let b_messages = engine_b
        .store()
        .get_messages_after(&identity_a.public_id(), 0, 10)
        .unwrap();
    assert_eq!(b_messages.len(), 1);
    assert_eq!(b_messages[0].author, identity_a.public_id());
    assert_eq!(b_messages[0].sequence, 1);
}

/// Verify bidirectional replication: both sides exchange what the other is missing.
#[tokio::test]
async fn bidirectional_replication() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("egregore=debug")
        .try_init();

    let identity_a = Identity::generate();
    let identity_b = Identity::generate();

    let store_a = FeedStore::open_memory().unwrap();
    let engine_a = Arc::new(FeedEngine::new(store_a));

    let store_b = FeedStore::open_memory().unwrap();
    let engine_b = Arc::new(FeedEngine::new(store_b));

    // A publishes 2 messages
    for i in 0..2 {
        engine_a
            .publish(&identity_a, test_content(&format!("A-insight-{i}")))
            .unwrap();
    }

    // B publishes 3 messages
    for i in 0..3 {
        engine_b
            .publish(&identity_b, test_content(&format!("B-insight-{i}")))
            .unwrap();
    }

    // Start A as server
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let server_addr = listener.local_addr().unwrap();

    let server_id = identity_a.clone();
    let server_eng = engine_a.clone();
    let server_handle = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut conn =
            egregore::gossip::connection::SecureConnection::accept(stream, NETWORK_KEY, server_id)
                .await
                .unwrap();
        let config = ReplicationConfig::default();
        egregore::gossip::replication::replicate_as_server(&mut conn, &server_eng, &config)
            .await
            .unwrap();
        let _ = conn.close().await;
    });

    tokio::time::sleep(Duration::from_millis(50)).await;

    // B connects as client
    let stream = tokio::net::TcpStream::connect(server_addr).await.unwrap();
    let mut conn = egregore::gossip::connection::SecureConnection::connect(
        stream,
        NETWORK_KEY,
        identity_b.clone(),
    )
    .await
    .unwrap();
    let config = ReplicationConfig::default();
    egregore::gossip::replication::replicate_as_client(&mut conn, &engine_b, &config)
        .await
        .unwrap();
    let _ = conn.close().await;

    tokio::time::timeout(Duration::from_secs(5), server_handle)
        .await
        .expect("server timed out")
        .expect("server panicked");

    // Verify B has A's 2 messages
    let b_a_msgs = engine_b
        .store()
        .get_messages_after(&identity_a.public_id(), 0, 10)
        .unwrap();
    assert_eq!(b_a_msgs.len(), 2, "B should have A's 2 messages");

    // Verify A has B's 3 messages
    let a_b_msgs = engine_a
        .store()
        .get_messages_after(&identity_b.public_id(), 0, 10)
        .unwrap();
    assert_eq!(a_b_msgs.len(), 3, "A should have B's 3 messages");
}

/// Verify follow-filtered replication: B follows only one of two feeds on A.
#[tokio::test]
async fn follow_filtered_replication() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("egregore=debug")
        .try_init();

    let identity_x = Identity::generate();
    let identity_y = Identity::generate();

    let store_a = FeedStore::open_memory().unwrap();
    let engine_a = Arc::new(FeedEngine::new(store_a));

    let store_b = FeedStore::open_memory().unwrap();
    let engine_b = Arc::new(FeedEngine::new(store_b));

    // A has feeds X and Y
    engine_a
        .publish(&identity_x, test_content("X-insight"))
        .unwrap();
    engine_a
        .publish(&identity_y, test_content("Y-insight"))
        .unwrap();

    assert_eq!(engine_a.store().get_all_feeds().unwrap().len(), 2);

    // B follows only X
    let follow_config = ReplicationConfig {
        follows: Some([identity_x.public_id()].into_iter().collect()),
    };

    // Start A as server
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let server_addr = listener.local_addr().unwrap();

    let server_id = identity_x.clone(); // doesn't matter which identity serves
    let server_eng = engine_a.clone();
    let server_handle = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut conn =
            egregore::gossip::connection::SecureConnection::accept(stream, NETWORK_KEY, server_id)
                .await
                .unwrap();
        let config = ReplicationConfig::default(); // server doesn't filter
        egregore::gossip::replication::replicate_as_server(&mut conn, &server_eng, &config)
            .await
            .unwrap();
        let _ = conn.close().await;
    });

    tokio::time::sleep(Duration::from_millis(50)).await;

    // B connects as client with follow filter
    let stream = tokio::net::TcpStream::connect(server_addr).await.unwrap();
    let mut conn = egregore::gossip::connection::SecureConnection::connect(
        stream,
        NETWORK_KEY,
        Identity::generate(),
    )
    .await
    .unwrap();

    egregore::gossip::replication::replicate_as_client(&mut conn, &engine_b, &follow_config)
        .await
        .unwrap();
    let _ = conn.close().await;

    tokio::time::timeout(Duration::from_secs(5), server_handle)
        .await
        .expect("server timed out")
        .expect("server panicked");

    // B should have only X's feed, not Y's
    let b_feeds = engine_b.store().get_all_feeds().unwrap();
    assert_eq!(b_feeds.len(), 1, "B should have only 1 feed");
    assert_eq!(b_feeds[0].0, identity_x.public_id());

    let b_x_msgs = engine_b
        .store()
        .get_messages_after(&identity_x.public_id(), 0, 10)
        .unwrap();
    assert_eq!(b_x_msgs.len(), 1);

    let b_y_msgs = engine_b
        .store()
        .get_messages_after(&identity_y.public_id(), 0, 10)
        .unwrap();
    assert_eq!(b_y_msgs.len(), 0, "B should not have Y's messages");
}
