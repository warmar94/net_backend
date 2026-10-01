//! Mail: the [`Mailer`] trait, a [`LogMailer`] (development; the default), a [`MemoryMailer`]
//! (tests), and the SMTP mailer `SmtpMailer` (feature `smtp`: lettre on tokio with rustls + ring,
//! no OpenSSL / aws-lc).
//!
//! Requests never wait for a mail: the auth module puts each mail into a bounded queue that a few
//! background tasks send (`mail_queue`, `mail_concurrency`); a full queue drops the mail and logs it.
//! [`Mail`]'s `Debug` and every log line show the recipient masked (`a***@example.com`) and the
//! subject, never the body (it carries one-time links).

use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::future::BoxFuture;
use tokio::sync::Semaphore;

/// One plain-text mail.
#[derive(Clone)]
#[non_exhaustive]
pub struct Mail {
    /// The recipient address.
    pub to: String,
    /// The subject.
    pub subject: String,
    /// The plain-text body.
    pub text: String,
}

impl Mail {
    /// A mail.
    pub fn new(to: impl Into<String>, subject: impl Into<String>, text: impl Into<String>) -> Self {
        Self { to: to.into(), subject: subject.into(), text: text.into() }
    }
}

impl std::fmt::Debug for Mail {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Mail").field("to", &mask_address(&self.to)).field("subject", &self.subject).field("text", &"<hidden>").finish()
    }
}

/// `ada@example.com` → `a***@example.com` (for logs).
pub fn mask_address(address: &str) -> String {
    match address.split_once('@') {
        Some((local, domain)) => format!("{}***@{domain}", local.chars().next().map(String::from).unwrap_or_default()),
        None => "***".into(),
    }
}

/// A mail could not be sent. The text never contains the mail's body.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct MailError(pub String);

/// Sends mail. Implement it for an HTTP mail API (Postmark, SES, …) if SMTP does not fit.
///
/// Methods added later always come with a default implementation.
pub trait Mailer: Send + Sync + 'static {
    /// Send one mail.
    fn send<'a>(&'a self, mail: &'a Mail) -> BoxFuture<'a, Result<(), MailError>>;
}

/// Writes mails to the log (`INFO`): recipient (masked) and subject; the body only with
/// `show_body` (development: the one-time links are secrets).
#[derive(Clone, Copy, Debug, Default)]
pub struct LogMailer {
    show_body: bool,
}

impl LogMailer {
    /// A log mailer; `show_body` also logs the body.
    pub fn new(show_body: bool) -> Self {
        Self { show_body }
    }
}

impl Mailer for LogMailer {
    fn send<'a>(&'a self, mail: &'a Mail) -> BoxFuture<'a, Result<(), MailError>> {
        Box::pin(async move {
            if self.show_body {
                tracing::info!(to = %mask_address(&mail.to), subject = %mail.subject, body = %mail.text, ">>> NBS: mail (log mailer)");
            } else {
                tracing::info!(to = %mask_address(&mail.to), subject = %mail.subject, ">>> NBS: mail (log mailer; body hidden, set log_mailer_show_links to see it)");
            }
            Ok(())
        })
    }
}

/// Keeps every mail in memory (tests: read the links from [`sent`](MemoryMailer::sent)).
#[derive(Debug, Default)]
pub struct MemoryMailer {
    sent: Mutex<Vec<Mail>>,
    fail: Mutex<bool>,
}

impl MemoryMailer {
    /// An empty mailbox.
    pub fn new() -> Self {
        Self::default()
    }

