//! The proxies' own tokens in the OS keychain: the macOS login Keychain,
//! or the Secret Service (libsecret) on Linux. Secrets only travel on
//! stdin, never in argv.

use std::process::{Command, Output};

use crate::error::{Result, bail};
use crate::process;
use crate::secret::{Secret, trim_line_end};

pub(crate) trait Keychain {
    fn lookup(&self, account: &str) -> Result<Option<Secret>>;
    fn store(&self, account: &str, secret: &Secret) -> Result<()>;
    fn delete(&self, account: &str) -> Result<()>;
}

#[cfg(target_os = "macos")]
pub(crate) const STORE: &str = "the macOS Keychain";
#[cfg(not(target_os = "macos"))]
pub(crate) const STORE: &str = "the Secret Service";

/// Called by absolute path, never found on `PATH`, where a planted copy
/// would be handed the tokens. The Nix package bakes in libsecret's store
/// path.
#[cfg(target_os = "macos")]
const TOOL: &str = "/usr/bin/security";
#[cfg(not(target_os = "macos"))]
const TOOL: &str = match option_env!("HANGAR_SECRET_TOOL") {
    Some(path) => path,
    None => "/usr/bin/secret-tool",
};

pub(crate) struct OsKeychain {
    service: String,
    tool: String,
}

/// What may appear in a `security -i` command line unquoted.
fn plain(text: &str) -> bool {
    !text.is_empty()
        && text
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "._-/".contains(c))
}

/// Tokens go into a `security -i` command line, so they may hold no
/// whitespace or quotes; every real token is base64 or hex anyway.
pub(crate) fn check_token(token: &str) -> Result<()> {
    let printable = |c: char| c.is_ascii_graphic() && !"\"'\\".contains(c);
    if token.is_empty() {
        bail!("the token is empty");
    }
    if !token.chars().all(printable) {
        bail!("a token can't hold spaces, quotes or non-ASCII characters");
    }
    Ok(())
}

impl OsKeychain {
    /// `service` (`$HANGAR_KEYCHAIN_SERVICE`) keeps several hangar setups
    /// (or tests) apart; `hangar` by default.
    pub(crate) fn new(service: Option<String>) -> Result<Self> {
        let service = service.unwrap_or_else(|| "hangar".to_string());
        if !plain(&service) || service.contains('/') {
            bail!(
                "HANGAR_KEYCHAIN_SERVICE: only letters, digits, '.', '_', '-'"
            );
        }
        Ok(Self {
            service,
            tool: tool(),
        })
    }
}

/// Only debug builds read `$HANGAR_TEST_KEYCHAIN_TOOL`, the CLI tests'
/// fake. Release builds compile the lookup out, so nothing at runtime
/// redirects them (`nix/cli.nix` checks the name is not in the binary).
fn tool() -> String {
    #[cfg(debug_assertions)]
    if let Some(fake) = crate::config::env_var("HANGAR_TEST_KEYCHAIN_TOOL") {
        return fake;
    }
    TOOL.to_string()
}

impl Keychain for OsKeychain {
    fn lookup(&self, account: &str) -> Result<Option<Secret>> {
        let mut command = lookup_command(&self.tool, &self.service, account);
        let output = process::output(&mut command, None)?;
        if !output.status.success() {
            if not_found(&output) {
                return Ok(None);
            }
            bail!("keychain lookup failed: {}", process::stderr(&output));
        }
        let Ok(value) = String::from_utf8(output.stdout) else {
            bail!("keychain item {account} is not valid UTF-8");
        };
        Ok(Some(Secret::new(trim_line_end(&value).to_string())))
    }

    fn store(&self, account: &str, secret: &Secret) -> Result<()> {
        check_token(secret.expose())?;
        let (mut command, input) =
            store_command(&self.tool, &self.service, account, secret);
        let output = process::output(&mut command, Some(input.as_bytes()))?;
        if !output.status.success() {
            bail!("could not store {account}: {}", process::stderr(&output));
        }
        Ok(())
    }

    fn delete(&self, account: &str) -> Result<()> {
        let mut command = delete_command(&self.tool, &self.service, account);
        let output = process::output(&mut command, None)?;
        if !output.status.success() && !not_found(&output) {
            bail!("could not delete {account}: {}", process::stderr(&output));
        }
        Ok(())
    }
}

fn tool_command(tool: &str, args: &[&str]) -> Command {
    let mut command = process::command(tool);
    command.args(args);
    command
}

#[cfg(target_os = "macos")]
fn lookup_command(tool: &str, service: &str, account: &str) -> Command {
    let args = ["find-generic-password", "-s", service, "-a", account, "-w"];
    tool_command(tool, &args)
}

#[cfg(target_os = "macos")]
fn delete_command(tool: &str, service: &str, account: &str) -> Command {
    let args = ["delete-generic-password", "-s", service, "-a", account];
    tool_command(tool, &args)
}

