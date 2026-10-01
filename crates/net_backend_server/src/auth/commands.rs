//! The auth module's commands: `user:create`, `user:role`, `user:ban`, `user:unban`,
//! `sessions:revoke`. Each is audited as `cli.*`. Passwords never come from the command line
//! itself (it is visible in the process list): from a file, or generated and printed once.

use std::sync::Arc;

use futures_util::future::BoxFuture;
use net_backend_protocol::admin::BanRequest;
use net_backend_protocol::{UnixMillis, UserId};

use super::service::{Actor, AuthService};
use crate::command::{AppCommand, CommandArgs, CommandCtx};
use crate::error::Error;

fn service(ctx: &CommandCtx<'_>) -> Result<Arc<AuthService>, Error> {
    ctx.state().get::<AuthService>().ok_or_else(|| Error::Cli("the auth module is not set up".into()))
}

/// An answer error as a command error (the client-facing message is fine for an operator).
fn cli(error: crate::AppError) -> Error {
    let api = error.api_error();
    let mut text = format!("{}: {}", api.code, api.message);
    if let Some(details) = &api.details {
        text.push_str(&format!(" {details}"));
    }
    Error::Cli(text)
}

pub(crate) fn all() -> Vec<Arc<dyn AppCommand>> {
    vec![Arc::new(UserCreate), Arc::new(UserRole), Arc::new(UserBan), Arc::new(UserUnban), Arc::new(SessionsRevoke)]
}

struct UserCreate;

impl AppCommand for UserCreate {
    fn name(&self) -> &'static str {
        "user:create"
    }

    fn about(&self) -> &'static str {
        "Create an account (password from a file, or generated and printed once)"
    }

    fn usage(&self) -> &'static str {
        "<email> [--name <display name>] [--password-file <file>] [--admin] [--verified]"
    }

    fn run<'a>(&'a self, mut ctx: CommandCtx<'a>, args: &'a [String]) -> BoxFuture<'a, Result<(), Error>> {
        Box::pin(async move {
            let args = CommandArgs::parse(args, &["--name", "--password-file"], &["--admin", "--verified"])?;
            let email = args.required(0, "email")?.to_string();
            let (password, generated) = match args.option("--password-file") {
                Some(path) => {
                    let text = std::fs::read_to_string(path).map_err(|e| Error::io(format!("reading {path}"), e))?;
                    (text.trim_end_matches(['\r', '\n']).to_string(), false)
                }
                None => {
                    let bytes = super::tokens::random_bytes::<12>().map_err(cli)?;
                    (bytes.iter().map(|b| format!("{b:02x}")).collect::<String>(), true)
                }
            };
            let service = service(&ctx)?;
            let user = service
                .create_user(ctx.state(), &email, password.clone(), args.option("--name").map(str::to_string), args.flag("--admin"), args.flag("--verified"))
                .await
                .map_err(cli)?;
            ctx.println(format!("Created user {} ({email}){}", user.get(), if args.flag("--admin") { " with the admin role" } else { "" }))?;
            if generated {
                ctx.println(format!("Password (shown once): {password}"))?;
            }
            Ok(())
        })
    }
}

struct UserRole;

impl AppCommand for UserRole {
    fn name(&self) -> &'static str {
        "user:role"
    }

    fn about(&self) -> &'static str {
        "Grant a role to an account (or revoke it with --revoke)"
    }

    fn usage(&self) -> &'static str {
        "<email or id> <role> [--revoke]"
    }

    fn run<'a>(&'a self, mut ctx: CommandCtx<'a>, args: &'a [String]) -> BoxFuture<'a, Result<(), Error>> {
        Box::pin(async move {
            let args = CommandArgs::parse(args, &[], &["--revoke"])?;
            let key = args.required(0, "email or id")?;
            let role = args.required(1, "role")?;
            let service = service(&ctx)?;
            let user = service.find_user(ctx.state(), key).await.map_err(cli)?;
            let grant = !args.flag("--revoke");
            service.set_role(ctx.state(), &Actor::cli(), UserId(user.id), role, grant).await.map_err(cli)?;
            ctx.println(format!("{} role `{role}` {} user {}", if grant { "Granted" } else { "Revoked" }, if grant { "to" } else { "from" }, user.id))
        })
    }
}

struct UserBan;

impl AppCommand for UserBan {
    fn name(&self) -> &'static str {
        "user:ban"
    }

    fn about(&self) -> &'static str {
        "Ban an account (revokes its sessions)"
    }

    fn usage(&self) -> &'static str {
        "<email or id> [--reason <text>] [--hours <n>]"
    }

    fn run<'a>(&'a self, mut ctx: CommandCtx<'a>, args: &'a [String]) -> BoxFuture<'a, Result<(), Error>> {
        Box::pin(async move {
            let args = CommandArgs::parse(args, &["--reason", "--hours"], &[])?;
            let key = args.required(0, "email or id")?;
            let service = service(&ctx)?;
            let user = service.find_user(ctx.state(), key).await.map_err(cli)?;
            let mut request = BanRequest::new();
            if let Some(reason) = args.option("--reason") {
                request = request.with_reason(reason);
            }
            if let Some(hours) = args.option("--hours") {
                let hours: i64 =
                    hours.parse().ok().filter(|h| *h > 0 && *h <= 24 * 365 * 100).ok_or_else(|| Error::Cli("--hours must be a positive number".into()))?;
                request = request.with_until(UnixMillis(ctx.state().now().get().saturating_add(hours * 3_600_000)));
            }
            service.ban(ctx.state(), &Actor::cli(), UserId(user.id), request).await.map_err(cli)?;
            ctx.println(format!("Banned user {}", user.id))
        })
    }
}

struct UserUnban;

impl AppCommand for UserUnban {
    fn name(&self) -> &'static str {
        "user:unban"
    }

    fn about(&self) -> &'static str {
        "Lift an account's ban"
    }

    fn usage(&self) -> &'static str {
        "<email or id>"
    }

    fn run<'a>(&'a self, mut ctx: CommandCtx<'a>, args: &'a [String]) -> BoxFuture<'a, Result<(), Error>> {
        Box::pin(async move {
            let args = CommandArgs::parse(args, &[], &[])?;
            let key = args.required(0, "email or id")?;
            let service = service(&ctx)?;
            let user = service.find_user(ctx.state(), key).await.map_err(cli)?;
            service.unban(ctx.state(), &Actor::cli(), UserId(user.id)).await.map_err(cli)?;
            ctx.println(format!("Unbanned user {}", user.id))
        })
    }
}

struct SessionsRevoke;

impl AppCommand for SessionsRevoke {
    fn name(&self) -> &'static str {
        "sessions:revoke"
    }

    fn about(&self) -> &'static str {
        "Revoke every session of an account (log it out everywhere)"
    }

    fn usage(&self) -> &'static str {
        "<email or id>"
    }

    fn run<'a>(&'a self, mut ctx: CommandCtx<'a>, args: &'a [String]) -> BoxFuture<'a, Result<(), Error>> {
        Box::pin(async move {
            let args = CommandArgs::parse(args, &[], &[])?;
            let key = args.required(0, "email or id")?;
            let service = service(&ctx)?;
            let user = service.find_user(ctx.state(), key).await.map_err(cli)?;
            let count = service.admin_revoke(ctx.state(), &Actor::cli(), UserId(user.id)).await.map_err(cli)?;
            ctx.println(format!("Revoked {count} session(s) of user {}", user.id))
        })
    }
}