    /// Every mail sent so far.
    pub fn sent(&self) -> Vec<Mail> {
        self.sent.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// The mails sent to `address`.
    pub fn sent_to(&self, address: &str) -> Vec<Mail> {
        self.sent().into_iter().filter(|m| m.to == address).collect()
    }

    /// Fail every send from now on (or not).
    pub fn set_failing(&self, fail: bool) {
        *self.fail.lock().unwrap_or_else(|e| e.into_inner()) = fail;
    }
}

impl Mailer for MemoryMailer {
    fn send<'a>(&'a self, mail: &'a Mail) -> BoxFuture<'a, Result<(), MailError>> {
        Box::pin(async move {
            if *self.fail.lock().unwrap_or_else(|e| e.into_inner()) {
                return Err(MailError("the memory mailer is set to fail".into()));
            }
            self.sent.lock().unwrap_or_else(|e| e.into_inner()).push(mail.clone());
            Ok(())
        })
    }
}

impl<M: Mailer> Mailer for Arc<M> {
    fn send<'a>(&'a self, mail: &'a Mail) -> BoxFuture<'a, Result<(), MailError>> {
        (**self).send(mail)
    }
}

/// The bounded send queue: at most `capacity` mails waiting or in flight, `concurrency` sent at
/// once, each bounded by `timeout`.
pub(crate) struct MailQueue {
    mailer: Arc<dyn Mailer>,
    slots: Arc<Semaphore>,
    capacity: u32,
    sending: Arc<Semaphore>,
    timeout: Duration,
}

impl std::fmt::Debug for MailQueue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MailQueue").field("capacity", &self.capacity).field("free", &self.slots.available_permits()).finish()
    }
}

impl MailQueue {
    pub(crate) fn new(mailer: Arc<dyn Mailer>, capacity: usize, concurrency: usize, timeout: Duration) -> Self {
        let capacity = u32::try_from(capacity.clamp(1, 1_000_000)).unwrap_or(1000);
        Self { mailer, slots: Arc::new(Semaphore::new(capacity as usize)), capacity, sending: Arc::new(Semaphore::new(concurrency.max(1))), timeout }
    }

    /// Queue a mail; false (and a WARN line) when the queue is full. Needs a tokio runtime.
    pub(crate) fn enqueue(&self, mail: Mail) -> bool {
        let Ok(slot) = self.slots.clone().try_acquire_owned() else {
            tracing::warn!(to = %mask_address(&mail.to), subject = %mail.subject, "the mail queue is full; a mail was dropped");
            return false;
        };
        let mailer = self.mailer.clone();
        let sending = self.sending.clone();
        let timeout = self.timeout;
        tokio::spawn(async move {
            let _slot = slot;
            let Ok(_permit) = sending.acquire_owned().await else { return };
            match tokio::time::timeout(timeout, mailer.send(&mail)).await {
                Ok(Ok(())) => tracing::debug!(to = %mask_address(&mail.to), subject = %mail.subject, "mail sent"),
                Ok(Err(error)) => tracing::warn!(to = %mask_address(&mail.to), subject = %mail.subject, %error, "a mail could not be sent"),
                Err(_) => tracing::warn!(to = %mask_address(&mail.to), subject = %mail.subject, timeout_secs = timeout.as_secs(), "sending a mail timed out"),
            }
        });
        true
    }

    /// Wait until every queued mail was handled (or `limit` passed); then refuse new ones.
    pub(crate) async fn drain(&self, limit: Duration) {
        if tokio::time::timeout(limit, self.slots.acquire_many(self.capacity)).await.is_err() {
            tracing::warn!("mails still queued at shutdown were dropped");
        }
        self.slots.close();
    }
}

#[cfg(feature = "smtp")]
#[cfg_attr(docsrs, doc(cfg(feature = "smtp")))]
pub use smtp::SmtpMailer;

#[cfg(feature = "smtp")]
mod smtp {
    use std::time::Duration;

    use futures_util::future::BoxFuture;
    use lettre::message::header::ContentType;
    use lettre::message::Mailbox;
    use lettre::transport::smtp::authentication::Credentials;
    use lettre::Address;
    use lettre::{AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor};

