//! SSH (and with `sftp`, SFTP) against an in-process mock SSH server on 127.0.0.1 (russh's server
//! side; it never executes anything). Throwaway keys and known_hosts files are generated at runtime
//! under the workspace's `target/tmp`; nothing touches `~/.ssh` or an SSH agent.

#[path = "common/mock_ssh.rs"]
mod mock_ssh;

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use mock_ssh::{random_key, write_key, MockOptions, MockSshServer};
use net_backend_client::ssh::{SshAuth, SshCommand, SshEvent, SshEvents, SshPromptAnswers, SshReconnect, SshSession, SshState, SshStream, SshTarget};
use net_backend_client::{Error, HostKeyProblem};

/// The upper bound of every wait (a condition, never a fixed time).
const WAIT: Duration = Duration::from_secs(60);

/// The next event matching `wanted`, with ONE overall deadline.
async fn event(events: &mut SshEvents, what: &str, wanted: impl Fn(&SshEvent) -> bool) -> SshEvent {
    let found = tokio::time::timeout(WAIT, async {
        while let Some(event) = events.next().await {
            if wanted(&event) {
                return Some(event);
            }
        }
        None
    })
    .await;
    match found {
        Ok(Some(event)) => event,
        Ok(None) => panic!("the events ended before {what}"),
        Err(_) => panic!("timed out waiting for {what}"),
    }
}

