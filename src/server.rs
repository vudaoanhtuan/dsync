//! Remote-agent mode (`dsync --server <root>`). Reads length-prefixed protocol frames from
//! stdin, executes each against a `LocalTransport` rooted at the remote path, and writes the
//! matching response to stdout. Never invoked by users directly. See specs/transport.md.

use std::path::PathBuf;

use tokio::io::{AsyncRead, AsyncWrite};

use crate::error::{DsyncError, Result};
use crate::ignore::IgnoreSet;
use crate::transport::protocol::{read_msg, write_msg, Request, Response, PROTOCOL_VERSION};
use crate::transport::{local::LocalTransport, Transport};

/// Entry point for `dsync --server <root>`. Loops until `Shutdown` or EOF.
pub async fn run(root: PathBuf) -> Result<()> {
    let transport = LocalTransport::new(root.clone());
    let mut stdin = tokio::io::stdin();
    let mut stdout = tokio::io::stdout();

    // Response compression settings, learned from the client's Hello.
    let mut compress = true;
    let mut level = 3;

    loop {
        let req: Request = match read_msg(&mut stdin).await {
            Ok(r) => r,
            Err(_) => break, // EOF / closed channel
        };
        let resp = match req {
            Request::Hello {
                version,
                compress: c,
                level: l,
            } => {
                compress = c;
                level = l;
                if version != PROTOCOL_VERSION {
                    Response::Error(format!(
                        "protocol version mismatch (remote {PROTOCOL_VERSION}, client {version}); upgrade dsync on both ends"
                    ))
                } else {
                    Response::Hello {
                        version: PROTOCOL_VERSION,
                    }
                }
            }
            Request::Shutdown => break,
            other => handle(&transport, &root, other).await,
        };
        write_msg(&mut stdout, &resp, compress, level).await?;
    }
    Ok(())
}

async fn handle(transport: &LocalTransport, root: &PathBuf, req: Request) -> Response {
    match exec(transport, root, req).await {
        Ok(r) => r,
        Err(e) => Response::Error(e.to_string()),
    }
}

async fn exec(transport: &LocalTransport, root: &PathBuf, req: Request) -> Result<Response> {
    Ok(match req {
        Request::Scan { ignore_patterns } => {
            let entries = match ignore_patterns {
                Some(patterns) => {
                    let set = IgnoreSet::from_patterns(root, &patterns)?;
                    transport.scan(Some(&set)).await?
                }
                None => transport.scan(None).await?,
            };
            Response::Scanned(entries)
        }
        Request::Signature { rel } => Response::Sig(transport.signature(&rel).await?),
        Request::Diff { rel, sig } => Response::Diffed(transport.diff(&rel, &sig).await?),
        Request::Patch {
            rel,
            delta,
            mtime,
            mode,
        } => Response::Patched(transport.patch(&rel, &delta, mtime, mode).await?),
        Request::WriteFile {
            rel,
            data,
            mtime,
            mode,
        } => Response::Patched(transport.write_file(&rel, &data, mtime, mode).await?),
        Request::ReadFile { rel } => Response::FileData(transport.read_file(&rel).await?),
        Request::Hash { rel } => Response::Hashed(transport.hash(&rel).await?),
        Request::Mkdir { rel, mode } => {
            transport.mkdir_all(&rel, mode).await?;
            Response::Ok
        }
        Request::Remove { rel } => {
            transport.remove(&rel).await?;
            Response::Ok
        }
        Request::Hello { .. } | Request::Shutdown => {
            return Err(DsyncError::Protocol("unexpected control frame".into()))
        }
    })
}

/// Allow the agent to read/write over arbitrary async streams in unit tests.
#[allow(dead_code)]
pub async fn run_with_streams<R, W>(root: PathBuf, mut reader: R, mut writer: W) -> Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let transport = LocalTransport::new(root.clone());
    let mut compress = true;
    let mut level = 3;
    loop {
        let req: Request = match read_msg(&mut reader).await {
            Ok(r) => r,
            Err(_) => break,
        };
        let resp = match req {
            Request::Hello {
                version,
                compress: c,
                level: l,
            } => {
                compress = c;
                level = l;
                let _ = version;
                Response::Hello {
                    version: PROTOCOL_VERSION,
                }
            }
            Request::Shutdown => break,
            other => handle(&transport, &root, other).await,
        };
        write_msg(&mut writer, &resp, compress, level).await?;
    }
    Ok(())
}
