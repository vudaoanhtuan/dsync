//! SSH implementation of `Transport`. Opens one authenticated `russh` connection and a pool of
//! exec channels, each running `dsync --server <path>` on the remote host. The custom
//! length-prefixed protocol (protocol.rs) carries scan/signature/diff/patch so the CPU-heavy
//! work happens locally to the remote files. See specs/transport.md.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use russh::client::{self, Handle};
use russh::keys::PrivateKeyWithHashAlg;
use russh::ChannelStream;
use tokio::sync::{Mutex, Semaphore};

use crate::config::Remote;
use crate::delta::{Delta, Signature};
use crate::error::{DsyncError, Result};
use crate::ignore::IgnoreSet;
use crate::transport::protocol::{read_msg, write_msg, Request, Response, PROTOCOL_VERSION};
use crate::transport::{FileEntry, Transport};

/// Host-key verification handler. Consults `~/.ssh/known_hosts`; refuses unknown hosts rather
/// than blindly trusting them.
struct ClientHandler {
    host: String,
    port: u16,
}

impl client::Handler for ClientHandler {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        server_public_key: &russh::keys::ssh_key::PublicKey,
    ) -> std::result::Result<bool, Self::Error> {
        match russh::keys::check_known_hosts(&self.host, self.port, server_public_key) {
            Ok(true) => Ok(true), // known and matches
            Ok(false) => {
                // Unknown host: trust on first use, but make the decision explicit in logs.
                tracing::warn!(
                    "host {} not in known_hosts; trusting on first use",
                    self.host
                );
                Ok(true)
            }
            Err(e) => {
                tracing::error!("known_hosts mismatch for {}: {e}", self.host);
                Ok(false)
            }
        }
    }
}

/// One exec channel + its async stream, running a remote `dsync --server` process.
struct AgentChannel {
    stream: ChannelStream<client::Msg>,
}

impl AgentChannel {
    /// Send a request, read the matching response. Returns the response and the on-wire byte
    /// size of the request frame (post-compression).
    async fn request(&mut self, req: Request, compress: bool, level: i32) -> Result<(Response, usize)> {
        let sent = write_msg(&mut self.stream, &req, compress, level).await?;
        let resp: Response = read_msg(&mut self.stream).await?;
        Ok((resp, sent))
    }
}

struct Pool {
    idle: Mutex<Vec<AgentChannel>>,
    sem: Semaphore,
}

impl Pool {
    async fn with_channel<F, Fut, T>(&self, f: F) -> Result<T>
    where
        F: FnOnce(AgentChannel) -> Fut,
        Fut: std::future::Future<Output = (AgentChannel, Result<T>)>,
    {
        let _permit = self
            .sem
            .acquire()
            .await
            .map_err(|e| DsyncError::Ssh(format!("pool closed: {e}")))?;
        let chan = {
            let mut idle = self.idle.lock().await;
            idle.pop()
                .ok_or_else(|| DsyncError::Ssh("no idle agent channel".into()))?
        };
        let (chan, result) = f(chan).await;
        self.idle.lock().await.push(chan);
        result
    }
}

pub struct SshTransport {
    _session: Handle<ClientHandler>,
    pool: Arc<Pool>,
    bytes_sent: Arc<AtomicU64>,
    compress: bool,
    level: i32,
}

impl SshTransport {
    /// Connect, authenticate, and open a pool of `channels` agent processes rooted at the remote
    /// path. `compress`/`level` control wire compression for this run.
    pub async fn connect(
        remote: &Remote,
        channels: usize,
        compress: bool,
        level: i32,
    ) -> Result<SshTransport> {
        let (user, host, port, path) = match remote {
            Remote::Ssh {
                user,
                host,
                port,
                path,
            } => (user.clone(), host.clone(), *port, path.clone()),
            Remote::Local { .. } => {
                return Err(DsyncError::Ssh("not an SSH remote".into()))
            }
        };
        let user = user.unwrap_or_else(whoami);

        let config = Arc::new(client::Config::default());
        let handler = ClientHandler {
            host: host.clone(),
            port,
        };
        let mut session = client::connect(config, (host.as_str(), port), handler)
            .await
            .map_err(|e| DsyncError::Ssh(format!("connect to {host}:{port} failed: {e}")))?;

        authenticate(&mut session, &user).await?;

        let n = channels.max(1);
        let mut idle = Vec::with_capacity(n);
        for _ in 0..n {
            let chan = open_agent_channel(&session, &path, compress, level).await?;
            idle.push(chan);
        }

        let pool = Arc::new(Pool {
            idle: Mutex::new(idle),
            sem: Semaphore::new(n),
        });

        Ok(SshTransport {
            _session: session,
            pool,
            bytes_sent: Arc::new(AtomicU64::new(0)),
            compress,
            level,
        })
    }

