use std::fs;
use std::path::Path;

use crate::config::Var;
use crate::error::{Error, Result, bail};
use crate::keychain::{Keychain, STORE};
use crate::secret::{Secret, random_hex, trim_line_end};

/// First match wins: `$HANGAR_MASTER_PASSWORD`,
/// `$HANGAR_MASTER_PASSWORD_FILE`, `masterPasswordFile` from the config,
/// then the OS keychain. A new install gets a generated password; one with
/// broker data (`data`) never does, because a new password would lock it.
/// An empty password would silently create an unencrypted vault.
pub(crate) fn master_password(
    var: Var,
    config_file: Option<&str>,
    keychain: &dyn Keychain,
    data: Option<&Path>,
) -> Result<Secret> {
    let value = if let Some(value) = var("HANGAR_MASTER_PASSWORD") {
        value
    } else if let Some(file) = var("HANGAR_MASTER_PASSWORD_FILE")
        .or_else(|| config_file.map(str::to_string))
        .filter(|file| !file.is_empty())
    {
        let Ok(value) = fs::read_to_string(&file) else {
            bail!("master password file not readable: {file}");
        };
        value
    } else if let Some(secret) = keychain.lookup()? {
        secret.expose().to_string()
    } else if let Some(data) = data {
        return Err(Error::with_hint(
            format!(
                "master password missing from {STORE} but broker data exists \
                 in {}",
                data.display()
            ),
            "restore the keychain item or run 'hangar destroy --state' to \
             start fresh",
        ));
    } else {
        let password = Secret::new(random_hex(32)?);
        keychain.store(&password)?;
        log::info!(
            "generated a master password for this install (stored in {STORE})"
        );
        return Ok(password);
    };
    let value = trim_line_end(&value);
    if value.is_empty() {
        bail!("master password is empty");
    }
    // The tower reads it as one line; a second line would be dropped.
    if value.contains(['\r', '\n']) {
        bail!("master password must be a single line");
    }
    Ok(Secret::new(value.to_string()))
}

#[cfg(test)]
mod tests {
    use super::master_password;
    use crate::keychain::fake::FakeKeychain;
    use crate::testing::scratch_dir;
    use std::fs;
    use std::path::Path;

    fn scratch(name: &str, contents: &str) -> String {
        let path = scratch_dir(name).join("secret");
        fs::write(&path, contents).unwrap();
        path.to_string_lossy().into_owned()
    }

    fn no_data() -> Option<&'static Path> {
        None
    }

    fn none(_: &str) -> Option<String> {
        None
    }

    #[test]
    fn env_value_beats_files_and_keychain() {
        let file = scratch("ignored", "from-file\n");
        let var = |name: &str| match name {
            "HANGAR_MASTER_PASSWORD" => Some("from-env".to_string()),
            _ => None,
        };
        let keychain = FakeKeychain::holding("from-keychain");
        let secret =
            master_password(&var, Some(&file), &keychain, no_data()).unwrap();
        assert_eq!(secret.expose(), "from-env");
    }

    #[test]
    fn env_file_beats_config_file_and_drops_newline() {
        let env_file = scratch("env", "from-env-file\n");
        let config_file = scratch("config", "from-config\n");
        let var = |name: &str| match name {
            "HANGAR_MASTER_PASSWORD_FILE" => Some(env_file.clone()),
            _ => None,
        };
        let keychain = FakeKeychain::default();
        let secret =
            master_password(&var, Some(&config_file), &keychain, no_data())
                .unwrap();
        assert_eq!(secret.expose(), "from-env-file");
        assert_eq!(*keychain.stores.borrow(), 0);
    }

    #[test]
    fn config_file_beats_keychain() {
        let config_file = scratch("config-only", "from-config\n");
        let keychain = FakeKeychain::holding("from-keychain");
        let secret =
            master_password(&none, Some(&config_file), &keychain, no_data())
                .unwrap();
        assert_eq!(secret.expose(), "from-config");
    }

    #[test]
    fn new_install_generates_once_then_reuses() {
        let keychain = FakeKeychain::default();
        let first = master_password(&none, None, &keychain, no_data()).unwrap();
        assert_eq!(first.expose().len(), 64);
        assert_eq!(keychain.item.borrow().as_deref(), Some(first.expose()));

        let second =
            master_password(&none, None, &keychain, no_data()).unwrap();
        assert_eq!(second.expose(), first.expose());
        assert_eq!(*keychain.stores.borrow(), 1);
    }

    #[test]
    fn existing_vault_without_keychain_item_never_generates() {
        let data = scratch_dir("vault").join("data");
        let keychain = FakeKeychain::default();
        let error = master_password(&none, None, &keychain, Some(&data))
            .unwrap_err()
            .to_string();
        assert!(error.contains("but broker data exists in"), "{error}");
        assert!(error.contains("hangar destroy --state"));
        assert_eq!(*keychain.stores.borrow(), 0);
    }

    #[test]
    fn unreadable_and_empty_files_are_errors() {
        let keychain = FakeKeychain::default();
        let missing = Path::new("/nonexistent/password").to_str().unwrap();
        let error = master_password(&none, Some(missing), &keychain, no_data())
            .unwrap_err()
            .to_string();
        assert!(error.contains("not readable: /nonexistent/password"));

        let empty = scratch("empty", "\n");
        let error = master_password(&none, Some(&empty), &keychain, no_data())
            .unwrap_err();
        assert_eq!(error.to_string(), "master password is empty");
        assert_eq!(*keychain.stores.borrow(), 0);
    }

    #[test]
    fn crlf_is_trimmed_but_a_second_line_is_rejected() {
        let keychain = FakeKeychain::default();
        let crlf = scratch("crlf", "secret\r\n");
        let secret =
            master_password(&none, Some(&crlf), &keychain, no_data()).unwrap();
        assert_eq!(secret.expose(), "secret");

        let two = scratch("two-lines", "secret\nmore\n");
        let error = master_password(&none, Some(&two), &keychain, no_data())
            .unwrap_err()
            .to_string();
        assert!(error.contains("single line"), "{error}");
    }
}
