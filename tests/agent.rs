//! Exercise the remote-agent loop (`server::run_with_streams`) and the wire protocol over an
//! in-memory duplex — the same code path the SSH transport drives, without needing sshd.

use std::path::PathBuf;

use dsync::transport::protocol::{read_msg, write_msg, Request, Response, PROTOCOL_VERSION};
use tokio::io::split;

async fn send(
    w: &mut (impl tokio::io::AsyncWrite + Unpin),
    r: &mut (impl tokio::io::AsyncRead + Unpin),
    req: Request,
) -> Response {
    write_msg(w, &req, true, 3).await.unwrap();
    read_msg(r).await.unwrap()
}

#[tokio::test]
async fn agent_full_protocol_roundtrip() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("basis.txt"), b"the quick brown fox").unwrap();

    let (client, server) = tokio::io::duplex(1 << 20);
    let (sr, sw) = split(server);
    let root_path = root.path().to_path_buf();
    let agent = tokio::spawn(async move {
        dsync::server::run_with_streams(root_path, sr, sw).await.unwrap();
    });

    let (mut cr, mut cw) = split(client);

    // Handshake.
    let resp = send(
        &mut cw,
        &mut cr,
        Request::Hello {
            version: PROTOCOL_VERSION,
            compress: true,
            level: 3,
        },
    )
    .await;
    assert!(matches!(resp, Response::Hello { version } if version == PROTOCOL_VERSION));

    // Scan (no ignore) should see the basis file.
    let resp = send(&mut cw, &mut cr, Request::Scan { ignore_patterns: None }).await;
    match resp {
        Response::Scanned(entries) => {
            assert!(entries.iter().any(|e| e.rel_path == PathBuf::from("basis.txt")));
        }
        other => panic!("expected Scanned, got {other:?}"),
    }

    // WriteFile a brand-new file via the whole-file path.
    let new_data = b"hello from the client".to_vec();
    let resp = send(
        &mut cw,
        &mut cr,
        Request::WriteFile {
            rel: "new.txt".into(),
            data: new_data.clone(),
            mtime: 1_700_000_000_000,
            mode: 0o644,
        },
    )
    .await;
    let expected = *blake3::hash(&new_data).as_bytes();
    assert!(matches!(resp, Response::Patched(h) if h == expected));
    assert_eq!(std::fs::read(root.path().join("new.txt")).unwrap(), new_data);

    // Signature → Diff → Patch round trip updating basis.txt.
    let sig = match send(&mut cw, &mut cr, Request::Signature { rel: "basis.txt".into() }).await {
        Response::Sig(Some(s)) => s,
        other => panic!("expected Sig, got {other:?}"),
    };
    // The "sender" here is the client: compute the delta locally from the new content.
    let updated = b"the quick brown fox jumps over the lazy dog".to_vec();
    let delta = dsync::delta::diff(&sig, &updated).unwrap();
    let resp = send(
        &mut cw,
        &mut cr,
        Request::Patch {
            rel: "basis.txt".into(),
            delta,
            mtime: 1_700_000_000_000,
            mode: 0o644,
        },
    )
    .await;
    let expected = *blake3::hash(&updated).as_bytes();
    assert!(matches!(resp, Response::Patched(h) if h == expected));
    assert_eq!(std::fs::read(root.path().join("basis.txt")).unwrap(), updated);

    // Hash + ReadFile.
    let resp = send(&mut cw, &mut cr, Request::Hash { rel: "new.txt".into() }).await;
    assert!(matches!(resp, Response::Hashed(_)));
    let resp = send(&mut cw, &mut cr, Request::ReadFile { rel: "new.txt".into() }).await;
    assert!(matches!(resp, Response::FileData(d) if d == new_data));

    // Mkdir + Remove.
    assert!(matches!(
        send(&mut cw, &mut cr, Request::Mkdir { rel: "d".into(), mode: 0o755 }).await,
        Response::Ok
    ));
    assert!(root.path().join("d").is_dir());
    assert!(matches!(
        send(&mut cw, &mut cr, Request::Remove { rel: "new.txt".into() }).await,
        Response::Ok
    ));
    assert!(!root.path().join("new.txt").exists());

    // Shutdown ends the loop.
    write_msg(&mut cw, &Request::Shutdown, true, 3).await.unwrap();
    agent.await.unwrap();
}
