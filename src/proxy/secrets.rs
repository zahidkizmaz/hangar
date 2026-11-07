//! A proxy's own tokens, the ones it enrols or logs in with. Each lives in
//! the OS keychain as `<proxy>/<key>` (`hangar secret set <proxy>`), or
//! comes from a command the config names:
//! `"secrets": {"<key>": {"command": ["op", "read", "op://…"]}}`.

use std::collections::BTreeMap;
use std::rc::Rc;

use crate::config::Fields;
use crate::error::{Error, Result, bail};
use crate::keychain::{Keychain, STORE, check_token};
use crate::process;
use crate::secret::{Secret, trim_line_end};

/// One token a kind needs, and what to tell the user it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Slot {
    pub(crate) key: &'static str,
    pub(crate) about: &'static str,
    /// Only the bays use it, so `up` asks for it when one of them comes
    /// up; the proxy starts without it.
    pub(crate) for_bays: bool,
}

#[derive(Clone)]
pub(crate) struct Secrets {
    proxy: String,
    keychain: Rc<dyn Keychain>,
    commands: BTreeMap<String, Vec<String>>,
}

impl Secrets {
    /// Reads the optional `secrets` setting of proxy `proxy`.
    pub(crate) fn new(
        proxy: &str,
        keychain: Rc<dyn Keychain>,
        fields: &Fields,
    ) -> Result<Self> {
        let mut commands = BTreeMap::new();
        if fields.map.contains_key("secrets") {
            let secrets = fields.object("secrets")?;
            for key in secrets.map.keys() {
                let source = secrets.object(key)?;
                source.only(&["command"])?;
                let command = source.strings("command")?;
                if command.is_empty() {
                    bail!("{}: expected a command", source.at("command"));
                }
                commands.insert(key.clone(), command);
            }
        }
        Ok(Self {
            proxy: proxy.to_string(),
            keychain,
            commands,
        })
    }

    /// Refuses a `secrets` entry for a token the kind doesn't take: one of
    /// `slots`, or a bay's own (`extra`).
    pub(crate) fn check(
        &self,
        slots: &[Slot],
        extra: &[String],
        path: &str,
    ) -> Result<()> {
        let known = |key: &String| {
            slots.iter().any(|slot| slot.key == key) || extra.contains(key)
        };
        if let Some(key) = self.commands.keys().find(|key| !known(key)) {
            let keys: Vec<&str> = slots
                .iter()
                .map(|slot| slot.key)
                .chain(extra.iter().map(String::as_str))
                .collect();
            let keys = if keys.is_empty() {
                "none".to_string()
            } else {
                keys.join(", ")
            };
            bail!("{path}.secrets.{key}: unknown token; known: {keys}");
        }
        Ok(())
    }

    fn account(&self, key: &str) -> String {
        format!("{}/{key}", self.proxy)
    }

    pub(crate) fn get(&self, key: &str) -> Result<Secret> {
        if let Some(command) = self.commands.get(key) {
            return self.run(key, command);
        }
        self.keychain.lookup(&self.account(key))?.ok_or_else(|| {
            Error::with_hint(
                format!("no {key} for proxy {} in {STORE}", self.proxy),
                format!("run 'hangar secret set {}'", self.proxy),
            )
        })
    }

    fn run(&self, key: &str, command: &[String]) -> Result<Secret> {
        let what = format!("proxy {}: {key} command", self.proxy);
        let mut child = process::command(&command[0]);
        child.args(&command[1..]);
        let output = process::output(&mut child, None)?;
        if !output.status.success() {
            bail!("{what} failed: {}", process::stderr(&output));
        }
        let Ok(text) = String::from_utf8(output.stdout) else {
            bail!("{what} printed something that isn't UTF-8");
        };
        let token = trim_line_end(&text);
        check_token(token)
            .map_err(|why| Error::new(format!("{what}: {why}")))?;
        Ok(Secret::new(token.to_string()))
    }

    /// Whether `key` is there: from a command, or in the keychain.
    pub(crate) fn has(&self, key: &str) -> Result<bool> {
        Ok(self.commands.contains_key(key)
            || self.keychain.lookup(&self.account(key))?.is_some())
    }

    /// Whether `key` comes from a command, so `secret set` leaves it.
    pub(crate) fn is_command(&self, key: &str) -> bool {
        self.commands.contains_key(key)
    }

    pub(crate) fn store(&self, key: &str, token: &Secret) -> Result<()> {
        check_token(token.expose()).map_err(|why| {
            Error::new(format!("proxy {}: {key}: {why}", self.proxy))
        })?;
        self.keychain.store(&self.account(key), token)
    }

