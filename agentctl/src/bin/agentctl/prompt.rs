//! Line prompts on the controlling terminal, outside the TUI.

use std::io::{IsTerminal as _, Write as _};

use agent::Error;
use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use zeroize::Zeroizing;

/// Whether both standard input and standard error are a terminal a person can answer on.
pub(crate) fn interactive() -> bool {
    std::io::stdin().is_terminal() && std::io::stderr().is_terminal()
}

/// Asks a yes-or-no question on standard error; anything but `y` or `yes` is no.
///
/// # Errors
///
/// Returns an error when standard input cannot be read.
pub(crate) fn confirm(question: &str) -> Result<bool, Error> {
    eprint!("{question} [y/N] ");
    std::io::stderr().flush().ok();
    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer)?;
    Ok(matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes"))
}

/// Reads a secret typed or pasted on the terminal without echoing it.
///
/// # Errors
///
/// Returns an error when the terminal cannot be read, the user presses Ctrl-C, or nothing was entered.
pub(crate) fn secret(label: &str) -> Result<Zeroizing<String>, Error> {
    eprint!("{label}: ");
    std::io::stderr().flush().ok();
    crossterm::terminal::enable_raw_mode()?;
    let read = read_secret();
    let _ = crossterm::terminal::disable_raw_mode();
    eprintln!();
    let value = read?;
    if value.trim().is_empty() {
        return Err(Error::Invalid("nothing was entered".into()));
    }
    Ok(value)
}

fn read_secret() -> Result<Zeroizing<String>, Error> {
    let mut value = Zeroizing::new(String::new());
    loop {
        let Event::Key(KeyEvent {
            code, modifiers, kind, ..
        }) = crossterm::event::read()?
        else {
            continue;
        };
        if kind == KeyEventKind::Release {
            continue;
        }
        match code {
            KeyCode::Enter => return Ok(value),
            KeyCode::Char('c') if modifiers.contains(KeyModifiers::CONTROL) => {
                return Err(Error::Invalid("cancelled".into()));
            }
            KeyCode::Char(character) => value.push(character),
            KeyCode::Backspace => {
                value.pop();
            }
            _ => {}
        }
    }
}
