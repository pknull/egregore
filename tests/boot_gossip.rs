//! The node binary starts the gossip server and sync loop exactly once.
//!
//! With push enabled the gossip transport owns both; without push the node
//! runs them directly. Either way one listener binds the gossip port, and a
//! push node that cannot bind it exits rather than run without gossip.

use std::io::{BufRead, BufReader, Read};
use std::net::TcpListener;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// Logged after gossip has started.
const BOOT_DONE: &str = "node_status publisher disabled";
const SERVER_UP: &str = "gossip server listening";
const SYNC_UP: &str = "gossip sync loop started";

fn replace_line(config: String, from: &str, to: &str) -> String {
    assert!(config.contains(from), "config template lost {from:?}");
    config.replacen(from, to, 1)
}

/// Write a node config into `dir` and start the node. The API is off and
/// the gossip and metrics ports are chosen by the OS unless `gossip_port`
/// names one.
fn spawn_node(dir: &tempfile::TempDir, push_enabled: bool, gossip_port: u16) -> Child {
    let mut config = include_str!("../src/config_template.yaml").to_string();
    config = replace_line(config, "api_enabled: true", "api_enabled: false");
    config = replace_line(
        config,
        "gossip_port: 7655",
        &format!("gossip_port: {gossip_port}"),
    );
    config = replace_line(config, "network_key: CHANGE_ME", "network_key: boot-test");
    config = replace_line(config, "bind: \"127.0.0.1:9090\"", "bind: \"127.0.0.1:0\"");
    config = replace_line(
        config,
        "push_enabled: true",
        &format!("push_enabled: {push_enabled}"),
    );
    std::fs::write(dir.path().join("config.yaml"), config).unwrap();

    // `--data-dir` overrides the template's `data_dir`.
    Command::new(env!("CARGO_BIN_EXE_egregore"))
        .arg("--data-dir")
        .arg(dir.path())
        .env("RUST_LOG", "info")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap()
}

fn forward_lines(stream: impl Read + Send + 'static, tx: mpsc::Sender<String>) {
    std::thread::spawn(move || {
        for line in BufReader::new(stream).lines().map_while(Result::ok) {
            if tx.send(line).is_err() {
                break;
            }
        }
    });
}

fn log_lines(child: &mut Child) -> mpsc::Receiver<String> {
    let (tx, rx) = mpsc::channel();
    forward_lines(child.stdout.take().unwrap(), tx.clone());
    forward_lines(child.stderr.take().unwrap(), tx);
    rx
}

fn count(lines: &[String], needle: &str) -> usize {
    lines.iter().filter(|line| line.contains(needle)).count()
}

/// Boot a node and return its log once gossip is up, plus a grace period
/// for any late duplicate or error.
fn boot_log(push_enabled: bool) -> Vec<String> {
    let dir = tempfile::tempdir().unwrap();
    let mut child = spawn_node(&dir, push_enabled, 0);
    let rx = log_lines(&mut child);

    let mut deadline = Instant::now() + Duration::from_secs(30);
    let mut ready = false;
    let mut lines = Vec::new();
    while let Some(left) = deadline.checked_duration_since(Instant::now()) {
        match rx.recv_timeout(left) {
            Ok(line) => lines.push(line),
            Err(_) => break,
        }
        if !ready
            && [BOOT_DONE, SERVER_UP, SYNC_UP]
                .iter()
                .all(|m| count(&lines, m) > 0)
        {
            ready = true;
            deadline = Instant::now() + Duration::from_secs(2);
        }
    }
    let still_running = child.try_wait().unwrap().is_none();
    child.kill().ok();
    child.wait().ok();
    let log = lines.join("\n");
    assert!(ready, "gossip did not start:\n{log}");
    assert!(still_running, "node exited:\n{log}");
    lines
}

fn assert_gossip_started_once(lines: &[String]) {
    let log = lines.join("\n");
    assert_eq!(count(lines, SERVER_UP), 1, "{log}");
    assert_eq!(count(lines, SYNC_UP), 1, "{log}");
    assert_eq!(count(lines, "\"level\":\"ERROR\""), 0, "{log}");
}

#[test]
fn push_node_starts_gossip_once() {
    assert_gossip_started_once(&boot_log(true));
}

#[test]
fn pull_only_node_starts_gossip_once() {
    assert_gossip_started_once(&boot_log(false));
}

#[test]
fn push_node_exits_when_the_gossip_port_is_taken() {
    let held = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = held.local_addr().unwrap().port();
    let dir = tempfile::tempdir().unwrap();
    let mut child = spawn_node(&dir, true, port);
    let rx = log_lines(&mut child);

    let deadline = Instant::now() + Duration::from_secs(30);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if Instant::now() > deadline {
            child.kill().ok();
            panic!("node kept running with its gossip port taken");
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    // The process has exited, so both pipes close and the channel drains.
    let log: Vec<String> = rx.iter().collect();
    assert!(!status.success(), "{}", log.join("\n"));
    assert!(
        count(&log, "gossip transport failed to start") > 0,
        "{}",
        log.join("\n")
    );
    drop(held);
}
