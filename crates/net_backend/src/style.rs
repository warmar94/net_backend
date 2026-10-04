//! The installer's look in a terminal: the banner, section titles, badges, progress lines, the
//! spinner and the summary box, drawn with `crossterm`.
//!
//! Styling is on only when stdout is a terminal that understands ANSI sequences (on Windows,
//! virtual terminal processing is switched on through crossterm), `NO_COLOR` is not set and
//! `--no-color` was not given. Otherwise every message is printed exactly as plain text.

use std::fmt::Display;
use std::io::{BufRead, BufReader, IsTerminal, Write};
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use crossterm::style::{Attribute, Color, Stylize};
use crossterm::terminal::{Clear, ClearType};
use crossterm::{cursor, queue};
use inquire::ui::{Attributes, Color as InquireColor, ErrorMessageRenderConfig, RenderConfig, StyleSheet, Styled};

/// Test-only: `NET_BACKEND_FORCE_STYLE=1` styles the output even when it is not a terminal (to
/// record what a terminal shows). `NO_COLOR` and `--no-color` still win.
const FORCE: &str = "NET_BACKEND_FORCE_STYLE";

/// (stdout styled, stderr styled, plain text asked for), decided once by [`init`].
static STATE: OnceLock<(bool, bool, bool)> = OnceLock::new();

/// The accent colours (256-colour palette, shown the same by Windows Terminal, conhost, macOS
/// Terminal and Linux terminals).
const CYAN: Color = Color::AnsiValue(45);
const VIOLET: Color = Color::AnsiValue(141);
const GREEN: Color = Color::AnsiValue(42);
const RED: Color = Color::AnsiValue(203);
const DIM: Color = Color::AnsiValue(245);
/// The banner's rows, top to bottom: cyan to violet.
const GRADIENT: [Color; 6] =
    [Color::AnsiValue(51), Color::AnsiValue(45), Color::AnsiValue(39), Color::AnsiValue(69), Color::AnsiValue(99), Color::AnsiValue(135)];

/// Whether output is styled: no `--no-color`, no `NO_COLOR`, and a terminal (or forced).
pub fn decide(no_color_flag: bool, no_color_env: bool, forced: bool, terminal: bool) -> bool {
    !no_color_flag && !no_color_env && (forced || terminal)
}

/// Decides once whether stdout and stderr are styled.
pub fn init(no_color_flag: bool) {
    STATE.get_or_init(|| {
        let no_color = std::env::var_os("NO_COLOR").is_some_and(|v| !v.is_empty());
        let forced = std::env::var_os(FORCE).is_some_and(|v| v == "1");
        let out = decide(no_color_flag, no_color, forced, std::io::stdout().is_terminal());
        let err = decide(no_color_flag, no_color, forced, std::io::stderr().is_terminal());
        // A real terminal must understand ANSI sequences; forced output goes to a file.
        let ansi = forced || ansi_terminal();
        (out && ansi, err && ansi, no_color_flag || no_color)
    });
}

/// Windows: switches on virtual terminal processing (false when the console refuses).
#[cfg(windows)]
fn ansi_terminal() -> bool {
    crossterm::ansi_support::supports_ansi()
}

#[cfg(not(windows))]
fn ansi_terminal() -> bool {
    std::env::var("TERM").map_or(true, |term| term != "dumb")
}

/// Whether stdout is styled.
pub fn on() -> bool {
    STATE.get().is_some_and(|s| s.0)
}

/// Whether `--no-color` or `NO_COLOR` asked for plain text.
pub fn plain_asked() -> bool {
    STATE.get().is_some_and(|s| s.2)
}

fn err_on() -> bool {
    STATE.get().is_some_and(|s| s.1)
}

fn paint(on: bool, text: impl Display, color: Color) -> String {
    if on {
        text.to_string().with(color).to_string()
    } else {
        text.to_string()
    }
}

