//! Matchmaking: the protocol's `/v1/matchmaking` routes and the `match.found` / `match.expired`
//! pushes (cargo feature `matchmaking`, module [`Matchmaking`]). The game decides who plays with
//! whom; the server keeps the queues and tells the players.
//!
//! - **Queues** come from the configuration (`[[modules.matchmaking.queues]]`: a key, the players
//!   per match, how long a ticket waits). `GET /v1/matchmaking/queues` lists them with how many
//!   tickets wait.
//! - **Tickets:** a player queues with `POST /v1/matchmaking/ticket` (a queue and `attributes`:
//!   what the game's rules read), one ticket at a time; cancels with `DELETE`; reads it with `GET`
//!   (waiting, or matched with its match). A waiting ticket runs out after its queue's
//!   `timeout_secs` (push `match.expired`). With `cancel_on_disconnect`, a player whose last
//!   WebSocket connection on this instance closes leaves its queue.
//! - **Rounds:** every `interval_ms`, each queue with waiting tickets is matched. The default rule
//!   is first come, first matched, `players` per match; the game's
//!   [`MatchmakingRound`](events::MatchmakingRound) hook replaces it with its own (ratings,
//!   regions, parties, a wider search the longer a ticket waits) and attaches `data` to a match (a
//!   lobby it created, a server address, the teams). Each matched player gets `match.found`.
//! - **Hooks** ([`events`]): [`BeforeTicketCreate`](events::BeforeTicketCreate) (change or refuse a
//!   ticket), [`MatchmakingRound`](events::MatchmakingRound) (the game's rules),
//!   [`AfterMatchFound`](events::AfterMatchFound).
//!
//! **Storage:** tickets live in the memory of the server instance the player queued on (they last
//! seconds to minutes, and a round needs every ticket of a queue in one place); nothing is
//! written to the database. A server restart empties the queues.
//!
//! The module needs [`Auth`](crate::auth::Auth) registered before it; pushes need the WebSocket hub
//! (`ws.enabled`), and a client without one reads its ticket over HTTP.

pub mod config;
pub mod events;
mod module;
mod openapi;
mod routes;
mod service;

pub use config::{MatchmakingConfig, QueueSpec};
pub use module::Matchmaking;
pub use service::MatchmakingService;
