//! Asking the user for a token: typed unseen on the terminal, or one line
//! of stdin when there is no terminal (`op read … | hangar secret set`).

use std::fs::{File, OpenOptions};
use std::io::{self, BufRead, BufReader, IsTerminal, Write};
use std::process::Stdio;

use crate::error::{Context, Result, bail};
use crate::process;
use crate::secret::{Secret, trim_line_end};

pub(crate) trait Prompt {
    /// Whether a person is there to type.
    fn terminal(&self) -> bool;
    /// The answer to `question`, without its line end.
    fn ask(&self, question: &str) -> Result<Secret>;
}

pub(crate) struct Terminal;

impl Prompt for Terminal {
    fn terminal(&self) -> bool {
        io::stdin().is_terminal()
    }

    fn ask(&self, question: &str) -> Result<Secret> {
        let line = if self.terminal() {
            unseen(question)?
        } else {
            let mut line = String::new();
            io::stdin().read_line(&mut line).context("stdin")?;
            line
        };
        Ok(Secret::new(trim_line_end(&line).to_string()))
    }
}

/// Reads a line from the terminal with its echo off; the echo comes back
/// even when the read fails.
fn unseen(question: &str) -> Result<String> {
    let tty = OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/tty")
        .context("/dev/tty")?;
    write!(&tty, "{question}: ").context("/dev/tty")?;
    let echo = Echo::off(&tty)?;
    let mut line = String::new();
    let read = BufReader::new(&tty).read_line(&mut line);
    drop(echo);
    writeln!(&tty).context("/dev/tty")?;
    read.context("/dev/tty")?;
    Ok(line)
}

/// The terminal's echo, off until dropped. `stty` acts on its stdin, the
/// terminal; it never sees what is typed.
struct Echo(File);

impl Echo {
    fn off(tty: &File) -> Result<Self> {
        stty(tty, "-echo")?;
        Ok(Self(tty.try_clone().context("/dev/tty")?))
    }
}

impl Drop for Echo {
    fn drop(&mut self) {
        if let Err(error) = stty(&self.0, "echo") {
            log::warn!("{error}; run 'stty echo'");
        }
    }
}

fn stty(tty: &File, setting: &str) -> Result<()> {
    let mut command = process::command("stty");
    let stdin = tty.try_clone().context("/dev/tty")?;
    command.arg(setting).stdin(Stdio::from(stdin));
    if !process::status(&mut command)?.success() {
        bail!("stty {setting} failed");
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod fake {
    use std::cell::RefCell;

    use super::Prompt;
    use crate::error::{Result, bail};
    use crate::secret::Secret;

    /// Answers from a list, and records each question.
    #[derive(Default)]
    pub(crate) struct FakePrompt {
        pub(crate) terminal: bool,
        pub(crate) answers: RefCell<Vec<String>>,
        pub(crate) questions: RefCell<Vec<String>>,
    }

    impl FakePrompt {
        pub(crate) fn answering(answers: &[&str]) -> Self {
            Self {
                terminal: true,
                answers: RefCell::new(
                    answers.iter().rev().map(|a| (*a).to_string()).collect(),
                ),
                questions: RefCell::default(),
            }
        }
    }

    impl Prompt for FakePrompt {
        fn terminal(&self) -> bool {
            self.terminal
        }

        fn ask(&self, question: &str) -> Result<Secret> {
            self.questions.borrow_mut().push(question.to_string());
            match self.answers.borrow_mut().pop() {
                Some(answer) => Ok(Secret::new(answer)),
                None => bail!("no answer for {question}"),
            }
        }
    }
}
