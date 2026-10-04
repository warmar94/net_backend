//! Files against the real server's `files` module on loopback: uploads streamed from disk and from
//! memory with progress (multipart: the meta part and the file part), downloads into memory and to a
//! file with progress (checked against the SHA-256), the typed calls (list, usage, settings,
//! delete), who may read, the server's refusals as errors, an expired access token refreshed
//! before the upload, a cancelled download leaving no part file, the blocking client.

mod common;

use std::path::PathBuf;
use std::time::Duration;

use common::{email, Server, PASSWORD};
use net_backend_client::files::{DownloadOptions, FileUpload, TransferProgress};
use net_backend_client::protocol::auth::RegisterRequest;
use net_backend_client::protocol::files::{DeleteFile, EditFile, FileMeta, FileQuery, FileVisibility, GetFile, GetFileUsage, ListFiles, UpdateFile};
use net_backend_client::protocol::{codes, FileId};
use net_backend_client::{Client, Error};

fn temp(name: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("target")
        .join("tmp")
        .join(format!("client-files-test-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("dir");
    dir
}

fn sha(bytes: &[u8]) -> String {
    ring::digest::digest(&ring::digest::SHA256, bytes).as_ref().iter().map(|b| format!("{b:02x}")).collect()
}

async fn player(server: &Server, name: &str) -> Client {
    let client = server.client();
    client.register(RegisterRequest::new(email(name), PASSWORD)).await.expect("register");
    client
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn uploads_and_downloads_with_progress() {
    let server = Server::start();
    let alice = player(&server, "files-alice").await;
    let bob = player(&server, "files-bob").await;
    let dir = temp("roundtrip");
    let bytes: Vec<u8> = (0..3_000_000u32).map(|i| (i.wrapping_mul(7) % 253) as u8).collect();
    let source = dir.join("replay-17.replay");
    std::fs::write(&source, &bytes).expect("write");

    // From disk, with progress and settings; the server's SHA-256 is the local one.
    let upload = FileUpload::path(&source)
        .content_type("application/x-replay")
        .meta(FileMeta::new().with_visibility(FileVisibility::Public).with_metadata(serde_json::json!({"map": "caves"})).with_sha256(sha(&bytes)));
    let mut transfer = alice.start_upload(upload);
    let mut reports: Vec<TransferProgress> = Vec::new();
    while let Some(progress) = transfer.next_progress().await {
        reports.push(progress);
    }
    let info = transfer.finish().await.expect("uploaded");
    assert_eq!((info.name.as_str(), info.content_type.as_str(), info.size), ("replay-17.replay", "application/x-replay", 3_000_000));
    assert_eq!(info.sha256, sha(&bytes));
    assert!(reports.windows(2).all(|w| w[0].done <= w[1].done), "progress only grows");
    assert_eq!(reports.last().map(|p| (p.done, p.total)), Some((3_000_000, Some(3_000_000))));
    assert!(reports.len() > 3, "several reports: {}", reports.len());

    // Into memory (also for another player: it is public), to a file with progress.
    assert!(alice.download_file(info.id, DownloadOptions::default()).await.expect("download") == bytes);
    assert!(bob.download_file(info.id, DownloadOptions::default()).await.expect("public") == bytes);
    let target = dir.join("copy.replay");
    let mut download = bob.start_download_to(info.id, &target, DownloadOptions::default());
    let mut last = TransferProgress::default();
    while let Some(progress) = download.next_progress().await {
        last = progress;
    }
    assert_eq!(download.finish().await.expect("to a file"), 3_000_000);
    assert_eq!((last.done, last.total), (3_000_000, Some(3_000_000)));
    assert!(std::fs::read(&target).expect("read") == bytes);
    assert!(!dir.join("copy.replay.part").exists());
    let mut small = DownloadOptions::default();
    small.max_bytes = 1000;
    assert!(matches!(bob.download_file(info.id, small).await, Err(Error::BodyTooLarge { limit: 1000, .. })));

    // From memory; the typed calls.
    let shot = alice.upload_file(FileUpload::bytes("shot.png", vec![0x89, b'P', b'N', b'G']).content_type("image/png")).await.expect("bytes");
    assert_eq!((shot.size, shot.visibility), (4, FileVisibility::Private));
    let mine = alice.call(&ListFiles::new()).await.expect("list");
    assert_eq!(mine.items.iter().map(|f| f.id).collect::<Vec<_>>(), vec![shot.id, info.id]);
    let theirs = bob.call(&ListFiles::new().with_query(FileQuery::of(info.owner))).await.expect("list theirs");
    assert_eq!(theirs.items.iter().map(|f| f.id).collect::<Vec<_>>(), vec![info.id], "only the public file");
    let usage = alice.call(&GetFileUsage::new()).await.expect("usage");
    assert_eq!((usage.files, usage.bytes), (2, 3_000_004));
    let missing = bob.call(&GetFile::new(shot.id)).await.err();
    assert_eq!(missing.as_ref().and_then(Error::status), Some(404));
    assert_eq!(bob.download_file(shot.id, DownloadOptions::default()).await.err().and_then(|e| e.status()), Some(404));
    let edited = alice.call(&EditFile::new(shot.id, UpdateFile::new().with_name("best-shot.png").with_visibility(FileVisibility::Public))).await.expect("edit");
    assert_eq!((edited.name.as_str(), edited.visibility), ("best-shot.png", FileVisibility::Public));
    assert!(bob.download_file(shot.id, DownloadOptions::default()).await.is_ok());
    let refused = bob.call(&DeleteFile::new(shot.id)).await.err();
    assert!(refused.is_some_and(|e| e.is(codes::FORBIDDEN)));
    alice.call(&DeleteFile::new(shot.id)).await.expect("delete");
    assert_eq!(alice.call(&GetFile::new(shot.id)).await.err().and_then(|e| e.status()), Some(404));

    // The server's refusals: a wrong SHA-256 (422), nothing to read.
    let wrong = alice.upload_file(FileUpload::bytes("x.bin", vec![1, 2, 3]).meta(FileMeta::new().with_sha256(sha(b"other")))).await.err();
    assert!(wrong.is_some_and(|e| e.is(codes::VALIDATION_FAILED)));
    let gone = alice.upload_file(FileUpload::path(dir.join("does-not-exist.bin"))).await.err();
    assert!(matches!(gone, Some(Error::InvalidRequest(_))));
    assert!(matches!(alice.download_file(FileId(999_999), DownloadOptions::default()).await, Err(Error::Api { status: Some(404), .. })));

    // An access token that expired meanwhile is refreshed before the upload.
    server.advance(Duration::from_secs(2 * 3600));
    let later = alice.upload_file(FileUpload::bytes("later.bin", vec![9; 10])).await.expect("after a refresh");
    assert_eq!(later.size, 10);

    // A dropped download stops and leaves no part file.
    let cancelled = dir.join("cancelled.replay");
    let download = bob.start_download_to(info.id, &cancelled, DownloadOptions::default());
    drop(download);
    common::until("the cancelled download's part file to go", || !dir.join("cancelled.replay.part").exists()).await;
    assert!(!cancelled.exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn limits_come_back_as_errors() {
    let server = Server::start_with(|setup| {
        setup.files.max_file_bytes = 1000;
        setup.files.max_bytes_per_user = 1500;
    });
    let client = player(&server, "files-limits").await;
    let big = client.upload_file(FileUpload::bytes("big.bin", vec![0; 1001])).await.err();
    assert!(big.as_ref().is_some_and(|e| e.is(codes::PAYLOAD_TOO_LARGE)), "{big:?}");
    client.upload_file(FileUpload::bytes("a.bin", vec![0; 900])).await.expect("fits");
    let quota = client.upload_file(FileUpload::bytes("b.bin", vec![0; 900])).await.err();
    assert!(quota.is_some_and(|e| e.is(codes::QUOTA_EXCEEDED)));
}

#[test]
fn the_blocking_client_uploads_and_downloads() {
    let server = Server::start();
    let client = net_backend_client::blocking::Client::new(&server.base).expect("client");
    client.register(RegisterRequest::new(email("files-blocking"), PASSWORD)).expect("register");
    let info = client.upload_file(FileUpload::bytes("save.dat", vec![5; 100_000])).expect("upload");
    let mut transfer = client.start_upload(FileUpload::bytes("second.dat", vec![6; 200_000]));
    let second = loop {
        let _ = transfer.try_progress();
        if let Some(result) = transfer.try_finish() {
            break result.expect("second upload");
        }
        std::thread::sleep(Duration::from_millis(5));
    };
    assert_eq!(second.size, 200_000);
    assert_eq!(client.download_file(info.id, DownloadOptions::default()).expect("download"), vec![5; 100_000]);
    let dir = temp("blocking");
    assert_eq!(client.download_file_to(second.id, dir.join("second.dat")).expect("to a file"), 200_000);
    let waited = client.start_download_to(info.id, dir.join("first.dat"), DownloadOptions::default()).wait().expect("wait");
    assert_eq!(waited, 100_000);
}