/// `security` exits 44 when the item doesn't exist.
#[cfg(target_os = "macos")]
fn not_found(output: &Output) -> bool {
    output.status.code() == Some(44)
}

/// `security -i` reads commands from stdin, so the token never shows up
/// in argv (unlike `add-generic-password -w <token>`). [`check_token`]
/// keeps the line free of anything that would need quoting.
#[cfg(target_os = "macos")]
fn store_command(
    tool: &str,
    service: &str,
    account: &str,
    secret: &Secret,
) -> (Command, String) {
    let input = format!(
        "add-generic-password -U -s {service} -a {account} -w {}\n",
        secret.expose()
    );
    (tool_command(tool, &["-i"]), input)
}

#[cfg(not(target_os = "macos"))]
fn lookup_command(tool: &str, service: &str, account: &str) -> Command {
    tool_command(tool, &["lookup", "service", service, "key", account])
}

#[cfg(not(target_os = "macos"))]
fn delete_command(tool: &str, service: &str, account: &str) -> Command {
    tool_command(tool, &["clear", "service", service, "key", account])
}

/// `secret-tool lookup` exits 1 without output when the item doesn't exist.
#[cfg(not(target_os = "macos"))]
fn not_found(output: &Output) -> bool {
    output.status.code() == Some(1) && output.stderr.is_empty()
}

#[cfg(not(target_os = "macos"))]
fn store_command(
    tool: &str,
    service: &str,
    account: &str,
    secret: &Secret,
) -> (Command, String) {
    let label = format!("--label=hangar {account}");
    let args = [label.as_str(), "service", service, "key", account];
    let args: Vec<&str> = std::iter::once("store").chain(args).collect();
    (tool_command(tool, &args), secret.expose().to_string())
}

#[cfg(test)]
pub(crate) mod fake {
    use std::cell::RefCell;
    use std::collections::BTreeMap;

    use super::{Keychain, check_token};
    use crate::error::Result;
    use crate::secret::Secret;

    #[derive(Default)]
    pub(crate) struct FakeKeychain {
        pub(crate) items: RefCell<BTreeMap<String, String>>,
    }

    impl FakeKeychain {
        pub(crate) fn holding(items: &[(&str, &str)]) -> Self {
            let fake = Self::default();
            for (account, value) in items {
                fake.items
                    .borrow_mut()
                    .insert((*account).into(), (*value).into());
            }
            fake
        }
    }

    impl Keychain for FakeKeychain {
        fn lookup(&self, account: &str) -> Result<Option<Secret>> {
            Ok(self.items.borrow().get(account).cloned().map(Secret::new))
        }

        fn store(&self, account: &str, secret: &Secret) -> Result<()> {
            check_token(secret.expose())?;
            self.items
                .borrow_mut()
                .insert(account.into(), secret.expose().into());
            Ok(())
        }

        fn delete(&self, account: &str) -> Result<()> {
            self.items.borrow_mut().remove(account);
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};

    use super::{Keychain, OsKeychain, TOOL, check_token, store_command, tool};
    use crate::secret::Secret;
    use crate::testing::scratch_dir;

    #[cfg(target_os = "macos")]
    const MISSING: i32 = 44;
    #[cfg(not(target_os = "macos"))]
    const MISSING: i32 = 1;

    /// An `OsKeychain` running a script instead of the real tool; the
    /// script logs its argv to `args` and its stdin to `stdin`.
    fn fake(name: &str, body: &str) -> (OsKeychain, PathBuf) {
        let dir = scratch_dir(&format!("keychain-{name}"));
        let tool = dir.join("tool");
        let script = format!(
            "#!/bin/sh\necho \"$*\" >>\"{0}/args\"\ncat >\"{0}/stdin\"\n{body}\n",
            dir.display()
        );
        fs::write(&tool, script).unwrap();
        fs::set_permissions(&tool, fs::Permissions::from_mode(0o755)).unwrap();
        let keychain = OsKeychain {
            service: "svc".into(),
            tool: tool.display().to_string(),
        };
        (keychain, dir)
    }

    fn read(dir: &Path, name: &str) -> String {
        fs::read_to_string(dir.join(name)).unwrap()
    }

    #[test]
    fn the_tool_is_called_by_an_absolute_path() {
        assert!(TOOL.starts_with('/'), "{TOOL}");
        #[cfg(target_os = "macos")]
        assert_eq!(TOOL, "/usr/bin/security");
        assert_eq!(tool(), TOOL);
        assert_eq!(OsKeychain::new(None).unwrap().tool, TOOL);
    }

