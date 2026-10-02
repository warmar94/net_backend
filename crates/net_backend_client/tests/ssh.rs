//! SSH (and with `sftp`, SFTP) against an in-process mock SSH server on 127.0.0.1 (russh's server
//! side; it never executes anything). Throwaway keys and known_hosts files are generated at runtime
//! under the workspace's `target/tmp`; nothing touches `~/.ssh` or an SSH agent.

#[path = "common/mock_ssh.rs"]
mod mock_ssh;

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use mock_ssh::{random_key, write_key, MockOptions, MockSshServer};
use net_backend_client::ssh::{SshAuth, SshCommand, SshPromptAnswers, SshSession, SshStream, SshTarget};
use net_backend_client::{Error, HostKeyProblem};

const USER: &str = "tester";

/// A fresh directory under the workspace's `target/tmp` for one test's throwaway files.
fn scratch() -> PathBuf {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..").join("..").join("target").join("tmp").join(format!(
        "client-ssh-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::SeqCst)
    ));
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

/// A mock server, a client key file (encrypted with `passphrase` if given) and a known_hosts file
/// that lists the mock.
struct Setup {
    mock: MockSshServer,
    dir: PathBuf,
    key: PathBuf,
    known_hosts: PathBuf,
}

impl Setup {
    fn new() -> Self {
        Self::with(None, MockOptions::default())
    }

    fn with(passphrase: Option<&str>, options: MockOptions) -> Self {
        let dir = scratch();
        let client = random_key();
        let key = dir.join("id_test");
        write_key(&client, &key, passphrase).expect("key");
        let mock = MockSshServer::start_with("127.0.0.1:0", USER, client.public_key().clone(), options).expect("mock");
        let known_hosts = dir.join("known_hosts");
        std::fs::write(&known_hosts, format!("{}\n", mock.known_hosts_line("127.0.0.1"))).expect("known_hosts");
        Self { mock, dir, key, known_hosts }
    }

