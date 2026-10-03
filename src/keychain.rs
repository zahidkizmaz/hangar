//! The master password in the OS keychain: the macOS login Keychain, or
//! the Secret Service (libsecret) on Linux. Secrets only travel on stdin.

use std::process::{Command, Output};

use crate::error::{Result, bail};
use crate::process;
use crate::secret::{Secret, trim_line_end};

pub(crate) trait Keychain {
    fn lookup(&self) -> Result<Option<Secret>>;
    fn store(&self, password: &Secret) -> Result<()>;
    fn delete(&self) -> Result<()>;
}

const ACCOUNT: &str = "master-password";

#[cfg(target_os = "macos")]
pub(crate) const STORE: &str = "the macOS Keychain";
#[cfg(not(target_os = "macos"))]
pub(crate) const STORE: &str = "the Secret Service";

/// Called by absolute path, never found on `PATH`, where a planted copy
/// would be handed the master password. The Nix package bakes in
/// libsecret's store path.
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

impl OsKeychain {
    /// `service` (`$HANGAR_KEYCHAIN_SERVICE`) lets several hangar setups
    /// (or tests) keep separate passwords; `hangar` by default. It ends up
    /// in a `security -i` command line, so only plain characters are
    /// allowed.
    pub(crate) fn new(service: Option<String>) -> Result<Self> {
        let service = service.unwrap_or_else(|| "hangar".to_string());
        let plain = |c: char| c.is_ascii_alphanumeric() || "._-".contains(c);
        if !service.chars().all(plain) {
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
    fn lookup(&self) -> Result<Option<Secret>> {
        let output = process::output(
            &mut lookup_command(&self.tool, &self.service),
            None,
        )?;
        if !output.status.success() {
            if not_found(&output) {
                return Ok(None);
            }
            bail!("keychain lookup failed: {}", process::stderr(&output));
        }
        let Ok(value) = String::from_utf8(output.stdout) else {
            bail!("keychain item is not valid UTF-8");
        };
        Ok(Some(Secret::new(trim_line_end(&value).to_string())))
    }

    fn store(&self, password: &Secret) -> Result<()> {
        let (mut command, input) =
            store_command(&self.tool, &self.service, password);
        let output = process::output(&mut command, Some(input.as_bytes()))?;
        if !output.status.success() {
            bail!(
                "could not store the master password: {}",
                process::stderr(&output)
            );
        }
        Ok(())
    }

    fn delete(&self) -> Result<()> {
        let output = process::output(
            &mut delete_command(&self.tool, &self.service),
            None,
        )?;
        if !output.status.success() && !not_found(&output) {
            bail!(
                "could not delete the master password: {}",
                process::stderr(&output)
            );
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
fn lookup_command(tool: &str, service: &str) -> Command {
    let args = ["find-generic-password", "-s", service, "-a", ACCOUNT, "-w"];
    tool_command(tool, &args)
}

#[cfg(target_os = "macos")]
fn delete_command(tool: &str, service: &str) -> Command {
    let args = ["delete-generic-password", "-s", service, "-a", ACCOUNT];
    tool_command(tool, &args)
}

/// `security` exits 44 when the item doesn't exist.
#[cfg(target_os = "macos")]
fn not_found(output: &Output) -> bool {
    output.status.code() == Some(44)
}

/// `security -i` reads commands from stdin, so the password never shows up
/// in argv (unlike `add-generic-password -w <password>`). It's hex, so the
/// command line needs no quoting.
#[cfg(target_os = "macos")]
fn store_command(
    tool: &str,
    service: &str,
    password: &Secret,
) -> (Command, String) {
    let input = format!(
        "add-generic-password -U -s {service} -a {ACCOUNT} -w {}\n",
        password.expose()
    );
    (tool_command(tool, &["-i"]), input)
}

#[cfg(not(target_os = "macos"))]
fn lookup_command(tool: &str, service: &str) -> Command {
    tool_command(tool, &["lookup", "service", service, "key", ACCOUNT])
}

#[cfg(not(target_os = "macos"))]
fn delete_command(tool: &str, service: &str) -> Command {
    tool_command(tool, &["clear", "service", service, "key", ACCOUNT])
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
    password: &Secret,
) -> (Command, String) {
    let args = [
        "store",
        "--label=hangar master password",
        "service",
        service,
        "key",
        ACCOUNT,
    ];
    (tool_command(tool, &args), password.expose().to_string())
}

#[cfg(test)]
pub(crate) mod fake {
    use super::Keychain;
    use crate::error::Result;
    use crate::secret::Secret;
    use std::cell::RefCell;

    #[derive(Default)]
    pub(crate) struct FakeKeychain {
        pub(crate) item: RefCell<Option<String>>,
        pub(crate) stores: RefCell<usize>,
    }

    impl FakeKeychain {
        pub(crate) fn holding(value: &str) -> Self {
            let fake = Self::default();
            *fake.item.borrow_mut() = Some(value.to_string());
            fake
        }
    }

    impl Keychain for FakeKeychain {
        fn lookup(&self) -> Result<Option<Secret>> {
            Ok(self.item.borrow().clone().map(Secret::new))
        }

        fn store(&self, password: &Secret) -> Result<()> {
            *self.item.borrow_mut() = Some(password.expose().to_string());
            *self.stores.borrow_mut() += 1;
            Ok(())
        }

        fn delete(&self) -> Result<()> {
            *self.item.borrow_mut() = None;
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Keychain, OsKeychain, TOOL, store_command, tool};
    use crate::secret::Secret;
    use crate::testing::scratch_dir;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;

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

    fn read(dir: &std::path::Path, name: &str) -> String {
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
        for bad in ["two words", "semi;colon", "new\nline", "quote'"] {
            assert!(OsKeychain::new(Some(bad.into())).is_err(), "{bad}");
        }
    }

    #[test]
    fn the_password_goes_on_stdin_never_in_argv() {
        let password = Secret::new("0a1b2c".into());
        let (command, input) = store_command("/bin/tool", "svc", &password);
        assert_eq!(command.get_program(), "/bin/tool");
        let args: Vec<String> = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert!(!args.iter().any(|arg| arg.contains("0a1b2c")), "{args:?}");
        assert!(input.contains("0a1b2c"));
        #[cfg(target_os = "macos")]
        {
            assert_eq!(args, ["-i"]);
            assert_eq!(
                input,
                "add-generic-password -U -s svc -a master-password -w 0a1b2c\n"
            );
        }
        #[cfg(not(target_os = "macos"))]
        assert_eq!(input, "0a1b2c");
    }

    #[test]
    fn a_stored_password_is_looked_up_without_its_line_end() {
        let (keychain, dir) = fake("hit", "echo stored-password");
        let found = keychain.lookup().unwrap().unwrap();
        assert_eq!(found.expose(), "stored-password");
        #[cfg(target_os = "macos")]
        let args = "find-generic-password -s svc -a master-password -w\n";
        #[cfg(not(target_os = "macos"))]
        let args = "lookup service svc key master-password\n";
        assert_eq!(read(&dir, "args"), args);
    }

    #[test]
    fn a_missing_item_is_none_and_deleting_it_is_fine() {
        let (keychain, dir) = fake("missing", &format!("exit {MISSING}"));
        assert!(keychain.lookup().unwrap().is_none());
        keychain.delete().unwrap();
        #[cfg(target_os = "macos")]
        let deleted = "delete-generic-password -s svc -a master-password\n";
        #[cfg(not(target_os = "macos"))]
        let deleted = "clear service svc key master-password\n";
        assert!(read(&dir, "args").ends_with(deleted));
    }

    #[test]
    fn tool_failures_name_what_failed() {
        let (keychain, _) = fake("fails", "echo locked >&2\nexit 2");
        let error = keychain.lookup().unwrap_err().to_string();
        assert_eq!(error, "keychain lookup failed: locked");
        let password = Secret::new("0a1b2c".into());
        let error = keychain.store(&password).unwrap_err().to_string();
        assert_eq!(error, "could not store the master password: locked");
        let error = keychain.delete().unwrap_err().to_string();
        assert_eq!(error, "could not delete the master password: locked");
    }

    #[test]
    fn a_missing_tool_is_named() {
        let keychain = OsKeychain {
            service: "svc".into(),
            tool: "/nonexistent/keychain-tool".into(),
        };
        let password = Secret::new("0a1b2c".into());
        for error in [
            keychain.lookup().map(drop).unwrap_err(),
            keychain.store(&password).unwrap_err(),
            keychain.delete().unwrap_err(),
        ] {
            let error = error.to_string();
            assert!(error.contains("/nonexistent/keychain-tool"), "{error}");
        }
    }

    #[test]
    fn an_item_that_is_not_utf8_is_refused() {
        let (keychain, _) = fake("binary", r"printf '\377'");
        let error = keychain.lookup().unwrap_err().to_string();
        assert_eq!(error, "keychain item is not valid UTF-8");
    }
}