fn bold(on: bool, text: impl Display, color: Color) -> String {
    if on {
        text.to_string().with(color).attribute(Attribute::Bold).to_string()
    } else {
        text.to_string()
    }
}

fn dim(on: bool, text: impl Display) -> String {
    paint(on, text, DIM)
}

// ---------------------------------------------------------------------------------------------
// The banner

/// The block letters: six rows, six columns each (`█`, and `▀` / `▄` for half rows).
const fn glyph(letter: char) -> [&'static str; 6] {
    match letter {
        'N' => ["██▄ ██", "███ ██", "███▄██", "██▀███", "██ ███", "██ ▀██"],
        'E' => ["██████", "██    ", "██▄▄▄ ", "██▀▀▀ ", "██    ", "██████"],
        'T' => ["██████", "  ██  ", "  ██  ", "  ██  ", "  ██  ", "  ██  "],
        'B' => ["█████▄", "██  ██", "██▄▄█▀", "██▀▀█▄", "██  ██", "█████▀"],
        'A' => ["▄████▄", "██  ██", "██▄▄██", "██▀▀██", "██  ██", "██  ██"],
        'C' => ["▄█████", "██    ", "██    ", "██    ", "██    ", "▀█████"],
        'K' => ["██  ██", "██ ▄█▀", "██▄█▀ ", "██▀█▄ ", "██ ▀█▄", "██  ██"],
        'D' => ["█████▄", "██  ██", "██  ██", "██  ██", "██  ██", "█████▀"],
        _ => [" "; 6],
    }
}

const BANNER_TEXT: &str = "NET BACKEND";

/// The banner's rows without colour (two spaces in, one column between letters).
fn banner_rows() -> Vec<String> {
    (0..6)
        .map(|row| {
            let line: Vec<&str> = BANNER_TEXT.chars().map(|c| glyph(c)[row]).collect();
            format!("  {}", line.join(" ")).trim_end().to_string()
        })
        .collect()
}

/// The big "NET BACKEND" banner with a cyan-to-violet gradient and the subtitle.
pub fn banner(on: bool, version: &str) -> String {
    let mut text = String::from("\n");
    for (row, line) in banner_rows().iter().enumerate() {
        text.push_str(&bold(on, line, GRADIENT[row]));
        text.push('\n');
    }
    text.push_str(&format!("\n  {}  {}\n", dim(on, "Build your own game backend in Rust"), paint(on, format!("v{version}"), VIOLET)));
    text
}

// ---------------------------------------------------------------------------------------------
// Badges and messages

/// The kind of a message: its badge and colour.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Info,
    Warn,
    Done,
    Error,
}

impl Kind {
    fn word(self) -> &'static str {
        match self {
            Kind::Info => "INFO",
            Kind::Warn => "WARN",
            Kind::Done => "DONE",
            Kind::Error => "ERROR",
        }
    }

    /// (background, text) of the badge.
    fn colors(self) -> (Color, Color) {
        match self {
            Kind::Info => (Color::AnsiValue(33), Color::White),
            Kind::Warn => (Color::AnsiValue(214), Color::Black),
            Kind::Done => (Color::AnsiValue(35), Color::Black),
            Kind::Error => (Color::AnsiValue(160), Color::White),
        }
    }
}

/// ` WARN ` on a coloured block (plain: the word with the same padding).
pub fn badge(on: bool, kind: Kind) -> String {
    let word = format!(" {} ", kind.word());
    if on {
        let (bg, fg) = kind.colors();
        word.with(fg).on(bg).attribute(Attribute::Bold).to_string()
    } else {
        word
    }
}

/// A badge and its message, indented by two.
pub fn badged(on: bool, kind: Kind, message: &str) -> String {
    let message = if kind == Kind::Error { paint(on, message, RED) } else { message.to_string() };
    format!("  {} {message}", badge(on, kind))
}

