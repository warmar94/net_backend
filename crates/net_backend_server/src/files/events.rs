//! What games can hook into in the files module ([`crate::hooks`]).
//!
//! | Event | Kind | When |
//! |---|---|---|
//! | [`BeforeFileUpload`] | before | the bytes of an upload arrived (size and SHA-256 known), the file is about to be stored; refuse with any error (the bytes are discarded) |
//! | [`BeforeFileUpdate`] | before | the owner is about to change a file's name, visibility or share list (`PATCH /v1/files/{file}`); change them (checked again) or refuse |
//! | [`AfterFileChange`] | after | a file was uploaded, its settings changed, or it was deleted |
//!
//! ```
//! use net_backend_server::files::events::BeforeFileUpload;
//! use net_backend_server::hooks::Decision;
//! use net_backend_server::{AppError, Config, NetBackendServer};
//!
//! let server = NetBackendServer::new(Config::default()).before::<BeforeFileUpload, _, _>(|_ctx, upload| async move {
//!     // The game's rule: replays only as .replay files.
//!     if upload.content_type == "application/x-replay" && !upload.name.ends_with(".replay") {
//!         return Ok(Decision::Reject(AppError::bad_request("a replay must be named *.replay")));
//!     }
//!     Ok(Decision::Continue(upload))
//! });
//! # let _ = server;
//! ```

use net_backend_protocol::files::FileVisibility;
use net_backend_protocol::{FileId, UserId};

use crate::hooks::Event;

/// An upload's bytes arrived; the file is about to be stored. Refuse with any error (the client
/// gets it); changed fields are ignored.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct BeforeFileUpload {
    /// The owner (the uploading player).
    pub owner: UserId,
    /// The file's name.
    pub name: String,
    /// Its content type.
    pub content_type: String,
    /// Its size in bytes.
    pub size: u64,
    /// The SHA-256 of its bytes (lower-case hex).
    pub sha256: String,
    /// Who may read it.
    pub visibility: FileVisibility,
}

impl Event for BeforeFileUpload {
    const NAME: &'static str = "files.before_upload";
}

/// The owner is about to change a file's settings (`PATCH /v1/files/{file}`). Hooks may change
/// `name`, `visibility` and `shared_with` (checked again afterwards; `None` leaves a setting as it
/// is) or refuse: a game that keeps uploads private until it reviewed them refuses `public` here
/// as in [`BeforeFileUpload`].
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct BeforeFileUpdate {
    /// The owner (the player changing it).
    pub owner: UserId,
    /// The file.
    pub file: FileId,
    /// Its visibility now.
    pub current: FileVisibility,
    /// A new name.
    pub name: Option<String>,
    /// A new visibility.
    pub visibility: Option<FileVisibility>,
    /// A new share list (replaces the old one).
    pub shared_with: Option<Vec<UserId>>,
}

impl Event for BeforeFileUpdate {
    const NAME: &'static str = "files.before_update";
}

/// What happened to a file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum FileChange {
    /// Uploaded.
    Uploaded,
    /// Its name, visibility, shares or metadata changed.
    Updated,
    /// Deleted.
    Deleted,
}

/// A file was uploaded, changed or deleted.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct AfterFileChange {
    /// The owner.
    pub owner: UserId,
    /// The file.
    pub file: FileId,
    /// What happened.
    pub change: FileChange,
}

impl Event for AfterFileChange {
    const NAME: &'static str = "files.after_change";
}