    fn target(&self) -> SshTarget {
        SshTarget::new("127.0.0.1", USER).with_port(self.mock.port()).with_auth(SshAuth::key_file(&self.key)).with_known_hosts_file(&self.known_hosts)
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn commands_output_exit_codes_and_stdin() {
    let setup = Setup::new();
    let ssh = SshSession::connect(setup.target()).await.expect("connect");
    assert_eq!(ssh.fingerprint(), setup.mock.fingerprint());
    let out = ssh.run("echo hello there").await.expect("echo");
    assert_eq!((out.stdout_text().as_str(), out.exit.status, out.success()), ("hello there\n", Some(0), true));
    let out = ssh.run("fail").await.expect("ran");
    assert_eq!((out.exit.status, out.stderr_text().as_str()), (Some(3), "mock: failed as asked\n"));
    let out = ssh.run(SshCommand::new("cat").with_stdin(b"from stdin".to_vec())).await.expect("cat");
    assert_eq!(out.stdout, b"from stdin");
    let out = ssh.run("signal").await.expect("signal");
    assert_eq!((out.exit.status, out.exit.signal.as_deref()), (None, Some("KILL")));
    // Streaming: chunks as they arrive, then the exit.
    let mut run = ssh.run_streaming("stderr oops");
    let chunk = run.next_chunk().await.expect("a chunk");
    assert_eq!((chunk.stream, chunk.text().as_str()), (SshStream::Stderr, "oops\n"));
    assert!(run.finish().await.expect("exit").success());
    // Several commands at once on their own channels.
    let (a, b) = tokio::join!(ssh.run("echo a"), ssh.run("echo b"));
    assert_eq!((a.expect("a").stdout_text(), b.expect("b").stdout_text()), ("a\n".to_string(), "b\n".to_string()));
    let refused = ssh.run("refuse").await.expect_err("refused");
    assert!(matches!(refused, Error::Ssh(_)), "{refused:?}");
    ssh.close().await;
    assert!(ssh.is_closed());
    let after = ssh.run("echo late").await.expect_err("closed");
    assert!(matches!(after, Error::Disconnected { .. }), "{after:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn limits_and_timeouts() {
    let setup = Setup::new();
    let ssh = SshSession::connect(setup.target().with_max_output_bytes(64 * 1024)).await.expect("connect");
    let flood = ssh.run("flood 1000000").await.expect_err("too much output");
    assert!(matches!(flood, Error::BodyTooLarge { limit: 65_536, .. }), "{flood:?}");
    let hang = ssh.run(SshCommand::new("hang").with_timeout(Duration::from_millis(200))).await.expect_err("timeout");
    assert!(matches!(hang, Error::Timeout { sent: Some(true), .. }), "{hang:?}");
    let long = ssh.run("x".repeat(70 * 1024)).await.expect_err("too long");
    assert!(matches!(long, Error::RequestTooLarge { .. }) && long.was_sent() == Some(false));
    // Dropping a running command stops it (TERM, then the channel is closed).
    let signals = setup.mock.stats().signals.load(Ordering::SeqCst);
    let execs = setup.mock.stats().execs.load(Ordering::SeqCst);
    let run = ssh.run_streaming("sleep 30000");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    while setup.mock.stats().execs.load(Ordering::SeqCst) == execs {
        assert!(tokio::time::Instant::now() < deadline, "the command never reached the server");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    drop(run);
    while setup.mock.stats().signals.load(Ordering::SeqCst) == signals {
        assert!(tokio::time::Instant::now() < deadline, "no TERM signal");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(ssh.run("echo still").await.is_ok());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn host_keys_are_strict() {
    let setup = Setup::new();
    // Unknown: an empty known_hosts.
    let empty = setup.dir.join("empty_known_hosts");
    std::fs::write(&empty, "").expect("write");
    let target = SshTarget::new("127.0.0.1", USER).with_port(setup.mock.port()).with_auth(SshAuth::key_file(&setup.key)).with_known_hosts_file(&empty);
    let error = SshSession::connect(target.clone()).await.expect_err("unknown");
    assert!(matches!(&error, Error::HostKey { problem: HostKeyProblem::Unknown, fingerprint, .. } if *fingerprint == setup.mock.fingerprint()), "{error:?}");
    assert_eq!(error.was_sent(), Some(false));
    // Changed: the host is listed with another key of the same type.
    let other = random_key();
    let changed = setup.dir.join("changed_known_hosts");
    std::fs::write(&changed, format!("{}\n", setup.mock.known_hosts_line_for("127.0.0.1", other.public_key()))).expect("write");
    let target_changed =
        SshTarget::new("127.0.0.1", USER).with_port(setup.mock.port()).with_auth(SshAuth::key_file(&setup.key)).with_known_hosts_file(&changed);
    let error = SshSession::connect(target_changed).await.expect_err("changed");
    assert!(matches!(error, Error::HostKey { problem: HostKeyProblem::Changed, .. }), "{error:?}");
    // A pinned fingerprint (checked on the server) is trusted without known_hosts.
    let pinned = SshTarget::new("127.0.0.1", USER)
        .with_port(setup.mock.port())
        .with_auth(SshAuth::key_file(&setup.key))
        .trust_host_key_fingerprint(setup.mock.fingerprint());
    assert!(SshSession::connect(pinned).await.is_ok());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn passwords_prompts_and_encrypted_keys() {
    let options =
        MockOptions { password: Some("fake-pw-123".into()), keyboard_interactive: Some(("fake-pw-123".into(), "424242".into())), ..MockOptions::default() };
    let setup = Setup::with(Some("fake-passphrase"), options);
    let base = || SshTarget::new("127.0.0.1", USER).with_port(setup.mock.port()).with_known_hosts_file(&setup.known_hosts);
    let ssh = SshSession::connect(base().with_auth(SshAuth::key_file_with_passphrase(&setup.key, "fake-passphrase"))).await.expect("encrypted key");
    assert_eq!(ssh.run("whoami").await.expect("whoami").stdout_text(), "tester\n");
    let error = SshSession::connect(base().with_auth(SshAuth::key_file(&setup.key))).await.expect_err("no passphrase");
    assert!(matches!(&error, Error::AuthFailed(why) if why.contains("id_test") && !why.contains(setup.dir.to_string_lossy().as_ref())), "{error:?}");
    assert!(SshSession::connect(base().with_auth(SshAuth::password("fake-pw-123"))).await.is_ok());
    let wrong = SshSession::connect(base().with_auth(SshAuth::password("wrong"))).await.expect_err("wrong password");
    assert!(matches!(wrong, Error::AuthFailed(_)) && !format!("{wrong:?}").contains("wrong"), "{wrong:?}");
    let answers = SshPromptAnswers::new().answer_containing("password", "fake-pw-123").answer_containing("code", "424242");
    assert!(SshSession::connect(base().with_auth(SshAuth::keyboard_interactive(answers))).await.is_ok());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn terrapin_exposed_servers_are_refused_unless_allowed() {
    let options = MockOptions { no_strict_kex: true, ciphers: Some(vec!["chacha20-poly1305@openssh.com"]), ..MockOptions::default() };
    let setup = Setup::with(None, options);
    let error = SshSession::connect(setup.target()).await.expect_err("refused");
    assert!(matches!(&error, Error::Ssh(why) if why.contains("Terrapin")), "{error:?}");
    let ssh = SshSession::connect(setup.target().allow_terrapin_vulnerable(true)).await.expect("allowed by the opt-out");
    assert!(ssh.run("echo ok").await.is_ok());
}

#[test]
fn the_blocking_session() {
    let setup = Setup::new();
    let ssh = net_backend_client::blocking::SshSession::connect(setup.target()).expect("connect");
    assert_eq!(ssh.run("echo blocking").expect("run").stdout_text(), "blocking\n");
    let mut run = ssh.run_streaming("echo polled");
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    let exit = loop {
        let _ = run.try_next_chunk();
        if let Some(exit) = run.try_finish() {
            break exit;
        }
        assert!(std::time::Instant::now() < deadline, "no exit");
        std::thread::sleep(Duration::from_millis(5));
    };
    assert!(exit.expect("exit").success());
    ssh.close();
}

#[cfg(feature = "sftp")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sftp_file_operations() {
    let setup = Setup::new();
    let ssh = SshSession::connect(setup.target().with_max_transfer_bytes(1024 * 1024)).await.expect("connect");
    let data: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
    assert_eq!(ssh.upload("upload.bin", data.clone()).await.expect("upload"), data.len() as u64);
    assert_eq!(setup.mock.file("upload.bin").as_deref(), Some(data.as_slice()));
    assert_eq!(ssh.download("upload.bin").await.expect("download"), data);
    let local = setup.dir.join("downloaded.bin");
    assert_eq!(ssh.download_file("upload.bin", &local).await.expect("download_file"), data.len() as u64);
    assert_eq!(std::fs::read(&local).expect("local"), data);
    ssh.create_dir("logs").await.expect("mkdir");
    assert_eq!(ssh.upload_file(&local, "logs/copy.bin").await.expect("upload_file"), data.len() as u64);
    ssh.rename("logs/copy.bin", "logs/moved.bin").await.expect("rename");
    let listing = ssh.list_dir("logs").await.expect("list");
    assert_eq!(listing.iter().map(|e| e.name.as_str()).collect::<Vec<_>>(), ["moved.bin"]);
    assert_eq!(listing[0].size, Some(data.len() as u64));
    ssh.remove_file("logs/moved.bin").await.expect("remove");
    ssh.remove_dir("logs").await.expect("rmdir");
    let missing = ssh.download("logs/moved.bin").await.expect_err("gone");
    assert!(matches!(missing, Error::Ssh(_)), "{missing:?}");
    let too_big = ssh.upload("big.bin", vec![0u8; 2 * 1024 * 1024]).await.expect_err("over the limit");
    assert!(matches!(too_big, Error::RequestTooLarge { .. }) && too_big.was_sent() == Some(false));
    setup.mock.put_file("huge.bin", &vec![1u8; 2 * 1024 * 1024]);
    let huge = ssh.download("huge.bin").await.expect_err("over the limit");
    assert!(matches!(huge, Error::BodyTooLarge { .. }), "{huge:?}");
    // A failed download leaves no part file behind.
    assert!(ssh.download_file("nope.bin", setup.dir.join("nope.bin")).await.is_err());
    let leftovers: Vec<_> =
        std::fs::read_dir(&setup.dir).expect("dir").filter_map(Result::ok).filter(|e| e.file_name().to_string_lossy().ends_with(".part")).collect();
    assert!(leftovers.is_empty() || wait_gone(&setup.dir));
}

/// Part files are removed on the blocking pool: wait for that (one deadline).
#[cfg(feature = "sftp")]
fn wait_gone(dir: &std::path::Path) -> bool {
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    loop {
        let left = std::fs::read_dir(dir).map(|d| d.filter_map(Result::ok).any(|e| e.file_name().to_string_lossy().ends_with(".part"))).unwrap_or(false);
        if !left {
            return true;
        }
        if std::time::Instant::now() > deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn the_blocking_session_from_spawn_blocking() {
    let setup = Setup::new();
    let target = setup.target();
    let runtime = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().expect("runtime");
    let out = runtime.block_on(async move {
        tokio::task::spawn_blocking(move || {
            let ssh = net_backend_client::blocking::SshSession::connect(target)?;
            let out = ssh.run("echo from spawn_blocking")?;
            ssh.close();
            Ok::<_, Error>(out)
        })
        .await
        .expect("join")
    });
    assert_eq!(out.expect("connect + run").stdout_text(), "from spawn_blocking\n");
}
