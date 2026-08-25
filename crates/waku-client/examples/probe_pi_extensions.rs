//! PIWAKU: temporary diagnostic — spawns the real debug daemon exactly like
//! the supervisor does, connects, and sends LoadPiExtensions with a real
//! project. Run:
//!   cargo run -p waku-client --example probe_pi_extensions
//! Requires target/debug/waku-debug-daemon to exist.

use std::io::{BufRead as _, BufReader};
use waku_protocol::DAEMON_TOKEN_ENV;
use std::process::Stdio;
use std::time::{Duration, Instant};
use uuid::Uuid;
use waku_client::{Command, DaemonClient, ResponsePayload};

fn main() -> anyhow::Result<()> {
    let daemon_bin = std::path::PathBuf::from("target/debug/waku-debug-daemon");
    let token = "probe-secret".to_string();
    let mut child = std::process::Command::new(&daemon_bin)
        .arg("--bind")
        .arg("127.0.0.1:0")
        .arg("--parent-pid")
        .arg(std::process::id().to_string())
        .env(DAEMON_TOKEN_ENV, &token)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .context("could not launch the debug daemon")?;

    let stdout = child.stdout.take().context("no readiness stream")?;
    let mut line = String::new();
    BufReader::new(stdout).read_line(&mut line)?;
    let ready: waku_client::DaemonReady = serde_json::from_str(line.trim())?;
    eprintln!("daemon ready at {} (protocol {})", ready.address, ready.protocol_version);

    let client = waku_client::DaemonClient::connect(&ready.address, token)?;
    eprintln!("connected");

    let project = std::path::PathBuf::from("/Users/max333/Documents/ChatGPT/PiWaku");
    let started = Instant::now();
    let payload = client.request(
        Uuid::nil(),
        Uuid::nil(),
        Command::LoadPiExtensions {
            projects: vec![("PiWaku".to_string(), project)],
        },
    );
    match payload? {
        ResponsePayload::PiExtensions { extensions } => {
            eprintln!("inventory in {:?}: {} packages", started.elapsed(), extensions.len());
        }
        other => anyhow::bail!("unexpected payload: {other:?}"),
    }

    let _ = client.shutdown();
    let _ = child.wait();
    Ok(())
}

use anyhow::Context as _;