/// Wait (async) until `check` is true, with ONE overall deadline.
async fn until(what: &str, mut check: impl FnMut() -> bool) {
    let deadline = tokio::time::Instant::now() + WAIT;
    while !check() {
        assert!(tokio::time::Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// Fast reconnects for the tests (no jitter).
fn fast_reconnect(base: Duration) -> SshReconnect {
    SshReconnect::default().with_base(base).with_cap(base).with_jitter(false)
}

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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_lost_connection_ends_the_session_without_reconnect() {
    let setup = Setup::new();
    let ssh = SshSession::connect(setup.target()).await.expect("connect");
    let mut events = ssh.events();
    assert_eq!(ssh.state(), SshState::Connected);
    setup.mock.drop_connections();
    let closed = event(&mut events, "Closed", |e| matches!(e, SshEvent::Closed { .. })).await;
    assert!(matches!(closed, SshEvent::Closed { error: Some(Error::Disconnected { .. }) }), "{closed:?}");
    assert!(ssh.is_closed() && ssh.state() == SshState::Closed);
    let after = ssh.run("echo late").await.expect_err("closed");
    assert!(matches!(after, Error::Disconnected { sent: Some(false), .. }), "{after:?}");
    assert_eq!(setup.mock.stats().connections.load(Ordering::SeqCst), 1, "never reconnected");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_reconnect_never_reruns_a_command_and_waiting_commands_go_out_on_the_new_connection() {
    let setup = Setup::new();
    let target = setup.target().with_connect_timeout(Duration::from_secs(1)).with_reconnect(fast_reconnect(Duration::from_millis(20)));
    let ssh = SshSession::connect(target).await.expect("connect");
    let mut events = ssh.events();
    // Running when the connection is lost: answered Disconnected (it had started), never re-run.
    let execs = setup.mock.stats().execs.load(Ordering::SeqCst);
    let running = ssh.run_streaming("hang");
    until("the command to reach the server", || setup.mock.stats().execs.load(Ordering::SeqCst) > execs).await;
    setup.mock.set_accepting(false); // reconnect attempts time out until it accepts again
    setup.mock.drop_connections();
    let lost = running.finish().await.expect_err("lost");
    assert!(matches!(lost, Error::Disconnected { sent: Some(true), .. }), "{lost:?}");
    let reconnecting = event(&mut events, "Reconnecting", |e| matches!(e, SshEvent::Reconnecting { .. })).await;
    assert!(matches!(reconnecting, SshEvent::Reconnecting { attempt: 1, error: Error::Disconnected { .. }, .. }), "{reconnecting:?}");
    // Made while reconnecting: waits (a failed attempt later it is still waiting) and runs on the
    // new connection.
    let mut waiting = ssh.run_streaming("echo after the reconnect");
    let second = event(&mut events, "a failed attempt", |e| matches!(e, SshEvent::Reconnecting { attempt: 2, .. })).await;
    assert!(matches!(second, SshEvent::Reconnecting { error: Error::Timeout { .. }, .. }), "{second:?}");
    assert!(waiting.try_finish().is_none(), "it waits for the connection");
    assert!(matches!(ssh.state(), SshState::Reconnecting { .. } | SshState::Connecting), "{:?}", ssh.state());
    assert!(!ssh.is_closed());
    setup.mock.set_accepting(true);
    let chunk = waiting.next_chunk().await.expect("output");
    assert_eq!(chunk.text(), "after the reconnect\n");
    assert!(waiting.finish().await.expect("ran on the new connection").success());
    event(&mut events, "Connected", |e| matches!(e, SshEvent::Connected { reconnected: true })).await;
    assert_eq!(ssh.state(), SshState::Connected);
    assert_eq!(setup.mock.stats().execs.load(Ordering::SeqCst), execs + 2, "the lost command was not run again");
    // The connection can be lost again (the attempt counter of a stable connection resets later).
    setup.mock.drop_connections();
    event(&mut events, "Reconnecting again", |e| matches!(e, SshEvent::Reconnecting { .. })).await;
    assert!(ssh.run("echo again").await.is_ok());
    #[cfg(feature = "sftp")]
    {
        // The SFTP channel is opened again on the new connection.
        ssh.upload("after.txt", b"x".to_vec()).await.expect("upload on the new connection");
        assert_eq!(ssh.download("after.txt").await.expect("download"), b"x");
    }
    ssh.close().await;
    let closed = event(&mut events, "Closed", |e| matches!(e, SshEvent::Closed { .. })).await;
    assert!(matches!(closed, SshEvent::Closed { error: None }), "{closed:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_refused_login_on_reconnect_is_final() {
    let setup = Setup::new();
    let ssh = SshSession::connect(setup.target().with_reconnect(fast_reconnect(Duration::from_millis(20)))).await.expect("connect");
    let mut events = ssh.events();
    setup.mock.set_refuse_logins(true);
    setup.mock.drop_connections();
    let closed = event(&mut events, "Closed", |e| matches!(e, SshEvent::Closed { .. })).await;
    assert!(matches!(closed, SshEvent::Closed { error: Some(Error::AuthFailed(_)) }), "{closed:?}");
    assert!(ssh.is_closed());
    assert_eq!(setup.mock.stats().connections.load(Ordering::SeqCst), 2, "one attempt, then final");
    let after = ssh.run("echo x").await.expect_err("closed");
    assert!(matches!(after, Error::Disconnected { sent: Some(false), .. }), "{after:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reconnects_stop_after_the_attempt_limit_and_close_ends_a_wait() {
    let setup = Setup::new();
    let policy = fast_reconnect(Duration::from_millis(20)).with_max_attempts(Some(2));
    let ssh = SshSession::connect(setup.target().with_reconnect(policy)).await.expect("connect");
    let mut events = ssh.events();
    let Setup { mock, .. } = setup;
    drop(mock); // the server is gone: every attempt is refused
    let mut attempts = Vec::new();
    let closed = loop {
        match event(&mut events, "Closed", |_| true).await {
            SshEvent::Reconnecting { attempt, .. } => attempts.push(attempt),
            SshEvent::Closed { error } => break error,
            _ => {}
        }
    };
    assert_eq!(attempts, [1, 2]);
    assert!(matches!(closed, Some(Error::Network { .. } | Error::Timeout { .. })), "{closed:?}");
    // A session that waits for its next attempt closes at once when the app closes it.
    let setup = Setup::new();
    let ssh = SshSession::connect(setup.target().with_reconnect(fast_reconnect(Duration::from_secs(3600)))).await.expect("connect");
    let mut events = ssh.events();
    setup.mock.drop_connections();
    event(&mut events, "Reconnecting", |e| matches!(e, SshEvent::Reconnecting { .. })).await;
    let waiting = ssh.run_streaming("echo never");
    ssh.close().await;
    let closed = event(&mut events, "Closed", |e| matches!(e, SshEvent::Closed { .. })).await;
    assert!(matches!(closed, SshEvent::Closed { error: None }), "{closed:?}");
    let never = waiting.finish().await.expect_err("closed while waiting");
    assert!(matches!(never, Error::Disconnected { sent: Some(false), .. }), "{never:?}");
}

#[test]
fn the_blocking_session_reports_its_state() {
    let setup = Setup::new();
    let ssh = net_backend_client::blocking::SshSession::connect(setup.target().with_reconnect(fast_reconnect(Duration::from_millis(20)))).expect("connect");
    let mut events = ssh.events();
    assert_eq!(ssh.state(), SshState::Connected);
    setup.mock.drop_connections();
    let deadline = std::time::Instant::now() + WAIT;
    loop {
        if let Some(SshEvent::Connected { reconnected: true }) = events.try_next() {
            break;
        }
        assert!(std::time::Instant::now() < deadline, "no reconnect");
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(ssh.run("echo back").expect("run").stdout_text(), "back\n");
    ssh.close();
}

/// SFTP transfers: pipelined downloads (integrity, sizes, short reads), progress, the remote
/// handle closed after a cancel / timeout / error, the part file removed.
#[cfg(feature = "sftp")]
mod transfers {
    use std::io::Read;

    use net_backend_client::ssh::{SftpProgress, SftpTask};

    use super::*;

    const MIB: usize = 1024 * 1024;

    /// Deterministic content: 1 MiB blocks of xorshift output, seeded by the block index.
    fn block(index: u64) -> Vec<u8> {
        let mut state = index.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
        let mut out = Vec::with_capacity(MIB);
        while out.len() < MIB {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            out.extend_from_slice(&state.to_le_bytes());
        }
        out
    }

    fn content(len: usize) -> Vec<u8> {
        let mut out = Vec::with_capacity(len);
        let mut index = 0;
        while out.len() < len {
            let take = (len - out.len()).min(MIB);
            out.extend_from_slice(&block(index)[..take]);
            index += 1;
        }
        out
    }

    /// Whether `data` (in memory or read from `path`) is `content(len)`, compared block by block.
    fn same_as_content(mut data: impl Read, len: usize) -> bool {
        let mut buffer = vec![0u8; MIB];
        let mut done = 0;
        let mut index = 0;
        while done < len {
            let want = (len - done).min(MIB);
            if data.read_exact(&mut buffer[..want]).is_err() || buffer[..want] != block(index)[..want] {
                return false;
            }
            done += want;
            index += 1;
        }
        let mut rest = [0u8; 1];
        matches!(data.read(&mut rest), Ok(0))
    }

    /// Drive a task to its end, collecting every progress report.
    async fn finish_with_progress<T>(mut task: SftpTask<T>) -> (Result<T, Error>, Vec<SftpProgress>) {
        let mut reports = Vec::new();
        while let Some(report) = tokio::time::timeout(WAIT, task.next_progress()).await.expect("progress") {
            reports.push(report);
        }
        (tokio::time::timeout(WAIT, task.finish()).await.expect("result"), reports)
    }

    fn assert_progress(reports: &[SftpProgress], size: u64) {
        assert!(!reports.is_empty(), "at least the last report");
        assert!(reports.windows(2).all(|w| w[0].done <= w[1].done), "only grows: {reports:?}");
        let last = reports.last().copied().unwrap_or_default();
        assert_eq!((last.done, last.total), (size, Some(size)), "the last report is the whole size");
    }

    fn part_files(dir: &std::path::Path) -> usize {
        std::fs::read_dir(dir).map(|d| d.filter_map(Result::ok).filter(|e| e.file_name().to_string_lossy().ends_with(".part")).count()).unwrap_or(0)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_256_mib_download_arrives_whole_in_memory_and_to_a_file() {
        const SIZE: usize = 256 * MIB;
        let setup = Setup::new();
        setup.mock.put_file_owned("big.bin", content(SIZE));
        let ssh = SshSession::connect(setup.target().with_max_transfer_bytes(512 * MIB as u64)).await.expect("connect");
        let local = setup.dir.join("big.bin");
        let (result, reports) = finish_with_progress(ssh.start_download_file("big.bin", &local)).await;
        assert_eq!(result.expect("download_file"), SIZE as u64);
        assert_progress(&reports, SIZE as u64);
        assert!(same_as_content(std::fs::File::open(&local).expect("local file"), SIZE), "the file is byte-for-byte the remote one");
        assert_eq!(part_files(&setup.dir), 0);
        std::fs::remove_file(&local).expect("remove");
        let (result, reports) = finish_with_progress(ssh.start_download("big.bin")).await;
        let data = result.expect("download");
        assert_progress(&reports, SIZE as u64);
        assert!(same_as_content(data.as_slice(), SIZE), "the data is byte-for-byte the remote file");
        until("every handle closed", || setup.mock.stats().sftp_handles.load(Ordering::SeqCst) == 0).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn every_size_from_empty_up_arrives_whole_also_with_short_reads() {
        for short_reads in [false, true] {
            let setup = Setup::with(None, MockOptions { sftp_short_reads: short_reads, ..MockOptions::default() });
            let ssh = SshSession::connect(setup.target()).await.expect("connect");
            for size in [0, 1, 65_535, 65_536, 65_537, 16 * 65_536 + 7, 3_000_017, if short_reads { 5 * MIB } else { 1 }] {
                setup.mock.put_file_owned("file.bin", content(size));
                let data = ssh.download("file.bin").await.unwrap_or_else(|e| panic!("{size} bytes, short reads {short_reads}: {e}"));
                assert!(same_as_content(data.as_slice(), size), "{size} bytes into memory, short reads {short_reads}");
                let local = setup.dir.join(format!("file-{size}.bin"));
                let (result, reports) = finish_with_progress(ssh.start_download_file("file.bin", &local)).await;
                assert_eq!(result.expect("download_file"), size as u64);
                if size > 0 {
                    assert_progress(&reports, size as u64);
                }
                assert!(same_as_content(std::fs::File::open(&local).expect("local"), size), "{size} bytes to a file, short reads {short_reads}");
            }
            assert_eq!(part_files(&setup.dir), 0);
            until("every handle closed", || setup.mock.stats().sftp_handles.load(Ordering::SeqCst) == 0).await;
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_upload_reports_progress_up_to_its_size() {
        let setup = Setup::new();
        let ssh = SshSession::connect(setup.target()).await.expect("connect");
        let data = content(3 * MIB + 5);
        let (result, reports) = finish_with_progress(ssh.start_upload("up.bin", data.clone())).await;
        assert_eq!(result.expect("upload"), data.len() as u64);
        assert_progress(&reports, data.len() as u64);
        assert!(setup.mock.file("up.bin").as_deref() == Some(data.as_slice()));
        let local = setup.dir.join("up-local.bin");
        std::fs::write(&local, &data).expect("local");
        let (result, reports) = finish_with_progress(ssh.start_upload_file(&local, "up2.bin")).await;
        assert_eq!(result.expect("upload_file"), data.len() as u64);
        assert_progress(&reports, data.len() as u64);
        until("every handle closed", || setup.mock.stats().sftp_handles.load(Ordering::SeqCst) == 0).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_cancel_or_timeout_midway_closes_the_remote_handle_and_leaves_no_file() {
        // 8 MiB at 15 ms per read on a server that answers one read at a time: about 2 s.
        let setup = Setup::with(None, MockOptions { sftp_read_delay: Some(Duration::from_millis(15)), ..MockOptions::default() });
        setup.mock.put_file_owned("slow.bin", content(8 * MIB));
        setup.mock.put_file_owned("small.bin", content(1000));
        let ssh = SshSession::connect(setup.target()).await.expect("connect");
        // Cancel (drop the task) after the first progress report.
        let local = setup.dir.join("cancelled.bin");
        let mut task = ssh.start_download_file("slow.bin", &local);
        let first = tokio::time::timeout(WAIT, task.next_progress()).await.expect("progress").expect("a report");
        assert!(first.done > 0 && first.done < 8 * MIB as u64, "{first:?}");
        drop(task);
        until("the handle of the cancelled download closed", || setup.mock.stats().sftp_handles.load(Ordering::SeqCst) == 0).await;
        until("the part file removed", || part_files(&setup.dir) == 0).await;
        assert!(!local.exists());
        // The same session downloads again afterwards.
        assert!(same_as_content(ssh.download("small.bin").await.expect("small").as_slice(), 1000));
        // A timeout midway: the operation's deadline (and each request's) is the SFTP timeout.
        let short = SshSession::connect(setup.target().with_sftp_timeout(Duration::from_millis(400))).await.expect("connect");
        let local = setup.dir.join("timed-out.bin");
        let error = short.download_file("slow.bin", &local).await.expect_err("timeout");
        assert!(matches!(error, Error::Timeout { sent: None, .. }), "{error:?}");
        until("the handle of the timed-out download closed", || setup.mock.stats().sftp_handles.load(Ordering::SeqCst) == 0).await;
        until("the part file removed", || part_files(&setup.dir) == 0).await;
        assert!(!local.exists());
        // The timed-out session's SFTP channel still works (a listing has no read delay).
        assert!(short.list_dir(".").await.expect("list after a timeout").iter().any(|e| e.name == "small.bin"));
        assert!(same_as_content(ssh.download("small.bin").await.expect("small").as_slice(), 1000));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_connection_lost_midway_ends_a_transfer_disconnected_and_leaves_no_file() {
        // 8 MiB at 15 ms per read on a server that answers one read at a time: about 2 s.
        let setup = Setup::with(None, MockOptions { sftp_read_delay: Some(Duration::from_millis(15)), ..MockOptions::default() });
        setup.mock.put_file_owned("slow.bin", content(8 * MIB));
        let ssh = SshSession::connect(setup.target()).await.expect("connect");
        let local = setup.dir.join("lost.bin");
        let mut task = ssh.start_download_file("slow.bin", &local);
        let first = tokio::time::timeout(WAIT, task.next_progress()).await.expect("progress").expect("a report");
        assert!(first.done > 0 && first.done < 8 * MIB as u64, "{first:?}");
        setup.mock.drop_connections();
        let error = tokio::time::timeout(WAIT, task.finish()).await.expect("result").expect_err("the connection was lost");
        // The server answered part of it: sent, like a command that had started; never `Ssh`.
        assert!(matches!(error, Error::Disconnected { sent: Some(true), .. }), "{error:?}");
        until("the part file removed", || part_files(&setup.dir) == 0).await;
        assert!(!local.exists());
        // Later operations on the closed session (no reconnect): never sent.
        until("the session closed", || ssh.state() == SshState::Closed).await;
        let after = ssh.list_dir(".").await.expect_err("closed");
        assert!(matches!(after, Error::Disconnected { sent: Some(false), .. }), "{after:?}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_file_cut_short_during_the_download_is_an_error_and_leaves_no_file() {
        // 8 MiB at 15 ms per read on a server that answers one read at a time: about 2 s.
        let setup = Setup::with(None, MockOptions { sftp_read_delay: Some(Duration::from_millis(15)), ..MockOptions::default() });
        setup.mock.put_file_owned("cut.bin", content(8 * MIB));
        let ssh = SshSession::connect(setup.target()).await.expect("connect");
        let local = setup.dir.join("cut.bin");
        let mut task = ssh.start_download_file("cut.bin", &local);
        let first = tokio::time::timeout(WAIT, task.next_progress()).await.expect("progress").expect("a report");
        assert!(first.done > 0 && first.done < 8 * MIB as u64, "{first:?}");
        // The server's file is truncated midway (as `truncate -s` would).
        setup.mock.put_file_owned("cut.bin", content(4 * MIB));
        let error = tokio::time::timeout(WAIT, task.finish()).await.expect("result").expect_err("cut short");
        assert!(
            matches!(&error, Error::Ssh(why) if why.contains("changed size during the download") && why.contains(&format!("expected {} bytes", 8 * MIB)) && why.contains("received ")),
            "{error:?}"
        );
        assert!(!local.exists(), "no final file");
        until("the part file removed", || part_files(&setup.dir) == 0).await;
        until("the handle closed", || setup.mock.stats().sftp_handles.load(Ordering::SeqCst) == 0).await;
        // Into memory too.
        setup.mock.put_file_owned("cut.bin", content(8 * MIB));
        let mut task = ssh.start_download("cut.bin");
        tokio::time::timeout(WAIT, task.next_progress()).await.expect("progress").expect("a report");
        setup.mock.put_file_owned("cut.bin", content(MIB));
        let error = tokio::time::timeout(WAIT, task.finish()).await.expect("result").expect_err("cut short");
        assert!(matches!(&error, Error::Ssh(why) if why.contains("changed size during the download")), "{error:?}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_file_shorter_than_its_reported_size_is_an_error_and_size_0_or_none_reads_to_the_end() {
        const SIZE: usize = 3 * MIB + 5;
        // The server reports a larger size than the file has: the end comes too early.
        let setup = Setup::with(None, MockOptions { sftp_fstat_size: Some(Some(5 * MIB as u64)), ..MockOptions::default() });
        setup.mock.put_file_owned("short.bin", content(SIZE));
        let ssh = SshSession::connect(setup.target()).await.expect("connect");
        let local = setup.dir.join("short.bin");
        let error = ssh.download_file("short.bin", &local).await.expect_err("shorter than reported");
        let counts = format!("expected {} bytes, its size when it was opened; received {SIZE}", 5 * MIB);
        assert!(matches!(&error, Error::Ssh(why) if why.contains("changed size during the download") && why.contains(&counts)), "{error:?}");
        assert!(!local.exists(), "no final file");
        until("the part file removed", || part_files(&setup.dir) == 0).await;
        let error = ssh.download("short.bin").await.expect_err("shorter than reported");
        assert!(matches!(&error, Error::Ssh(why) if why.contains(&counts)), "{error:?}");
        until("every handle closed", || setup.mock.stats().sftp_handles.load(Ordering::SeqCst) == 0).await;
        // A reported size of 0, or none (as some special files do): read to the end.
        for reported in [Some(0), None] {
            let setup = Setup::with(None, MockOptions { sftp_fstat_size: Some(reported), ..MockOptions::default() });
            setup.mock.put_file_owned("special.bin", content(SIZE));
            let ssh = SshSession::connect(setup.target()).await.expect("connect");
            let data = ssh.download("special.bin").await.unwrap_or_else(|e| panic!("reported {reported:?}: {e}"));
            assert!(same_as_content(data.as_slice(), SIZE), "reported {reported:?}: whole file in memory");
            let local = setup.dir.join("special.bin");
            assert_eq!(ssh.download_file("special.bin", &local).await.unwrap_or_else(|e| panic!("reported {reported:?}: {e}")), SIZE as u64);
            assert!(same_as_content(std::fs::File::open(&local).expect("local"), SIZE), "reported {reported:?}: whole file on disk");
            assert_eq!(part_files(&setup.dir), 0);
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_server_error_midway_stops_the_download_and_closes_the_handle() {
        let setup = Setup::with(None, MockOptions { sftp_fail_reads_at: Some(3 * MIB as u64), ..MockOptions::default() });
        setup.mock.put_file_owned("broken.bin", content(8 * MIB));
        let ssh = SshSession::connect(setup.target()).await.expect("connect");
        let local = setup.dir.join("broken.bin");
        let error = ssh.download_file("broken.bin", &local).await.expect_err("server error");
        assert!(matches!(&error, Error::Ssh(why) if why.starts_with("SFTP:")), "{error:?}");
        assert!(!local.exists());
        until("the part file removed", || part_files(&setup.dir) == 0).await;
        until("the handle closed", || setup.mock.stats().sftp_handles.load(Ordering::SeqCst) == 0).await;
        let error = ssh.download("broken.bin").await.expect_err("server error");
        assert!(matches!(&error, Error::Ssh(_)), "{error:?}");
        // The session's SFTP channel still works.
        assert!(ssh.list_dir(".").await.expect("list").iter().any(|e| e.name == "broken.bin"));
        // A file the server reports over the limit is refused before any read.
        let limited = SshSession::connect(setup.target().with_max_transfer_bytes(MIB as u64)).await.expect("connect");
        assert!(matches!(limited.download("broken.bin").await, Err(Error::BodyTooLarge { .. })));
    }

    #[test]
    fn the_blocking_session_polls_a_transfer() {
        let setup = Setup::new();
        setup.mock.put_file_owned("polled.bin", content(2 * MIB));
        let ssh = net_backend_client::blocking::SshSession::connect(setup.target()).expect("connect");
        let mut task = ssh.start_download("polled.bin");
        let deadline = std::time::Instant::now() + WAIT;
        let mut last = SftpProgress::default();
        let data = loop {
            if let Some(report) = task.try_progress() {
                assert!(report.done >= last.done);
                last = report;
            }
            if let Some(result) = task.try_finish() {
                break result.expect("download");
            }
            assert!(std::time::Instant::now() < deadline, "no result");
            std::thread::sleep(Duration::from_millis(5));
        };
        assert!(same_as_content(data.as_slice(), 2 * MIB));
        assert_eq!(task.try_progress().map_or(last.done, |p| p.done), 2 * MIB as u64, "the last report survives the end");
        ssh.close();
    }
}
