//! Asking the user: for a token, typed unseen on the terminal or one line
//! of stdin when there is no terminal (`op read … | hangar secret set`);
//! or to confirm, on stderr like other diagnostics.

use std::fs::{File, OpenOptions};
use std::io::{self, BufRead, BufReader, IsTerminal, Read, Write};
use std::process::Stdio;

use crate::error::{Context, Result, bail};
use crate::process;
use crate::secret::{Secret, trim_line_end};

pub(crate) trait Prompt {
    /// Whether a person is there to type.
    fn terminal(&self) -> bool;
    /// The answer to `question`, without its line end.
    fn ask(&self, question: &str) -> Result<Secret>;
    /// Whether the user answered yes to `question`.
    fn confirm(&self, question: &str) -> Result<bool>;
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

    fn confirm(&self, question: &str) -> Result<bool> {
        eprint!("{question} [y/N] ");
        let mut answer = String::new();
        io::stdin().read_line(&mut answer).context("terminal")?;
        Ok(is_yes(&answer))
    }
}

fn is_yes(answer: &str) -> bool {
    matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}

fn unseen(question: &str) -> Result<String> {
    let tty = OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/tty")
        .context("/dev/tty")?;
    let echo = |on: bool| stty(&tty, if on { "echo" } else { "-echo" });
    read_hidden(&tty, &tty, question, echo)
}

/// Reads a line from `input` with the echo off; the echo comes back even
/// when the read fails.
fn read_hidden(
    input: impl Read,
    mut out: impl Write,
    question: &str,
    mut echo: impl FnMut(bool) -> Result<()>,
) -> Result<String> {
    write!(out, "{question}: ").context("/dev/tty")?;
    echo(false)?;
    let mut line = String::new();
    let read = BufReader::new(input).read_line(&mut line);
    if let Err(error) = echo(true) {
        log::warn!("{error}; run 'stty echo'");
    }
    writeln!(out).context("/dev/tty")?;
    read.context("/dev/tty")?;
    Ok(line)
}

/// `stty` acts on its stdin, the terminal; it never sees what is typed.
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

        fn confirm(&self, question: &str) -> Result<bool> {
            Ok(self.ask(question)?.expose() == "y")
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::fs::File;
    use std::io::{self, Cursor, Read};

    use super::{is_yes, read_hidden, stty};
    use crate::error::{Result, bail};
    use crate::testing::scratch_dir;

    fn echo_into(calls: &RefCell<Vec<bool>>) -> impl FnMut(bool) -> Result<()> {
        |on| {
            calls.borrow_mut().push(on);
            Ok(())
        }
    }

    #[test]
    fn a_hidden_read_turns_the_echo_off_only_while_reading() {
        let calls = RefCell::new(Vec::new());
        let mut out = Vec::new();
        let line = read_hidden(
            Cursor::new("tok\n"),
            &mut out,
            "Token",
            echo_into(&calls),
        )
        .unwrap();
        assert_eq!(line, "tok\n");
        assert_eq!(out, b"Token: \n");
        assert_eq!(*calls.borrow(), [false, true]);
    }

    struct Broken;

    impl Read for Broken {
        fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
            Err(io::Error::other("gone"))
        }
    }

    #[test]
    fn the_echo_comes_back_when_the_read_fails() {
        let calls = RefCell::new(Vec::new());
        let error = read_hidden(Broken, Vec::new(), "Token", echo_into(&calls))
            .unwrap_err();
        assert_eq!(error.to_string(), "/dev/tty: gone");
        assert_eq!(*calls.borrow(), [false, true]);
    }

    #[test]
    fn nothing_is_read_when_the_echo_cannot_be_turned_off() {
        let calls = RefCell::new(Vec::new());
        let failing = |on: bool| -> Result<()> {
            calls.borrow_mut().push(on);
            bail!("stty -echo failed")
        };
        let error =
            read_hidden(Broken, Vec::new(), "Token", failing).unwrap_err();
        assert_eq!(error.to_string(), "stty -echo failed");
        assert_eq!(*calls.borrow(), [false]);
    }

    #[test]
    fn an_echo_that_cannot_come_back_still_returns_the_line() {
        let calls = RefCell::new(Vec::new());
        let stuck = |on: bool| -> Result<()> {
            calls.borrow_mut().push(on);
            if on {
                bail!("stty echo failed");
            }
            Ok(())
        };
        let line =
            read_hidden(Cursor::new("tok\n"), Vec::new(), "Token", stuck)
                .unwrap();
        assert_eq!(line, "tok\n");
        assert_eq!(*calls.borrow(), [false, true]);
    }

    #[test]
    fn stty_on_something_that_is_not_a_terminal_fails() {
        let path = scratch_dir("prompt-stty").join("not-a-tty");
        let file = File::create(&path).unwrap();
        let error = stty(&file, "-echo").unwrap_err();
        assert_eq!(error.to_string(), "stty -echo failed");
    }

    #[test]
    fn only_y_or_yes_confirms() {
        for answer in ["y\n", "Yes\n", " YES "] {
            assert!(is_yes(answer), "{answer}");
        }
        for answer in ["", "\n", "n\n", "yep\n"] {
            assert!(!is_yes(answer), "{answer}");
        }
    }
}
