//! Remote-agent wire protocol: length-prefixed, optionally-zstd-compressed `postcard` frames.
//! Shared by the SSH client (`ssh.rs`) and the remote agent (`server.rs`). See specs/transport.md.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::delta::{Delta, Signature};
use crate::error::{DsyncError, Result};
use crate::transport::FileEntry;

/// Bumped on any incompatible wire change.
pub const PROTOCOL_VERSION: u32 = 1;

/// Payloads below this size skip compression (overhead not worth it).
const COMPRESS_THRESHOLD: usize = 256;

const FLAG_RAW: u8 = 0;
const FLAG_ZSTD: u8 = 1;

#[derive(Serialize, Deserialize, Debug)]
pub enum Request {
    /// Handshake: client announces its protocol version and compression preference. The agent
    /// replies `Hello` and uses these settings when encoding its responses.
    Hello {
        version: u32,
        compress: bool,
        level: i32,
    },
    /// Scan the remote root. `ignore_patterns` is None when the remote is the RECEIVER
    /// (scan everything); Some(patterns) when it is the SENDER (remote pull).
    Scan { ignore_patterns: Option<String> },
    Signature { rel: PathBuf },
    Diff { rel: PathBuf, sig: Signature },
    Patch {
        rel: PathBuf,
        delta: Delta,
        mtime: i64,
        mode: u32,
    },
    WriteFile {
        rel: PathBuf,
        data: Vec<u8>,
        mtime: i64,
        mode: u32,
    },
    ReadFile { rel: PathBuf },
    Hash { rel: PathBuf },
    Mkdir { rel: PathBuf, mode: u32 },
    Remove { rel: PathBuf },
    Shutdown,
}

#[derive(Serialize, Deserialize, Debug)]
pub enum Response {
    Hello { version: u32 },
    Scanned(Vec<FileEntry>),
    Sig(Option<Signature>),
    Diffed(Delta),
    Patched([u8; 32]),
    FileData(Vec<u8>),
    Hashed([u8; 32]),
    Ok,
    Error(String),
}

/// Encode a value to a framed, possibly-compressed byte buffer.
fn encode<T: Serialize>(msg: &T, compress: bool, level: i32) -> Result<Vec<u8>> {
    let payload =
        postcard::to_allocvec(msg).map_err(|e| DsyncError::Protocol(format!("encode: {e}")))?;
    let (flag, body) = if compress && payload.len() > COMPRESS_THRESHOLD {
        let compressed = zstd::encode_all(payload.as_slice(), level)
            .map_err(|e| DsyncError::Protocol(format!("zstd encode: {e}")))?;
        (FLAG_ZSTD, compressed)
    } else {
        (FLAG_RAW, payload)
    };
    let mut frame = Vec::with_capacity(body.len() + 5);
    frame.push(flag);
    frame.extend_from_slice(&(body.len() as u32).to_le_bytes());
    frame.extend_from_slice(&body);
    Ok(frame)
}

pub async fn write_msg<W, T>(w: &mut W, msg: &T, compress: bool, level: i32) -> Result<usize>
where
    W: AsyncWrite + Unpin,
    T: Serialize,
{
    let frame = encode(msg, compress, level)?;
    let n = frame.len();
    w.write_all(&frame)
        .await
        .map_err(|e| DsyncError::Protocol(format!("write: {e}")))?;
    w.flush()
        .await
        .map_err(|e| DsyncError::Protocol(format!("flush: {e}")))?;
    Ok(n)
}

pub async fn read_msg<R, T>(r: &mut R) -> Result<T>
where
    R: AsyncRead + Unpin,
    T: for<'de> Deserialize<'de>,
{
    let mut flag = [0u8; 1];
    r.read_exact(&mut flag)
        .await
        .map_err(|e| DsyncError::Protocol(format!("read flag: {e}")))?;
    let mut len_buf = [0u8; 4];
    r.read_exact(&mut len_buf)
        .await
        .map_err(|e| DsyncError::Protocol(format!("read len: {e}")))?;
    let len = u32::from_le_bytes(len_buf) as usize;
    let mut body = vec![0u8; len];
    r.read_exact(&mut body)
        .await
        .map_err(|e| DsyncError::Protocol(format!("read body: {e}")))?;
    let payload = match flag[0] {
        FLAG_RAW => body,
        FLAG_ZSTD => zstd::decode_all(body.as_slice())
            .map_err(|e| DsyncError::Protocol(format!("zstd decode: {e}")))?,
        other => return Err(DsyncError::Protocol(format!("unknown frame flag {other}"))),
    };
    postcard::from_bytes(&payload).map_err(|e| DsyncError::Protocol(format!("decode: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::EntryKind;

    fn roundtrip_req(req: Request, compress: bool) {
        let frame = encode(&req, compress, 3).unwrap();
        // strip header and decode through the same path as read_msg
        let flag = frame[0];
        let len = u32::from_le_bytes(frame[1..5].try_into().unwrap()) as usize;
        let body = &frame[5..5 + len];
        let payload = if flag == FLAG_ZSTD {
            zstd::decode_all(body).unwrap()
        } else {
            body.to_vec()
        };
        let _decoded: Request = postcard::from_bytes(&payload).unwrap();
    }

    #[test]
    fn all_request_variants_roundtrip() {
        roundtrip_req(
            Request::Hello {
                version: 1,
                compress: true,
                level: 3,
            },
            false,
        );
        roundtrip_req(Request::Scan { ignore_patterns: None }, false);
        roundtrip_req(
            Request::Scan {
                ignore_patterns: Some("*.log\n".into()),
            },
            true,
        );
        roundtrip_req(Request::Signature { rel: "a/b.txt".into() }, false);
        roundtrip_req(
            Request::Diff {
                rel: "a.txt".into(),
                sig: Signature(vec![1, 2, 3]),
            },
            false,
        );
        roundtrip_req(
            Request::Patch {
                rel: "a.txt".into(),
                delta: Delta(vec![9; 1000]),
                mtime: 123,
                mode: 0o644,
            },
            true,
        );
        roundtrip_req(
            Request::WriteFile {
                rel: "a.txt".into(),
                data: vec![7; 1000],
                mtime: 1,
                mode: 0o644,
            },
            true,
        );
        roundtrip_req(Request::ReadFile { rel: "a".into() }, false);
        roundtrip_req(Request::Hash { rel: "a".into() }, false);
        roundtrip_req(Request::Mkdir { rel: "d".into(), mode: 0o755 }, false);
        roundtrip_req(Request::Remove { rel: "a".into() }, false);
        roundtrip_req(Request::Shutdown, false);
    }

    #[test]
    fn response_variants_roundtrip() {
        let entries = vec![FileEntry {
            rel_path: "a.txt".into(),
            len: 10,
            mtime: 5,
            kind: EntryKind::File,
            mode: 0o644,
        }];
        let resp = Response::Scanned(entries);
        let frame = encode(&resp, true, 3).unwrap();
        let len = u32::from_le_bytes(frame[1..5].try_into().unwrap()) as usize;
        let body = &frame[5..5 + len];
        let payload = if frame[0] == FLAG_ZSTD {
            zstd::decode_all(body).unwrap()
        } else {
            body.to_vec()
        };
        let decoded: Response = postcard::from_bytes(&payload).unwrap();
        matches!(decoded, Response::Scanned(_));
    }
}
