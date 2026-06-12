//! Chopsticks supervisor — owns the Chopsticks child process or attaches to a
//! running instance (ticket T02).

use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, anyhow};
use async_trait::async_trait;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::{Mutex, broadcast};

use crate::contracts::{BuildMode, ChopsticksSupervisor, ForkConfig, Result, WsEndpoint};

/// Pinned Chopsticks package spec used for `npx`.
const CHOPSTICKS_PKG: &str = "@acala-network/chopsticks@1.4.2";

/// Capacity of the boot/log broadcast channel.
const LOG_CHANNEL_CAPACITY: usize = 1024;

/// How long to wait for the listening line before giving up.
const START_TIMEOUT: Duration = Duration::from_secs(120);

/// Number of trailing log lines to attach to a startup-failure error.
const STDERR_TAIL_LINES: usize = 20;

/// Spawns/attaches and supervises Chopsticks (ticket T02).
pub struct Supervisor {
    /// Broadcasts every captured stdout/stderr line to log subscribers.
    log_tx: broadcast::Sender<String>,
    /// The spawned child, if any. `None` in attach mode or before `start`.
    child: Mutex<Option<Child>>,
}

impl Default for Supervisor {
    fn default() -> Self {
        Self::new()
    }
}

impl Supervisor {
    /// Creates an idle supervisor with no child process attached.
    pub fn new() -> Self {
        let (log_tx, _rx) = broadcast::channel(LOG_CHANNEL_CAPACITY);
        Self {
            log_tx,
            child: Mutex::new(None),
        }
    }

    /// Forwards a single log line to all current subscribers. A send error
    /// (no live receivers) is intentionally ignored.
    fn forward_log_line(&self, line: impl Into<String>) {
        let _ = self.log_tx.send(line.into());
    }

    /// Spawn path: launch Chopsticks via `npx`, capture its output, and resolve
    /// the listening ws endpoint.
    async fn start_spawn(
        &self,
        chain_or_path: &str,
        build_mode: BuildMode,
        mock_signature_host: bool,
    ) -> Result<WsEndpoint> {
        let argv = build_spawn_argv(chain_or_path, build_mode, mock_signature_host);

        let mut cmd = Command::new("npx");
        cmd.args(&argv)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        let mut child = cmd
            .spawn()
            .with_context(|| format!("failed to spawn `npx {}`", argv.join(" ")))?;

        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow!("child stdout was not captured"))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| anyhow!("child stderr was not captured"))?;

        // Merge stdout + stderr into one channel of (line, is_stderr) items so we
        // can scan both for the listening line and forward both to subscribers.
        let (line_tx, mut line_rx) = tokio::sync::mpsc::unbounded_channel::<(String, bool)>();

        spawn_line_reader(stdout, false, line_tx.clone());
        spawn_line_reader(stderr, true, line_tx);

        // Keep a rolling tail of stderr for diagnostics on failure.
        let mut stderr_tail: Vec<String> = Vec::new();

        let endpoint = tokio::time::timeout(START_TIMEOUT, async {
            loop {
                match line_rx.recv().await {
                    Some((line, is_stderr)) => {
                        self.forward_log_line(line.clone());
                        if is_stderr {
                            stderr_tail.push(line.clone());
                            if stderr_tail.len() > STDERR_TAIL_LINES {
                                stderr_tail.remove(0);
                            }
                        }
                        if let Some(ep) = parse_listening_line(&line) {
                            return Ok::<WsEndpoint, anyhow::Error>(ep);
                        }
                    }
                    // Both readers closed => child's streams ended, i.e. the
                    // process exited before we saw the listening line.
                    None => {
                        return Err(anyhow!(
                            "Chopsticks exited before listening.\n--- stderr tail ---\n{}",
                            stderr_tail.join("\n")
                        ));
                    }
                }
            }
        })
        .await;

        match endpoint {
            Ok(Ok(ep)) => {
                *self.child.lock().await = Some(child);
                Ok(ep)
            }
            Ok(Err(e)) => {
                let _ = child.start_kill();
                Err(e)
            }
            Err(_elapsed) => {
                let _ = child.start_kill();
                Err(anyhow!(
                    "timed out after {}s waiting for Chopsticks to listen.\n--- stderr tail ---\n{}",
                    START_TIMEOUT.as_secs(),
                    stderr_tail.join("\n")
                ))
            }
        }
    }
}

#[async_trait]
impl ChopsticksSupervisor for Supervisor {
    async fn start(&self, cfg: &ForkConfig) -> Result<WsEndpoint> {
        match cfg {
            ForkConfig::Attach { url } => Ok(WsEndpoint(url.clone())),
            ForkConfig::Spawn {
                chain_or_path,
                build_mode,
                mock_signature_host,
            } => {
                self.start_spawn(chain_or_path, *build_mode, *mock_signature_host)
                    .await
            }
        }
    }

    fn log_lines(&self) -> broadcast::Receiver<String> {
        self.log_tx.subscribe()
    }

    async fn shutdown(&self) -> Result<()> {
        if let Some(mut child) = self.child.lock().await.take() {
            child.kill().await.context("failed to kill Chopsticks child")?;
        }
        Ok(())
    }
}