    use super::{Mail, MailError, Mailer};
    use crate::auth::SmtpTls;
    use crate::config::SecretString;
    use crate::error::Error;

    /// Sends through an SMTP server with lettre (tokio, rustls + ring, webpki roots). Cargo
    /// feature `smtp`.
    pub struct SmtpMailer {
        transport: AsyncSmtpTransport<Tokio1Executor>,
        from: Mailbox,
    }

    impl std::fmt::Debug for SmtpMailer {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("SmtpMailer").field("from", &self.from.email.to_string()).finish_non_exhaustive()
        }
    }

    impl SmtpMailer {
        /// A mailer for this server. `port` defaults by `tls` (587 / 465 / 25); `tls = None` is
        /// for a relay on the same machine only (the caller checks).
        pub fn new(
            host: &str,
            port: Option<u16>,
            tls: SmtpTls,
            credentials: Option<(String, SecretString)>,
            from: &str,
            timeout: Duration,
        ) -> Result<Self, Error> {
            let config_error = |what: String| Error::Config(vec![format!("modules.auth: {what}")]);
            let from: Mailbox = from.parse().map_err(|_| config_error("mail_from is not a valid address".into()))?;
            let (builder, default_port) = match tls {
                SmtpTls::Starttls => (AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(host).map_err(|e| config_error(format!("smtp: {e}")))?, 587),
                SmtpTls::Tls => (AsyncSmtpTransport::<Tokio1Executor>::relay(host).map_err(|e| config_error(format!("smtp: {e}")))?, 465),
                SmtpTls::None => (AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous(host), 25),
            };
            let mut builder = builder.port(port.unwrap_or(default_port)).timeout(Some(timeout));
            if let Some((user, password)) = credentials {
                builder = builder.credentials(Credentials::new(user, password.expose().to_string()));
            }
            Ok(Self { transport: builder.build(), from })
        }
    }

    impl Mailer for SmtpMailer {
        fn send<'a>(&'a self, mail: &'a Mail) -> BoxFuture<'a, Result<(), MailError>> {
            Box::pin(async move {
                // A bare address only (never `Name <address>`): the recipient is exactly the
                // validated address, a lenient parse cannot redirect the mail.
                let to: Address = mail.to.trim().parse().map_err(|_| MailError("the recipient is not a plain address".into()))?;
                let to = Mailbox::new(None, to);
                let message = Message::builder()
                    .from(self.from.clone())
                    .to(to)
                    .subject(mail.subject.clone())
                    .header(ContentType::TEXT_PLAIN)
                    .body(mail.text.clone())
                    .map_err(|e| MailError(format!("the mail could not be built: {e}")))?;
                self.transport.send(message).await.map(|_| ()).map_err(|e| MailError(format!("SMTP: {e}")))
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn masking_and_debug() {
        assert_eq!(mask_address("ada@example.com"), "a***@example.com");
        assert_eq!(mask_address("nope"), "***");
        let mail = Mail::new("ada@example.com", "Reset", "link: https://x/reset?token=nbse_SECRET");
        let debug = format!("{mail:?}");
        assert!(!debug.contains("SECRET") && !debug.contains("ada@"), "{debug}");
    }

    #[tokio::test]
    async fn queue_is_bounded_and_drains() {
        let memory = Arc::new(MemoryMailer::new());
        let queue = MailQueue::new(memory.clone(), 2, 1, Duration::from_secs(5));
        assert!(queue.enqueue(Mail::new("a@example.com", "1", "x")));
        assert!(queue.enqueue(Mail::new("b@example.com", "2", "x")));
        // Both slots are taken until the tasks ran: the third is dropped.
        assert!(!queue.enqueue(Mail::new("c@example.com", "3", "x")));
        queue.drain(Duration::from_secs(5)).await;
        assert_eq!(memory.sent().len(), 2);
        assert!(!queue.enqueue(Mail::new("d@example.com", "4", "x")), "closed after the drain");
    }
}