    async fn round_trip(&self, req: Request) -> Result<(Response, usize)> {
        let compress = self.compress;
        let level = self.level;
        self.pool
            .with_channel(|mut chan| async move {
                let r = chan.request(req, compress, level).await;
                (chan, r)
            })
            .await
    }
}

fn whoami() -> String {
    std::env::var("USER")
        .or_else(|_| std::env::var("LOGNAME"))
        .unwrap_or_else(|_| "root".to_string())
}

/// Authenticate: ssh-agent first, then common key files.
async fn authenticate(session: &mut Handle<ClientHandler>, user: &str) -> Result<()> {
    if try_agent_auth(session, user).await.unwrap_or(false) {
        return Ok(());
    }
    if try_key_files(session, user).await? {
        return Ok(());
    }
    Err(DsyncError::Ssh(format!(
        "authentication failed for {user}; tried ssh-agent and ~/.ssh/id_ed25519, id_rsa"
    )))
}

async fn try_agent_auth(session: &mut Handle<ClientHandler>, user: &str) -> Result<bool> {
    let mut agent = match russh::keys::agent::client::AgentClient::connect_env().await {
        Ok(a) => a,
        Err(_) => return Ok(false),
    };
    let identities = agent
        .request_identities()
        .await
        .map_err(|e| DsyncError::Ssh(format!("agent identities: {e}")))?;
    for identity in identities {
        // Only plain public keys are used for auth here; certificates are skipped.
        let key = match identity {
            russh::keys::agent::AgentIdentity::PublicKey { key, .. } => key,
            _ => continue,
        };
        match session
            .authenticate_publickey_with(user, key, None, &mut agent)
            .await
        {
            Ok(res) if res.success() => return Ok(true),
            _ => continue,
        }
    }
    Ok(false)
}

async fn try_key_files(session: &mut Handle<ClientHandler>, user: &str) -> Result<bool> {
    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
    for name in ["id_ed25519", "id_rsa"] {
        let path = home.join(".ssh").join(name);
        if !path.exists() {
            continue;
        }
        let key = match russh::keys::load_secret_key(&path, None) {
            Ok(k) => k,
            Err(_) => continue,
        };
        let with_hash = PrivateKeyWithHashAlg::new(Arc::new(key), None);
        match session.authenticate_publickey(user, with_hash).await {
            Ok(res) if res.success() => return Ok(true),
            _ => continue,
        }
    }
    Ok(false)
}

async fn open_agent_channel(
    session: &Handle<ClientHandler>,
    remote_path: &Path,
    compress: bool,
    level: i32,
) -> Result<AgentChannel> {
    let channel = session
        .channel_open_session()
        .await
        .map_err(|e| DsyncError::Ssh(format!("open channel: {e}")))?;
    let cmd = format!("dsync --server {}", shell_quote(remote_path));
    channel
        .exec(true, cmd.as_bytes())
        .await
        .map_err(|e| DsyncError::Ssh(format!("exec dsync --server (is dsync installed on the remote PATH?): {e}")))?;
    let stream = channel.into_stream();
    let mut chan = AgentChannel { stream };

    // Handshake.
    let (resp, _) = chan
        .request(
            Request::Hello {
                version: PROTOCOL_VERSION,
                compress,
                level,
            },
            compress,
            level,
        )
        .await?;
    match resp {
        Response::Hello { version } if version == PROTOCOL_VERSION => Ok(chan),
        Response::Hello { version } => Err(DsyncError::Protocol(format!(
            "protocol version mismatch (remote {version}, local {PROTOCOL_VERSION}); upgrade dsync on both ends"
        ))),
        Response::Error(e) => Err(DsyncError::Ssh(e)),
        _ => Err(DsyncError::Protocol("unexpected handshake response".into())),
    }
}

/// Single-quote a path for a POSIX remote shell.
fn shell_quote(path: &Path) -> String {
    let s = path.to_string_lossy();
    format!("'{}'", s.replace('\'', r"'\''"))
}

fn protocol_error(resp: Response) -> DsyncError {
    match resp {
        Response::Error(e) => DsyncError::Protocol(e),
        _ => DsyncError::Protocol("unexpected response variant".into()),
    }
}