/// Prints a message: plain text exactly as given when unstyled, else with its badge (leading
/// blank lines kept).
pub fn say(kind: Kind, text: &str) {
    if on() {
        let body = text.trim_start_matches('\n');
        let blank = &text[..text.len() - body.len()];
        println!("{blank}{}", badged(true, kind, body));
    } else {
        println!("{text}");
    }
}

/// An error on stderr: `error: …` plain, a red ERROR badge styled; `more` follows unchanged.
pub fn error(problem: &str, more: &str) {
    if err_on() {
        eprintln!("\n{}{more}", badged(true, Kind::Error, problem));
    } else {
        eprintln!("error: {problem}{more}");
    }
}

/// A green `✓` line (a step done), with an optional dim note.
pub fn check(on: bool, what: &str, note: &str) -> String {
    let note = if note.is_empty() { String::new() } else { format!("  {}", dim(on, note)) };
    format!("  {} {what}{note}", bold(on, "✓", GREEN))
}

// ---------------------------------------------------------------------------------------------
// Questions

/// A section title before a question: a coloured marker, the title, a dim rule, and the keys.
pub fn section(on: bool, title: &str, keys: &str) -> String {
    let rule = "─".repeat(60usize.saturating_sub(title.chars().count() + 4));
    format!("\n  {} {}  {}\n  {}", bold(on, "◆", VIOLET), bold(on, title, Color::White), dim(on, rule), dim(on, keys))
}

/// The keys of a choice from a list, a multi-choice, a yes / no and a text.
pub const KEYS_SELECT: &str = "↑↓ move · type to filter · enter confirm";
pub const KEYS_MULTI: &str = "↑↓ move · space toggle · → all · ← none · enter confirm";
pub const KEYS_CONFIRM: &str = "y / n · enter takes the default";
pub const KEYS_TEXT: &str = "type · enter confirm";

/// The questions' theme: a cyan `›` cursor, `●` / `○` boxes, coloured answers, dim help, red
/// errors.
pub fn render_config() -> RenderConfig<'static> {
    let cyan = InquireColor::AnsiValue(45);
    let green = InquireColor::AnsiValue(42);
    let dim = InquireColor::AnsiValue(245);
    let red = InquireColor::AnsiValue(203);
    RenderConfig::empty()
        .with_prompt_prefix(Styled::new("?").with_fg(cyan).with_attr(Attributes::BOLD))
        .with_answered_prompt_prefix(Styled::new("✓").with_fg(green).with_attr(Attributes::BOLD))
        .with_highlighted_option_prefix(Styled::new("›").with_fg(cyan).with_attr(Attributes::BOLD))
        .with_selected_checkbox(Styled::new("●").with_fg(green))
        .with_unselected_checkbox(Styled::new("○").with_fg(dim))
        .with_scroll_up_prefix(Styled::new("↑").with_fg(dim))
        .with_scroll_down_prefix(Styled::new("↓").with_fg(dim))
        .with_selected_option(Some(StyleSheet::new().with_fg(cyan).with_attr(Attributes::BOLD)))
        .with_answer(StyleSheet::new().with_fg(cyan).with_attr(Attributes::BOLD))
        .with_help_message(StyleSheet::new().with_fg(dim))
        .with_default_value(StyleSheet::new().with_fg(dim))
        .with_canceled_prompt_indicator(Styled::new("cancelled").with_fg(red))
        .with_error_message(
            ErrorMessageRenderConfig::empty()
                .with_prefix(Styled::new("✗").with_fg(red).with_attr(Attributes::BOLD))
                .with_message(StyleSheet::new().with_fg(red)),
        )
}