/// Reads `reader` line by line, forwarding each `(line, is_stderr)` to `tx`. The
/// channel closes when the stream ends (EOF on the captured pipe).
fn spawn_line_reader<R>(reader: R, is_stderr: bool, tx: tokio::sync::mpsc::UnboundedSender<(String, bool)>)
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let mut lines = BufReader::new(reader).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if tx.send((line, is_stderr)).is_err() {
                break;
            }
        }
    });
}

/// Maps a [`BuildMode`] to the value Chopsticks expects after
/// `--build-block-mode`.
fn build_block_mode_arg(mode: BuildMode) -> &'static str {
    match mode {
        BuildMode::Manual => "Manual",
        BuildMode::Instant => "Instant",
        BuildMode::Batch => "Batch",
    }
}

/// Builds the full argv for `npx` to spawn Chopsticks in spawn mode.
///
/// Pure: testable without a process. Mirrors:
/// `npx --yes <pkg> -c <chain_or_path> --build-block-mode <mode>` plus
/// `--mock-signature-host` when requested. The `dev --chain …` form was removed
/// in Chopsticks 1.x, so `-c <name|path>` is used.
fn build_spawn_argv(
    chain_or_path: &str,
    build_mode: BuildMode,
    mock_signature_host: bool,
) -> Vec<String> {
    let mut argv = vec![
        "--yes".to_string(),
        CHOPSTICKS_PKG.to_string(),
        "-c".to_string(),
        chain_or_path.to_string(),
        "--build-block-mode".to_string(),
        build_block_mode_arg(build_mode).to_string(),
    ];
    if mock_signature_host {
        argv.push("--mock-signature-host".to_string());
    }
    argv
}

/// Scans a single log line for the Chopsticks "listening" marker and, if found,
/// returns the resolved local ws endpoint.
///
/// Matches lines like `… RPC listening on … ws://[::]:8000` and resolves them to
/// `ws://localhost:<port>`. Non-matching lines return `None`.
fn parse_listening_line(line: &str) -> Option<WsEndpoint> {
    // Require both the listening marker and a ws url to avoid false positives.
    if !line.contains("listening") {
        return None;
    }
    let idx = line.find("ws://")?;
    let rest = &line[idx + "ws://".len()..];
    // The host part may be `[::]`, `127.0.0.1`, `0.0.0.0`, etc. The port is the
    // segment after the last ':' up to the first non-digit.
    let colon = rest.rfind(':')?;
    let port: String = rest[colon + 1..]
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    if port.is_empty() {
        return None;
    }
    Some(WsEndpoint(format!("ws://localhost:{port}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spawn_argv_includes_manual_mode_and_chain() {
        let argv = build_spawn_argv("polkadot", BuildMode::Manual, false);
        assert!(argv.contains(&CHOPSTICKS_PKG.to_string()));
        assert!(argv.contains(&"-c".to_string()));
        assert!(argv.contains(&"polkadot".to_string()));
        assert!(argv.contains(&"--build-block-mode".to_string()));
        assert!(argv.contains(&"Manual".to_string()));
        // -c must be immediately followed by the chain.
        let c_idx = argv.iter().position(|a| a == "-c").unwrap();
        assert_eq!(argv[c_idx + 1], "polkadot");
    }

    #[test]
    fn spawn_argv_adds_mock_signature_host_when_enabled() {
        let argv = build_spawn_argv("./acala.yml", BuildMode::Manual, true);
        assert!(argv.contains(&"--mock-signature-host".to_string()));
    }

    #[test]
    fn spawn_argv_omits_mock_signature_host_when_disabled() {
        let argv = build_spawn_argv("./acala.yml", BuildMode::Manual, false);
        assert!(!argv.contains(&"--mock-signature-host".to_string()));
    }

    #[test]
    fn parse_listening_line_extracts_ws_endpoint() {
        let line =
            "2024-01-01 [00:00:00.000] info: RPC listening on http://[::]:8000 and ws://[::]:8000";
        let ep = parse_listening_line(line).expect("should match listening line");
        assert_eq!(ep, WsEndpoint("ws://localhost:8000".to_string()));

        // Non-matching lines yield None (incl. the transient disconnect line).
        assert!(parse_listening_line("API-WS: disconnected").is_none());
        assert!(parse_listening_line("info: starting up").is_none());
        assert!(parse_listening_line("listening but no url here").is_none());
    }

    #[tokio::test]
    async fn attach_returns_url_without_spawning() {
        let sup = Supervisor::new();
        let cfg = ForkConfig::Attach {
            url: "ws://example:9944".to_string(),
        };
        let ep = sup.start(&cfg).await.expect("attach should succeed");
        assert_eq!(ep, WsEndpoint("ws://example:9944".to_string()));
        // No child should have been recorded.
        assert!(sup.child.lock().await.is_none());
    }

    #[tokio::test]
    async fn log_lines_receiver_observes_forwarded_lines() {
        let sup = Supervisor::new();
        let mut rx = sup.log_lines();
        // Push canned lines through the same forwarding path used by the child
        // reader.
        sup.forward_log_line("boot: line one");
        sup.forward_log_line("boot: line two");
        assert_eq!(rx.recv().await.unwrap(), "boot: line one");
        assert_eq!(rx.recv().await.unwrap(), "boot: line two");
    }
}
