//! The questions of `net-backend new` (inquire), asked when no flag was given and the terminal is
//! interactive. Each one fills an answer the flags would otherwise give.

use std::fmt;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};

use inquire::validator::Validation;
use inquire::{Confirm, InquireError, MultiSelect, Select, Text};

use crate::modules::{self, Module, MODULES};
use crate::names;
use crate::options::{Answers, ClientKind, Database};
use crate::style;

/// Whether questions can be asked: stdin and stdout are a terminal.
pub fn interactive() -> bool {
    std::io::stdin().is_terminal() && std::io::stdout().is_terminal()
}

/// An option of a question: its value and what the question shows.
struct Choice<T> {
    value: T,
    label: &'static str,
}

impl<T> fmt::Display for Choice<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label)
    }
}

/// A section title before a question (styled output only).
fn section(styled: bool, title: &str, keys: &str) {
    if styled {
        println!("{}", style::section(true, title, keys));
    }
}

/// A choice from a list; styled, the keys are in the section title instead of inquire's help.
fn select<'a, T: fmt::Display>(styled: bool, message: &'a str, options: Vec<T>) -> Select<'a, T> {
    let select = Select::new(message, options);
    if styled {
        select.without_help_message().with_formatter(&|option| style::one_line(&option.value.to_string()))
    } else {
        select
    }
}

/// Asks every question the answers leave open. `Err` with a message when the user cancels.
pub fn ask(mut answers: Answers) -> Result<Answers, String> {
    let styled = style::on();
    if styled {
        inquire::set_global_render_config(style::render_config());
    } else if style::plain_asked() {
        inquire::set_global_render_config(inquire::ui::RenderConfig::empty());
    }
    let cancel = |error: InquireError| match error {
        InquireError::OperationCanceled | InquireError::OperationInterrupted => "cancelled; nothing was written".to_string(),
        other => other.to_string(),
    };
    if answers.target.is_none() {
        section(styled, "Project", style::KEYS_TEXT);
        let name = Text::new("Project name?")
            .with_default("mygame")
            .with_help_message("the folder to create (a-z, 0-9, - and _)")
            .with_validator(|input: &str| {
                let checked = names::name_of(Path::new(input.trim())).and_then(names::validate_name);
                Ok(checked.map_or_else(|problem| Validation::Invalid(problem.to_string().into()), |()| Validation::Valid))
            })
            .prompt()
            .map_err(cancel)?;
        answers.target = Some(PathBuf::from(name.trim()));
    }

    let clients: Vec<Choice<ClientKind>> = ClientKind::ALL.into_iter().map(|value| Choice { value, label: value.label() }).collect();
    section(styled, "Client", style::KEYS_SELECT);
    let client = select(styled, "How will your game talk to the server?", clients).prompt().map_err(cancel)?.value;
    answers.client = Some(client);

    let databases: Vec<Choice<Database>> = Database::ALL.into_iter().map(|value| Choice { value, label: value.label() }).collect();
    section(styled, "Database", style::KEYS_SELECT);
    answers.database = Some(select(styled, "Database?", databases).prompt().map_err(cancel)?.value);

    let all: Vec<Choice<&'static Module>> = MODULES.iter().map(|value| Choice { value, label: value.label }).collect();
    let ticked: Vec<usize> = MODULES.iter().enumerate().filter(|(_, m)| m.default).map(|(index, _)| index).collect();
    section(styled, "Modules", style::KEYS_MULTI);
    let question = if styled { "Server modules?" } else { "Server modules? (space toggles; every module needs auth, which is added)" };
    let multi = MultiSelect::new(question, all);
    let multi = if styled {
        multi
            .with_help_message("every module needs auth, which is added")
            .with_formatter(&|picked| style::module_answer(&picked.iter().map(|o| o.value.value.label).collect::<Vec<_>>()))
    } else {
        multi
    };
    let picked = multi.with_default(&ticked).with_page_size(MODULES.len()).prompt().map_err(cancel)?;
    let names: Vec<&str> = picked.iter().map(|c| c.value.name).collect();
    let resolved = modules::resolve(names.iter().copied())?;
    if resolved.len() > names.len() {
        if styled {
            println!("{}", style::badged(true, style::Kind::Info, "auth added: every module needs it"));
        } else {
            println!("  (auth added: every module needs it)");
        }
    }
    answers.modules = Some(resolved);

    if client.has_demo() {
        let what = if client == ClientKind::Rust { "a small window" } else { "a small Bevy app" };
        section(styled, "Demo", style::KEYS_CONFIRM);
        let demo = Confirm::new(&format!("Add a demo app to try it ({what} with buttons and a log)?")).with_default(true).prompt().map_err(cancel)?;
        answers.demo = Some(demo);
        if demo && client == ClientKind::Bevy {
            let places = vec![
                Choice { value: false, label: "In this new project (demo/)" },
                Choice { value: true, label: "Next to an existing Bevy game (its own folder; the game is not changed)" },
            ];
            section(styled, "Demo folder", style::KEYS_SELECT);
            let next_to_game = select(styled, "Where does the Bevy demo go?", places).prompt().map_err(cancel)?.value;
            if next_to_game {
                section(styled, "Existing game", style::KEYS_TEXT);
                let game = Text::new("The existing game's folder?")
                    .with_help_message("the demo goes into net_backend_demo/ next to it")
                    .with_validator(|input: &str| {
                        Ok(if Path::new(input.trim()).join("Cargo.toml").is_file() {
                            Validation::Valid
                        } else {
                            Validation::Invalid("no Cargo.toml there (the folder of an existing Bevy game)".into())
                        })
                    })
                    .prompt()
                    .map_err(cancel)?;
                answers.existing = Some(PathBuf::from(game.trim()));
            }
        }
    } else {
        answers.demo = Some(false);
    }

    let build = match (client, answers.demo) {
        (ClientKind::Bevy, Some(true)) => " (the first Bevy build takes 5 to 10 minutes)",
        (ClientKind::Rust, Some(true)) => " (the first build of the demo takes about 1 to 2 minutes)",
        _ => "",
    };
    section(styled, "Start", style::KEYS_CONFIRM);
    let question = if styled {
        match (client, answers.demo) {
            (ClientKind::Bevy, Some(true)) => println!("{}", style::badged(true, style::Kind::Warn, "The first Bevy build takes 5 to 10 minutes.")),
            (ClientKind::Rust, Some(true)) => println!("{}", style::badged(true, style::Kind::Info, "The first build of the demo takes about 1 to 2 minutes.")),
            _ => {}
        }
        "Start the server (and the demo) when done?".to_string()
    } else {
        format!("Start the server (and the demo) when done?{build}")
    };
    let run = Confirm::new(&question).with_default(false).prompt().map_err(cancel)?;
    answers.run = Some(run);
    Ok(answers)
}