/// A confirmed answer on one line: runs of spaces become one.
pub fn one_line(label: &str) -> String {
    label.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The confirmed modules answer: the short names, comma-separated, and how many
/// (`accounts, saves, chat (3)`); `none` when nothing is ticked.
pub fn module_answer(labels: &[&str]) -> String {
    if labels.is_empty() {
        return "none".to_string();
    }
    let short = |label: &str| {
        let head = label.split(':').next().unwrap_or(label);
        head.split(" (").next().unwrap_or(head).trim().to_string()
    };
    format!("{} ({})", labels.iter().map(|l| short(l)).collect::<Vec<_>>().join(", "), labels.len())
}

// ---------------------------------------------------------------------------------------------
// The summary box

/// The terminal's width (80 when unknown).
fn width() -> usize {
    crossterm::terminal::size().map_or(80, |(w, _)| usize::from(w)).clamp(40, 200)
}

/// Wraps `text` into lines of at most `width` characters (words first, long words split).
fn wrap(text: &str, width: usize) -> Vec<String> {
    let mut lines = vec![String::new()];
    for word in text.split(' ') {
        let mut word: Vec<char> = word.chars().collect();
        loop {
            let line = lines.last_mut().expect("one line");
            let used = line.chars().count();
            let gap = usize::from(used > 0);
            if used + gap + word.len() <= width {
                if gap == 1 {
                    line.push(' ');
                }
                line.extend(word.iter());
                break;
            }
            if used > 0 {
                lines.push(String::new());
                continue;
            }
            // A long word (a path) breaks after its last separator that fits, else at the width.
            let fit = width.min(word.len());
            let cut = word[..fit].iter().rposition(|c| *c == '/' || *c == '\\').map_or(fit, |i| i + 1);
            let rest = word.split_off(cut);
            line.extend(word.iter());
            lines.push(String::new());
            word = rest;
        }
    }
    lines
}

/// A rounded box around rows of `(label, value)`, under a title line; values wrap inside.
pub fn summary_box(on: bool, title: &str, rows: &[(&str, String)], max_width: usize) -> String {
    let label_width = rows.iter().map(|(l, _)| l.chars().count()).max().unwrap_or(0);
    let value_room = max_width.saturating_sub(label_width + 8).max(20);
    let wrapped: Vec<(&str, Vec<String>)> = rows.iter().map(|(l, v)| (*l, wrap(v, value_room))).collect();
    let title_width = title_width(title);
    let inner = wrapped.iter().flat_map(|(_, lines)| lines.iter().map(|l| label_width + 2 + l.chars().count())).chain([title_width]).max().unwrap_or(0) + 4;
    let edge = |s: &str| paint(on, s, DIM);
    let mut text = format!("  {}\n", edge(&format!("╭{}╮", "─".repeat(inner))));
    let line = |content: &str, visible: usize| format!("  {}  {content}{}{}\n", edge("│"), " ".repeat(inner - 2 - visible), edge("│"));
    text.push_str(&line(title, title_width));
    text.push_str(&line("", 0));
    for (label, lines) in &wrapped {
        for (index, value) in lines.iter().enumerate() {
            let label = if index == 0 { format!("{label:<label_width$}") } else { " ".repeat(label_width) };
            let content = format!("{}  {value}", dim(on, &label));
            text.push_str(&line(&content, label_width + 2 + value.chars().count()));
        }
    }
    text.push_str(&format!("  {}\n", edge(&format!("╰{}╯", "─".repeat(inner)))));
    text
}

/// The visible width of a title that may hold ANSI sequences.
fn title_width(text: &str) -> usize {
    strip_ansi(text).chars().count()
}

/// `text` without ANSI escape sequences.
pub fn strip_ansi(text: &str) -> String {
    let mut out = String::new();
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            if chars.peek() == Some(&'[') {
                chars.next();
                for c in chars.by_ref() {
                    if c.is_ascii_alphabetic() {
                        break;
                    }
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// The box width for the summary in this terminal.
pub fn box_width() -> usize {
    width().min(80) - 4
}

pub fn heading(on: bool, text: &str) -> String {
    format!("  {}", bold(on, text, Color::White))
}

pub fn command(on: bool, text: &str) -> String {
    format!("  {} {}", paint(on, "$", DIM), bold(on, text, CYAN))
}

pub fn step(on: bool, command: &str, comment: &str) -> String {
    let comment = if comment.is_empty() { String::new() } else { format!("  {}", dim(on, format!("# {comment}"))) };
    format!("  {} {}{comment}", paint(on, "›", VIOLET), bold(on, command, Color::White))
}

pub fn note(on: bool, text: &str) -> String {
    format!("  {}", dim(on, text))
}

/// `DONE Created <name> <note>`: the summary's title.
pub fn title(on: bool, name: &str, note: &str) -> String {
    format!("{} {} {}", badge(on, Kind::Done), bold(on, format!("Created {name}"), Color::White), dim(on, format!("· {note}")))
}

// ---------------------------------------------------------------------------------------------
// The spinner

const FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

fn elapsed(time: Duration) -> String {
    let secs = time.as_secs();
    if secs >= 60 {
        format!("{}m {:02}s", secs / 60, secs % 60)
    } else {
        format!("{secs}s")
    }
}

/// Runs `command` (a build) under a spinner showing `label`, the time and cargo's latest
/// `Compiling …` line; the spinner line is cleared at the end, then a green `✓` (or a red line
/// and everything cargo printed). `Err` when it failed.
pub fn spin(label: &str, mut command: Command) -> Result<(), String> {
    let started = Instant::now();
    let mut child = command
        .env("CARGO_TERM_COLOR", "always")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("cannot start cargo: {e}"))?;
    let (sender, lines) = std::sync::mpsc::channel::<String>();
    if let Some(stderr) = child.stderr.take() {
        std::thread::spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                if sender.send(line).is_err() {
                    break;
                }
            }
        });
    }
    let mut output: Vec<String> = Vec::new();
    let mut latest = String::new();
    let mut frame = 0;
    let mut out = std::io::stdout();
    let status = loop {
        while let Ok(line) = lines.try_recv() {
            let plain = strip_ansi(&line);
            let plain = plain.trim();
            if ["Compiling", "Checking", "Downloaded", "Building"].iter().any(|w| plain.starts_with(w)) {
                latest = plain.to_string();
            }
            output.push(line);
        }
        if let Some(status) = child.try_wait().map_err(|e| format!("cargo: {e}"))? {
            break status;
        }
        let status_text = format!("{} {}  {}", FRAMES[frame % FRAMES.len()], label, elapsed(started.elapsed()));
        let room = width().saturating_sub(status_text.chars().count() + 6);
        let detail: String = latest.chars().take(room).collect();
        let _ = queue!(out, cursor::MoveToColumn(0), Clear(ClearType::CurrentLine));
        let _ = write!(out, "  {} {}", paint(true, status_text, CYAN), dim(true, detail));
        let _ = out.flush();
        frame += 1;
        std::thread::sleep(Duration::from_millis(80));
    };
    // The reader thread ends with the pipe: collect the last lines.
    while let Ok(line) = lines.recv_timeout(Duration::from_millis(200)) {
        output.push(line);
    }
    let _ = queue!(out, cursor::MoveToColumn(0), Clear(ClearType::CurrentLine));
    let _ = out.flush();
    if status.success() {
        println!("{}", check(true, label, &format!("({})", elapsed(started.elapsed()))));
        Ok(())
    } else {
        for line in &output {
            eprintln!("{line}");
        }
        Err(format!("{label} failed ({status})"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn banner_fits_80_columns() {
        for letter in BANNER_TEXT.chars().filter(|c| *c != ' ') {
            assert!(glyph(letter).iter().all(|row| row.chars().count() == 6), "{letter}");
        }
        let rows = banner_rows();
        assert_eq!(rows.len(), 6);
        for row in &rows {
            assert!(row.chars().count() <= 80, "{} columns: {row}", row.chars().count());
            assert!(row.chars().all(|c| " █▀▄".contains(c)), "{row}");
        }
        // Plain: no escape sequences; every line of the coloured one is as wide once stripped.
        let plain = banner(false, "0.2.0");
        assert!(!plain.contains('\x1b') && plain.contains("v0.2.0"));
        let coloured = banner(true, "0.2.0");
        assert!(coloured.contains('\x1b'));
        assert_eq!(strip_ansi(&coloured), plain);
        assert!(plain.lines().all(|l| l.chars().count() <= 80));
    }

    #[test]
    fn badges_without_colour() {
        assert_eq!(badge(false, Kind::Warn), " WARN ");
        assert_eq!(badge(false, Kind::Error), " ERROR ");
        assert_eq!(badged(false, Kind::Info, "auth added"), "   INFO  auth added");
        assert_eq!(check(false, "server/", "the game server"), "  ✓ server/  the game server");
        for kind in [Kind::Info, Kind::Warn, Kind::Done, Kind::Error] {
            let coloured = badge(true, kind);
            assert!(coloured.contains("\x1b[") && strip_ansi(&coloured) == badge(false, kind), "{kind:?}");
        }
    }

    #[test]
    fn short_answers() {
        assert_eq!(one_line("Rust client      (server + protocol + net_backend_client)"), "Rust client (server + protocol + net_backend_client)");
        let labels: Vec<&str> = crate::modules::MODULES.iter().map(|m| m.label).collect();
        let answer = module_answer(&labels);
        assert!(answer.starts_with("accounts, saves, chat, leaderboards,") && answer.ends_with(", files (11)"), "{answer}");
        assert!(answer.contains("OpenID Connect logins,") && !answer.contains(':'), "{answer}");
        assert_eq!(module_answer(&[]), "none");
    }

    #[test]
    fn styling_decision() {
        assert!(decide(false, false, false, true), "a terminal");
        assert!(!decide(false, false, false, false), "a pipe or a file");
        assert!(decide(false, false, true, false), "forced");
        assert!(!decide(true, false, true, true), "--no-color");
        assert!(!decide(false, true, true, true), "NO_COLOR");
    }

    #[test]
    fn boxes_and_wrapping() {
        assert_eq!(wrap("a bb ccc", 4), ["a bb", "ccc"]);
        assert_eq!(wrap("abcdefghij", 4), ["abcd", "efgh", "ij"]);
        let text = summary_box(false, "Created g", &[("Project", "x".repeat(100)), ("Db", "SQLite".into())], 76);
        let widths: Vec<usize> = text.lines().map(|l| l.chars().count()).collect();
        assert!(widths.iter().all(|w| *w == widths[0] && *w <= 80), "{widths:?}\n{text}");
        let coloured = summary_box(true, "Created g", &[("Db", "SQLite".into())], 76);
        assert_eq!(strip_ansi(&coloured), summary_box(false, "Created g", &[("Db", "SQLite".into())], 76));
    }

    /// The spinner runs a command to its end: `Ok` on success, `Err` naming it on failure.
    #[test]
    fn spinner_reports_the_result() {
        let mut ok = Command::new("cargo");
        ok.arg("--version");
        assert!(spin("cargo --version", ok).is_ok());
        let mut failing = Command::new("cargo");
        failing.arg("no-such-subcommand-here");
        assert!(spin("cargo no-such-subcommand-here", failing).unwrap_err().contains("cargo no-such-subcommand-here failed"));
    }

    /// Writes the banner and samples of the styled parts to `NET_BACKEND_PREVIEW` (a file).
    #[test]
    #[ignore]
    fn preview() {
        let path = std::env::var("NET_BACKEND_PREVIEW").expect("NET_BACKEND_PREVIEW");
        let mut text = banner(true, crate::generate::CRATES_VERSION);
        text.push_str(&format!("{}\n", section(true, "Client", KEYS_SELECT)));
        text.push_str(&format!("{}\n", section(true, "Modules", KEYS_MULTI)));
        for kind in [Kind::Info, Kind::Warn, Kind::Done, Kind::Error] {
            text.push_str(&format!("\n{}", badged(true, kind, "a message")));
        }
        text.push('\n');
        std::fs::write(path, text).unwrap();
    }
}
