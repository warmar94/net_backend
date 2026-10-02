//! SSH + SFTP to the server machine's OpenSSH (feature `sftp`): run a command, list a directory.
//! Admin tools only. Keys come from the SSH agent; host keys from the given known_hosts file
//! (nothing is written to it). Headless and bounded (the target's timeouts).
//!
//! ```text
//! SSH_HOST=server.example.com SSH_USER=deploy SSH_KNOWN_HOSTS=/home/admin/.ssh/known_hosts \
//!     cargo run -p net_backend_client --example admin_ssh --features sftp
//! ```

use net_backend_client::ssh::{SshAuth, SshSession, SshTarget};
use net_backend_client::Error;

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Error> {
    let (Ok(host), Ok(user), Ok(known_hosts)) = (std::env::var("SSH_HOST"), std::env::var("SSH_USER"), std::env::var("SSH_KNOWN_HOSTS")) else {
        eprintln!("set SSH_HOST, SSH_USER and SSH_KNOWN_HOSTS (keys come from the SSH agent)");
        return Ok(());
    };
    let target = SshTarget::new(host, user).with_auth(SshAuth::agent()).with_known_hosts_file(known_hosts);
    let ssh = SshSession::connect(target).await?;
    println!("connected; host key {}", ssh.fingerprint());
    let output = ssh.run("uptime").await?;
    print!("{}", output.stdout_text());
    for entry in ssh.list_dir(".").await? {
        println!("{:?} {} ({:?} bytes)", entry.kind, entry.safe_file_name().unwrap_or("<unsafe name>"), entry.size);
    }
    ssh.close().await;
    Ok(())
}