    #[test]
    fn the_service_name_defaults_and_allows_only_plain_characters() {
        assert_eq!(OsKeychain::new(None).unwrap().service, "hangar");
        let custom = OsKeychain::new(Some("hangar-e2e.1_a".into())).unwrap();
        assert_eq!(custom.service, "hangar-e2e.1_a");
        for bad in ["two words", "semi;colon", "new\nline", "quote'", "a/b"] {
            assert!(OsKeychain::new(Some(bad.into())).is_err(), "{bad}");
        }
    }

    #[test]
    fn tokens_that_would_need_quoting_are_refused() {
        for fine in ["st.abc-DEF_1", "eyJhbGci.eyJ.sig", "a+b/c=", "0a1b"] {
            assert!(check_token(fine).is_ok(), "{fine}");
        }
        for bad in ["", "two words", "new\nline", "quo\"te", "it's", "a\\b"] {
            assert!(check_token(bad).is_err(), "{bad:?}");
        }
        assert!(check_token("tökén").is_err());
    }

    #[test]
    fn the_token_goes_on_stdin_never_in_argv() {
        let token = Secret::new("st.0a1b2c".into());
        let (command, input) =
            store_command("/bin/tool", "svc", "infi/session-token", &token);
        assert_eq!(command.get_program(), "/bin/tool");
        let args: Vec<String> = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert!(!args.iter().any(|arg| arg.contains("0a1b2c")), "{args:?}");
        assert!(input.contains("st.0a1b2c"));
        #[cfg(target_os = "macos")]
        {
            assert_eq!(args, ["-i"]);
            assert_eq!(
                input,
                "add-generic-password -U -s svc -a infi/session-token \
                 -w st.0a1b2c\n"
            );
        }
        #[cfg(not(target_os = "macos"))]
        {
            assert_eq!(input, "st.0a1b2c");
            assert_eq!(args[0], "store");
        }
    }

    #[test]
    fn a_stored_token_is_looked_up_without_its_line_end() {
        let (keychain, dir) = fake("hit", "echo stored-token");
        let found = keychain.lookup("infi/proxy-token").unwrap().unwrap();
        assert_eq!(found.expose(), "stored-token");
        #[cfg(target_os = "macos")]
        let args = "find-generic-password -s svc -a infi/proxy-token -w\n";
        #[cfg(not(target_os = "macos"))]
        let args = "lookup service svc key infi/proxy-token\n";
        assert_eq!(read(&dir, "args"), args);
    }

    #[test]
    fn a_missing_item_is_none_and_deleting_it_is_fine() {
        let (keychain, dir) = fake("missing", &format!("exit {MISSING}"));
        assert!(keychain.lookup("p/token").unwrap().is_none());
        keychain.delete("p/token").unwrap();
        #[cfg(target_os = "macos")]
        let deleted = "delete-generic-password -s svc -a p/token\n";
        #[cfg(not(target_os = "macos"))]
        let deleted = "clear service svc key p/token\n";
        assert!(read(&dir, "args").ends_with(deleted));
    }

    #[test]
    fn storing_sends_the_token_on_stdin() {
        let (keychain, dir) = fake("store", "exit 0");
        let token = Secret::new("st.abc".into());
        keychain.store("p/token", &token).unwrap();
        assert!(read(&dir, "stdin").contains("st.abc"));
        assert!(!read(&dir, "args").contains("st.abc"));
        let bad = Secret::new("a b".into());
        assert!(keychain.store("p/token", &bad).is_err());
    }

    #[test]
    fn tool_failures_name_what_failed() {
        let (keychain, _) = fake("fails", "echo locked >&2\nexit 2");
        let error = keychain.lookup("p/token").unwrap_err().to_string();
        assert_eq!(error, "keychain lookup failed: locked");
        let token = Secret::new("st.abc".into());
        let error = keychain.store("p/token", &token).unwrap_err().to_string();
        assert_eq!(error, "could not store p/token: locked");
        let error = keychain.delete("p/token").unwrap_err().to_string();
        assert_eq!(error, "could not delete p/token: locked");
    }

    #[test]
    fn a_missing_tool_is_named() {
        let keychain = OsKeychain {
            service: "svc".into(),
            tool: "/nonexistent/keychain-tool".into(),
        };
        let token = Secret::new("st.abc".into());
        for error in [
            keychain.lookup("p/t").map(drop).unwrap_err(),
            keychain.store("p/t", &token).unwrap_err(),
            keychain.delete("p/t").unwrap_err(),
        ] {
            let error = error.to_string();
            assert!(error.contains("/nonexistent/keychain-tool"), "{error}");
        }
    }

    #[test]
    fn an_item_that_is_not_utf8_is_refused() {
        let (keychain, _) = fake("binary", r"printf '\377'");
        let error = keychain.lookup("p/t").unwrap_err().to_string();
        assert_eq!(error, "keychain item p/t is not valid UTF-8");
    }
}