    pub(crate) fn delete(&self, keys: &[String]) -> Result<()> {
        for key in keys {
            self.keychain.delete(&self.account(key))?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::rc::Rc;

    use super::{Secrets, Slot};
    use crate::config::Fields;
    use crate::json;
    use crate::keychain::fake::FakeKeychain;
    use crate::secret::Secret;

    const SLOTS: [Slot; 2] = [
        Slot {
            key: "proxy-token",
            about: "the proxy token",
            for_bays: false,
        },
        Slot {
            key: "session-token",
            about: "the session token",
            for_bays: true,
        },
    ];

    fn secrets(fields: &str, keychain: &Rc<FakeKeychain>) -> Secrets {
        let value = json::parse(fields).unwrap();
        let fields = Fields::new(&value, "proxies.infi").unwrap();
        Secrets::new("infi", keychain.clone(), &fields).unwrap()
    }

    fn error(fields: &str) -> String {
        let value = json::parse(fields).unwrap();
        let fields = Fields::new(&value, "proxies.infi").unwrap();
        let keychain = Rc::new(FakeKeychain::default());
        match Secrets::new("infi", keychain, &fields) {
            Ok(secrets) => secrets
                .check(&SLOTS, &[], "proxies.infi")
                .unwrap_err()
                .to_string(),
            Err(error) => error.to_string(),
        }
    }

    #[test]
    fn tokens_live_in_the_keychain_under_the_proxy_name() {
        let keychain =
            Rc::new(FakeKeychain::holding(&[("infi/proxy-token", "pt-1")]));
        let secrets = secrets("{}", &keychain);
        assert_eq!(secrets.get("proxy-token").unwrap().expose(), "pt-1");
        assert!(secrets.has("proxy-token").unwrap());
        assert!(!secrets.has("session-token").unwrap());
        secrets
            .store("session-token", &Secret::new("st-2".into()))
            .unwrap();
        assert!(secrets.has("session-token").unwrap());
        assert_eq!(
            keychain.items.borrow().get("infi/session-token").unwrap(),
            "st-2"
        );
        let keys: Vec<String> =
            SLOTS.iter().map(|slot| slot.key.to_string()).collect();
        secrets.delete(&keys).unwrap();
        assert!(keychain.items.borrow().is_empty());
    }

    #[test]
    fn a_missing_token_says_how_to_set_it() {
        let keychain = Rc::new(FakeKeychain::default());
        let error = secrets("{}", &keychain).get("proxy-token").unwrap_err();
        assert!(
            error
                .to_string()
                .starts_with("no proxy-token for proxy infi in "),
            "{error}"
        );
        assert_eq!(error.hint(), Some("run 'hangar secret set infi'"));
    }

    #[test]
    fn a_token_from_a_command_is_its_first_line_and_never_missing() {
        let keychain = Rc::new(FakeKeychain::default());
        let secrets = secrets(
            r#"{"secrets": {"session-token":
                {"command": ["/bin/sh", "-c", "echo st-cmd"]}}}"#,
            &keychain,
        );
        assert_eq!(secrets.get("session-token").unwrap().expose(), "st-cmd");
        assert!(secrets.has("session-token").unwrap());
        assert!(secrets.is_command("session-token"));
        assert!(!secrets.is_command("proxy-token"));
        assert!(!secrets.has("proxy-token").unwrap());
        secrets.check(&SLOTS, &[], "proxies.infi").unwrap();
    }

    #[test]
    fn a_failing_or_odd_command_is_an_error() {
        let keychain = Rc::new(FakeKeychain::default());
        for (script, expected) in [
            (
                "echo locked >&2; exit 3",
                "session-token command failed: locked",
            ),
            ("echo 'two words'", "can't hold spaces"),
            ("true", "the token is empty"),
            ("printf '\\\\377'", "printed something that isn't UTF-8"),
        ] {
            let secrets = secrets(
                &format!(
                    r#"{{"secrets": {{"session-token":
                        {{"command": ["/bin/sh", "-c", "{script}"]}}}}}}"#
                ),
                &keychain,
            );
            let error = secrets.get("session-token").unwrap_err().to_string();
            assert!(error.contains(expected), "{script}: {error}");
        }
    }

    #[test]
    fn the_secrets_setting_is_checked() {
        assert_eq!(
            error(r#"{"secrets": {"nope": {"command": ["x"]}}}"#),
            "proxies.infi.secrets.nope: unknown token; known: proxy-token, \
             session-token"
        );
        assert_eq!(
            error(r#"{"secrets": {"proxy-token": {"command": []}}}"#),
            "proxies.infi.secrets.proxy-token.command: expected a command"
        );
        assert_eq!(
            error(r#"{"secrets": {"proxy-token": {"file": "x"}}}"#),
            "proxies.infi.secrets.proxy-token.file: unknown setting"
        );
        let keychain = Rc::new(FakeKeychain::default());
        let none =
            secrets(r#"{"secrets": {"x": {"command": ["y"]}}}"#, &keychain);
        assert_eq!(
            none.check(&[], &[], "proxies.p").unwrap_err().to_string(),
            "proxies.p.secrets.x: unknown token; known: none"
        );
    }

    #[test]
    fn a_token_that_needs_quoting_is_not_stored() {
        let keychain = Rc::new(FakeKeychain::default());
        let error = secrets("{}", &keychain)
            .store("proxy-token", &Secret::new("a b".into()))
            .unwrap_err()
            .to_string();
        assert!(error.starts_with("proxy infi: proxy-token: "), "{error}");
    }
}