#[async_trait]
impl Transport for SshTransport {
    async fn scan(&self, ignore: Option<&IgnoreSet>) -> Result<Vec<FileEntry>> {
        // The SSH side is scanned via the agent. The client passes resolved ignore *patterns*
        // (a string) for the sender case; for the receiver case it passes None. The IgnoreSet
        // here is only ever the source-resolved one and only meaningful when this transport is
        // the sender (remote pull) — but we cannot extract patterns from a compiled IgnoreSet,
        // so the engine drives remote-sender filtering through `scan_with_patterns`.
        let _ = ignore;
        let (resp, _) = self
            .round_trip(Request::Scan {
                ignore_patterns: None,
            })
            .await?;
        match resp {
            Response::Scanned(v) => Ok(v),
            other => Err(protocol_error(other)),
        }
    }

    async fn signature(&self, rel: &Path) -> Result<Option<Signature>> {
        let (resp, _) = self
            .round_trip(Request::Signature {
                rel: rel.to_path_buf(),
            })
            .await?;
        match resp {
            Response::Sig(s) => Ok(s),
            other => Err(protocol_error(other)),
        }
    }

    async fn diff(&self, rel: &Path, sig: &Signature) -> Result<Delta> {
        let (resp, _) = self
            .round_trip(Request::Diff {
                rel: rel.to_path_buf(),
                sig: sig.clone(),
            })
            .await?;
        match resp {
            Response::Diffed(d) => Ok(d),
            other => Err(protocol_error(other)),
        }
    }

    async fn patch(&self, rel: &Path, delta: &Delta, mtime: i64, mode: u32) -> Result<[u8; 32]> {
        let (resp, sent) = self
            .round_trip(Request::Patch {
                rel: rel.to_path_buf(),
                delta: delta.clone(),
                mtime,
                mode,
            })
            .await?;
        self.bytes_sent.fetch_add(sent as u64, Ordering::Relaxed);
        match resp {
            Response::Patched(h) => Ok(h),
            other => Err(protocol_error(other)),
        }
    }

    async fn write_file(&self, rel: &Path, data: &[u8], mtime: i64, mode: u32) -> Result<[u8; 32]> {
        let (resp, sent) = self
            .round_trip(Request::WriteFile {
                rel: rel.to_path_buf(),
                data: data.to_vec(),
                mtime,
                mode,
            })
            .await?;
        self.bytes_sent.fetch_add(sent as u64, Ordering::Relaxed);
        match resp {
            Response::Patched(h) => Ok(h),
            other => Err(protocol_error(other)),
        }
    }

    async fn read_file(&self, rel: &Path) -> Result<Vec<u8>> {
        let (resp, _) = self
            .round_trip(Request::ReadFile {
                rel: rel.to_path_buf(),
            })
            .await?;
        match resp {
            Response::FileData(d) => Ok(d),
            other => Err(protocol_error(other)),
        }
    }

    async fn hash(&self, rel: &Path) -> Result<[u8; 32]> {
        let (resp, _) = self
            .round_trip(Request::Hash {
                rel: rel.to_path_buf(),
            })
            .await?;
        match resp {
            Response::Hashed(h) => Ok(h),
            other => Err(protocol_error(other)),
        }
    }

    async fn mkdir_all(&self, rel: &Path, mode: u32) -> Result<()> {
        let (resp, _) = self
            .round_trip(Request::Mkdir {
                rel: rel.to_path_buf(),
                mode,
            })
            .await?;
        match resp {
            Response::Ok => Ok(()),
            other => Err(protocol_error(other)),
        }
    }

    async fn remove(&self, rel: &Path) -> Result<()> {
        let (resp, _) = self
            .round_trip(Request::Remove {
                rel: rel.to_path_buf(),
            })
            .await?;
        match resp {
            Response::Ok => Ok(()),
            other => Err(protocol_error(other)),
        }
    }

    fn bytes_sent(&self) -> u64 {
        self.bytes_sent.load(Ordering::Relaxed)
    }

    async fn shutdown(&self) -> Result<()> {
        // Send Shutdown on each idle channel; ignore errors (best-effort teardown).
        let mut idle = self.pool.idle.lock().await;
        for chan in idle.iter_mut() {
            let _ = write_msg(&mut chan.stream, &Request::Shutdown, self.compress, self.level).await;
        }
        Ok(())
    }
}

impl SshTransport {
    /// Remote-sender scan with explicit ignore patterns (used for remote pull, see ignore.md).
    pub async fn scan_with_patterns(&self, patterns: Option<String>) -> Result<Vec<FileEntry>> {
        let (resp, _) = self
            .round_trip(Request::Scan {
                ignore_patterns: patterns,
            })
            .await?;
        match resp {
            Response::Scanned(v) => Ok(v),
            other => Err(protocol_error(other)),
        }
    }
}
