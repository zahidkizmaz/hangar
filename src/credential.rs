//! `hangar credential`: the user's own credentials in the broker, outside
//! the config. `up` never deletes them; it only manages `credentialFiles`.

use std::collections::BTreeMap;
use std::io::{self, IsTerminal, Read};
use std::process::Stdio;

use crate::config::{Settings, Source, managed_credentials, valid_key};
use crate::error::{Context, Error, Result, bail};
use crate::hangar::Hangar;
use crate::process;
use crate::secret::{Secret, trim_line_end};

// Echo comes back even on Ctrl-C (the trap runs in this shell). The value
// leaves on stdout, a pipe to hangar, never as an argument.
const READ_HIDDEN: &str = r#"trap 'stty echo' EXIT INT TERM
printf '%s: ' "$1" >&2
stty -echo
IFS= read -r value
printf '\n' >&2
printf '%s' "$value""#;

/// `credential set`, first half: the checks and the value.
pub(crate) fn read(hangar: &Hangar, name: &str) -> Result<Secret> {
    check_user_key(&hangar.settings, name)?;
    vault_ready(hangar)?;
    let value = read_value(name)?;
    if value.expose().is_empty() {
        bail!("{name}: empty value, nothing stored");
    }
    Ok(value)
}

/// `credential set`, second half, under the lock.
pub(crate) fn store(hangar: &Hangar, name: &str, value: &Secret) -> Result<()> {
    hangar.broker.put_credential(name, value)?;
    log::info!("stored {name}");
    Ok(())
}

pub(crate) struct CredentialList {
    pub(crate) entries: Vec<(String, &'static str)>,
}

pub(crate) fn list(hangar: &Hangar) -> Result<CredentialList> {
    vault_ready(hangar)?;
    let keys = hangar.broker.credential_keys()?;
    Ok(classify(keys, &managed_credentials(&hangar.settings)))
}

fn classify(
    keys: Vec<String>,
    managed: &BTreeMap<String, Source>,
) -> CredentialList {
    let entries = keys
        .into_iter()
        .map(|key| {
            let source = if managed.contains_key(&key) {
                "config"
            } else {
                "user"
            };
            (key, source)
        })
        .collect();
    CredentialList { entries }
}

pub(crate) fn remove(hangar: &Hangar, name: &str) -> Result<()> {
    check_user_key(&hangar.settings, name)?;
    vault_ready(hangar)?;
    hangar.broker.delete_credentials(&[name.to_string()])?;
    log::info!("removed {name}");
    Ok(())
}

fn check_user_key(settings: &Settings, name: &str) -> Result<()> {
    if !valid_key(name) {
        bail!("{name}: expected UPPER_SNAKE_CASE");
    }
    match managed_credentials(settings).get(name) {
        Some(Source::File(_)) => Err(Error::with_hint(
            format!("{name} is managed by tower.credentialFiles"),
            "change your config",
        )),
        Some(Source::App { app, .. }) => Err(Error::with_hint(
            format!("{name} is set by app {app}"),
            "a route uses the app's fixed value",
        )),
        None => Ok(()),
    }
}

fn vault_ready(hangar: &Hangar) -> Result<()> {
    if !hangar.broker.health().healthy {
        return Err(Error::with_hint(
            "the vault isn't running",
            "run 'hangar up' first",
        ));
    }
    Ok(())
}

fn read_value(name: &str) -> Result<Secret> {
    let value = if io::stdin().is_terminal() {
        read_hidden(name, Stdio::inherit())?
    } else {
        let mut value = String::new();
        io::stdin().read_to_string(&mut value).context(name)?;
        value
    };
    Ok(Secret::new(trim_line_end(&value).to_string()))
}

fn read_hidden(name: &str, input: Stdio) -> Result<String> {
    let mut read = process::command("sh");
    read.args(["-c", READ_HIDDEN, "hangar", name])
        .stdin(input)
        .stderr(Stdio::inherit());
    let output = read
        .stdout(Stdio::piped())
        .output()
        .map_err(|error| process::spawn_error(&read, &error))?;
    String::from_utf8(output.stdout).context(name)
}

#[cfg(test)]
mod tests {
    use super::{check_user_key, classify, list, read_hidden, remove};
    use crate::broker::fake::FakeBroker;
    use crate::config::Source;
    use crate::sandbox::fake::FakeSandbox;
    use crate::testing::{hangar_with_broker, scratch_dir, settings};
    use std::collections::BTreeMap;
    use std::fs::{self, File};
    use std::process::Stdio;
    use std::rc::Rc;

    #[test]
    fn credentials_the_config_manages_are_marked_config() {
        let managed = BTreeMap::from([(
            "GITHUB_TOKEN".to_string(),
            Source::File("/t".into()),
        )]);
        let list =
            classify(vec!["GITHUB_TOKEN".into(), "MINE".into()], &managed);
        assert_eq!(
            list.entries,
            [("GITHUB_TOKEN".into(), "config"), ("MINE".into(), "user")]
        );
    }

    #[test]
    fn list_and_remove_go_to_the_broker() {
        let state = scratch_dir("credential-broker");
        let broker = FakeBroker::default();
        broker
            .keys
            .borrow_mut()
            .extend(["GITHUB_TOKEN".into(), "MINE".into()]);
        let calls = broker.calls.clone();
        let config =
            r#"{"tower": {"credentialFiles": {"GITHUB_TOKEN": "/t"}}}"#;
        let sandbox = Rc::new(FakeSandbox::default());
        let hangar =
            hangar_with_broker(config, &state, sandbox, Box::new(broker));
        remove(&hangar, "MINE").unwrap();
        assert_eq!(
            list(&hangar).unwrap().entries,
            [("GITHUB_TOKEN".into(), "config")]
        );
        assert_eq!(*calls.borrow(), ["delete MINE", "credential_keys"]);
    }

    #[test]
    fn managed_and_malformed_names_are_refused() {
        let with_file = settings(
            r#"{"tower": {"credentialFiles": {"GITHUB_TOKEN": "/t"}},
                "bays": [{"name": "default", "apps": ["github-token"]}]}"#,
        );
        let error = check_user_key(&with_file, "GITHUB_TOKEN").unwrap_err();
        assert!(
            error
                .to_string()
                .contains("managed by tower.credentialFiles")
        );
        let error = check_user_key(&with_file, "GITHUB_GIT_USER").unwrap_err();
        assert!(
            error.to_string().contains("set by app github-token"),
            "{error}"
        );
        // A token stored by hand: github-token still gets its git username.
        let by_hand = settings(
            r#"{"bays": [{"name": "default", "apps": ["github-token"]}]}"#,
        );
        assert!(check_user_key(&by_hand, "GITHUB_TOKEN").is_ok());
        assert!(check_user_key(&by_hand, "GITHUB_GIT_USER").is_err());
        assert!(check_user_key(&by_hand, "lower").is_err());
        assert!(check_user_key(&by_hand, "MINE").is_ok());
    }

    #[test]
    fn the_hidden_prompt_returns_the_line_over_a_pipe() {
        let file = scratch_dir("hidden").join("input");
        fs::write(&file, "typed-secret\nignored\n").unwrap();
        // Not a terminal, so stty fails harmlessly; the line still arrives.
        let input = Stdio::from(File::open(&file).unwrap());
        assert_eq!(read_hidden("TOKEN", input).unwrap(), "typed-secret");
    }
}
