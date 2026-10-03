//! What hangar needs from a credential broker, whatever runs it: the
//! tower, the VM every bay's traffic leaves through, holding the real
//! credentials.
//! Each backend is one submodule, and only that module knows its CLI, API
//! and state files. The order of `up`, the checks on what a backend
//! claims and the credential reconcile live here and call only the trait.

mod agent_vault;
#[cfg(test)]
pub(crate) mod fake;

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::PathBuf;

use crate::config::{Source, managed_credentials};
use crate::error::{Context, Error, Result, bail};
use crate::hangar::Hangar;
use crate::json::{self, Json};
use crate::sandbox::{BoxState, PublishedPort, TOWER_VM};
use crate::secret::{Secret, trim_line_end};
use crate::state::write_private;
use crate::vm_record::VmRecord;
use log::warn;
use miniserde::json::Object;

pub(crate) use agent_vault::AgentVault;

/// The name of the port the bays' traffic goes to, in `ports()`, the
/// `tower-vm` record and `status`.
pub(crate) const PROXY: &str = "proxy";

/// A host the broker forwards to, and what it injects there.
#[derive(Debug, Clone)]
pub(crate) struct Route {
    pub(crate) name: String,
    pub(crate) host: String,
    pub(crate) auth: Auth,
    /// Backend-specific fields, passed through by backends that know them.
    pub(crate) extra: Object,
}

/// Credential names, never values.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Auth {
    Bearer {
        token: String,
    },
    Basic {
        username: String,
        password: Option<String>,
    },
    ApiKey {
        key: String,
        header: Option<String>,
        prefix: Option<String>,
    },
    /// Header name -> template with `{{ NAME }}` references.
    Custom {
        headers: BTreeMap<String, String>,
    },
    Passthrough,
}

impl Auth {
    pub(crate) fn credentials(&self) -> BTreeSet<String> {
        match self {
            Self::Bearer { token } => BTreeSet::from([token.clone()]),
            Self::Basic { username, password } => {
                [Some(username), password.as_ref()]
                    .into_iter()
                    .flatten()
                    .cloned()
                    .collect()
            }
            Self::ApiKey { key, .. } => BTreeSet::from([key.clone()]),
            Self::Custom { headers } => headers
                .values()
                .flat_map(|template| template_names(template))
                .collect(),
            Self::Passthrough => BTreeSet::new(),
        }
    }

    /// The config's form: `{"type": …, …}`.
    pub(crate) fn to_json(&self) -> Json {
        let mut auth = Object::new();
        let mut put = |key: &str, value: Option<&String>| {
            if let Some(value) = value {
                auth.insert(key.into(), json::string(value));
            }
        };
        let kind = match self {
            Self::Bearer { token } => {
                put("token", Some(token));
                "bearer"
            }
            Self::Basic { username, password } => {
                put("username", Some(username));
                put("password", password.as_ref());
                "basic"
            }
            Self::ApiKey {
                key,
                header,
                prefix,
            } => {
                put("key", Some(key));
                put("header", header.as_ref());
                put("prefix", prefix.as_ref());
                "api-key"
            }
            Self::Custom { headers } => {
                let headers = headers
                    .iter()
                    .map(|(name, value)| (name.clone(), json::string(value)))
                    .collect();
                auth.insert("headers".into(), Json::Object(headers));
                "custom"
            }
            Self::Passthrough => "passthrough",
        };
        auth.insert("type".into(), json::string(kind));
        Json::Object(auth)
    }
}

fn template_names(template: &str) -> Vec<String> {
    template
        .split("{{")
        .skip(1)
        .filter_map(|rest| rest.split_once("}}"))
        .map(|(name, _)| name.trim().to_string())
        .filter(|name| !name.is_empty())
        .collect()
}

pub(crate) struct Access {
    /// May embed the bay's token: it reaches the VM on stdin only.
    pub(crate) proxy_url: Secret,
    pub(crate) ca_pem: Vec<u8>,
    /// The one host port a bay may reach.
    pub(crate) host_port: u16,
    /// The bay's token was just replaced, so whatever runs in it still
    /// holds a dead one until restarted.
    pub(crate) renewed: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Policy {
    Deny,
    Allow,
}

impl Policy {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Deny => "deny",
            Self::Allow => "allow",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct BrokerHealth {
    pub(crate) reachable: bool,
    pub(crate) healthy: bool,
    /// What happens to hosts no route lists; `None` when unknown.
    pub(crate) unlisted: Option<Policy>,
}

/// The broker's admin login, for `hangar vault-ui`.
pub(crate) struct UiLogin {
    pub(crate) url: String,
    pub(crate) login: String,
    pub(crate) password: Secret,
    pub(crate) password_file: PathBuf,
}

pub(crate) trait Broker {
    /// Encrypted broker data, if any: the core never generates a new
    /// master password over it, and names the path in the error.
    fn has_data(&self) -> Option<PathBuf>;
    /// Rejects what this broker can't express.
    fn validate(&self, route: &Route) -> Result<()>;
    /// Creates or starts its VM, unlocks it with the master password (on
    /// stdin), waits until it's healthy and logs in as admin. True when it
    /// created the VM.
    fn ensure_running(&self, password: &Secret) -> Result<bool>;
    fn deny_unlisted(&self) -> Result<()>;
    /// Replaces the whole route set.
    fn set_routes(&self, routes: &[Route]) -> Result<()>;
    fn credential_keys(&self) -> Result<Vec<String>>;
    fn put_credential(&self, key: &str, value: &Secret) -> Result<()>;
    fn delete_credentials(&self, keys: &[String]) -> Result<()>;
    /// What bay `bay` needs; mints its token the first time.
    fn access(&self, bay: &str) -> Result<Access>;
    /// Deletes the tokens it minted for bays not in `bays`; tokens it
    /// didn't mint are never touched.
    fn retain_bays(&self, bays: &[String]) -> Result<()>;
    fn health(&self) -> BrokerHealth;
    /// What its VM publishes, from the config alone; one is named
    /// [`PROXY`].
    fn ports(&self) -> Vec<PublishedPort>;
    /// `None` until there is a login.
    fn ui(&self) -> Result<Option<UiLogin>>;
}

const RECREATE: &str = "run 'hangar destroy && hangar up' (home is kept)";

pub(crate) fn proxy_port(broker: &dyn Broker) -> Result<u16> {
    broker
        .ports()
        .iter()
        .find(|port| port.name == PROXY)
        .map(|port| port.host)
        .context("the broker publishes no proxy port")
}

fn wanted_record(broker: &dyn Broker) -> VmRecord {
    VmRecord {
        ports: broker
            .ports()
            .into_iter()
            .map(|port| (port.name, port.host, port.vm))
            .collect(),
        ..VmRecord::default()
    }
}

/// `up`, before any VM is touched: an existing tower must have been
/// created with today's proxy port, which every bay's egress is checked
/// against. Other port changes only warn.
pub(crate) fn preflight(hangar: &Hangar) -> Result<()> {
    if hangar.sandbox.state(TOWER_VM)? == BoxState::Missing {
        return Ok(());
    }
    let wanted = wanted_record(hangar.broker.as_ref());
    let Some(recorded) = VmRecord::load(&hangar.state.tower_vm()) else {
        return Err(Error::with_hint(
            "tower has no record of how it was created",
            RECREATE,
        ));
    };
    let proxy = |record: &VmRecord| {
        record
            .ports
            .iter()
            .find(|(name, ..)| name == PROXY)
            .map(|(_, host, _)| *host)
    };
    match proxy(&recorded) {
        Some(port) if Some(port) == proxy(&wanted) => {}
        recorded => {
            let what = recorded.map_or_else(
                || "has no record of its proxy port".to_string(),
                |port| format!("was created with proxy port {port}"),
            );
            return Err(Error::with_hint(format!("tower {what}"), RECREATE));
        }
    }
    if recorded.ports != wanted.ports {
        warn!(
            "{TOWER_VM} was created with different ports; run 'hangar \
             destroy && hangar up' to apply"
        );
    }
    Ok(())
}

/// Starts the tower; a newly created VM gets its `tower-vm` record.
/// The record follows the VM, not success: a VM that a failing start
/// still created (slow boot, failed login) is recorded too, so the next
/// `up` finishes it instead of refusing.
pub(crate) fn start(hangar: &Hangar, password: &Secret) -> Result<()> {
    let was_missing = hangar.sandbox.state(TOWER_VM)? == BoxState::Missing;
    let started = hangar.broker.ensure_running(password);
    let created = match started {
        Ok(created) => created,
        Err(_) => {
            was_missing && hangar.sandbox.state(TOWER_VM)? != BoxState::Missing
        }
    };
    if created {
        let record = wanted_record(hangar.broker.as_ref());
        write_private(&hangar.state.tower_vm(), record.render().as_bytes())?;
    }
    started.map(|_| ())
}

/// Checked, not trusted: routes and credentials only follow a broker
/// that says it now refuses unlisted hosts.
pub(crate) fn deny_unlisted(hangar: &Hangar) -> Result<()> {
    hangar.broker.deny_unlisted()?;
    if hangar.broker.health().unlisted != Some(Policy::Deny) {
        bail!("broker does not deny unlisted hosts; refusing to apply routes");
    }
    Ok(())
}

pub(crate) fn apply_routes(hangar: &Hangar) -> Result<()> {
    let routes: Vec<Route> = hangar.settings.routes.values().cloned().collect();
    hangar.broker.set_routes(&routes)
}

/// Makes the broker hold exactly the configured credentials. Only keys
/// hangar set itself are ever deleted, so credentials added by hand (e.g.
/// an OAuth login) survive.
pub(crate) fn apply_credentials(hangar: &Hangar) -> Result<()> {
    let wanted = read_values(&managed_credentials(&hangar.settings))?;
    let managed = hangar.state.credential_keys();
    let previous = fs::read_to_string(&managed).unwrap_or_default();
    let stale = stale_keys(&previous, &wanted);
    if !stale.is_empty() {
        hangar
            .broker
            .delete_credentials(&stale)
            .context("deleting credentials no longer in the config")?;
    }
    // Recorded before posting: a failed post must not leave earlier keys
    // unrecorded, or removing them from the config would never delete them.
    let mut keys = wanted.keys().cloned().collect::<Vec<_>>().join("\n");
    keys.push('\n');
    write_private(&managed, keys.as_bytes())?;
    for (key, value) in &wanted {
        hangar.broker.put_credential(key, value)?;
    }
    Ok(())
}

/// The broker's access for `bay`, checked against the port bays are
/// created to reach.
pub(crate) fn access(hangar: &Hangar, bay: &str) -> Result<Access> {
    let access = hangar.broker.access(bay)?;
    let proxy = proxy_port(hangar.broker.as_ref())?;
    if access.host_port != proxy {
        bail!(
            "broker gave port {} for bay {bay}, but its proxy port is \
             {proxy}",
            access.host_port
        );
    }
    Ok(access)
}

pub(crate) fn retain_bays(hangar: &Hangar) -> Result<()> {
    let bays: Vec<String> = hangar
        .bays()
        .into_iter()
        .map(|bay| bay.name.to_string())
        .collect();
    hangar.broker.retain_bays(&bays)
}

/// The value of each managed credential: file contents (one trailing
/// newline dropped) or the app's fixed value.
fn read_values(
    managed: &BTreeMap<String, Source>,
) -> Result<BTreeMap<String, Secret>> {
    managed
        .iter()
        .map(|(key, source)| {
            let value = match source {
                Source::App { value, .. } => value.clone(),
                Source::File(file) => {
                    let Ok(value) = fs::read_to_string(file) else {
                        bail!("credential file not readable: {file} ({key})");
                    };
                    trim_line_end(&value).to_string()
                }
            };
            Ok((key.clone(), Secret::new(value)))
        })
        .collect()
}

/// Keys hangar set before (one per line) that the config no longer wants.
/// Keys hangar never set aren't in `previous`, so they're never deleted.
fn stale_keys<V>(previous: &str, wanted: &BTreeMap<String, V>) -> Vec<String> {
    let previous: BTreeSet<&str> = previous.lines().collect();
    previous
        .into_iter()
        .filter(|key| !key.is_empty() && !wanted.contains_key(*key))
        .map(ToString::to_string)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::fake::FakeBroker;
    use super::{
        Auth, apply_credentials, preflight, read_values, stale_keys, start,
    };
    use crate::config::Source;
    use crate::hangar::Hangar;
    use crate::sandbox::fake::FakeSandbox;
    use crate::sandbox::{BoxState, TOWER_VM};
    use crate::secret::Secret;
    use crate::testing::{hangar_with_broker, scratch_dir};
    use std::collections::BTreeMap;
    use std::fs;
    use std::path::Path;
    use std::rc::Rc;

    const RECREATE: &str = ": run 'hangar destroy && hangar up' (home is kept)";

    #[test]
    fn only_keys_hangar_set_and_no_longer_wants_are_stale() {
        let wanted = BTreeMap::from([("GITHUB_TOKEN".to_string(), "value")]);
        let previous = "GITHUB_TOKEN\nJIRA_TOKEN\n\nJIRA_TOKEN\n";
        assert_eq!(stale_keys(previous, &wanted), ["JIRA_TOKEN"]);
        assert_eq!(stale_keys("", &wanted), Vec::<String>::new());
    }

    #[test]
    fn credential_values_come_from_files_or_are_fixed() {
        let file = scratch_dir("credential-values").join("token");
        fs::write(&file, "secret\r\n").unwrap();
        let managed = BTreeMap::from([
            ("A".to_string(), Source::File(file.display().to_string())),
            (
                "B".to_string(),
                Source::App {
                    app: "a".into(),
                    value: "fixed".into(),
                },
            ),
        ]);
        let values = read_values(&managed).unwrap();
        assert_eq!(values["A"].expose(), "secret");
        assert_eq!(values["B"].expose(), "fixed");

        let missing = BTreeMap::from([(
            "C".to_string(),
            Source::File("/nonexistent".into()),
        )]);
        let error = read_values(&missing).unwrap_err().to_string();
        assert_eq!(error, "credential file not readable: /nonexistent (C)");
    }

    #[test]
    fn auth_names_its_credentials_and_renders_the_config_form() {
        let custom = Auth::Custom {
            headers: BTreeMap::from([(
                "Authorization".to_string(),
                "Basic {{ USER }}:{{PASS}} {{ }}".to_string(),
            )]),
        };
        let basic = Auth::Basic {
            username: "U".into(),
            password: None,
        };
        let key = Auth::ApiKey {
            key: "K".into(),
            header: Some("X-Key".into()),
            prefix: None,
        };
        let names =
            |auth: &Auth| auth.credentials().into_iter().collect::<Vec<_>>();
        assert_eq!(names(&custom), ["PASS", "USER"]);
        assert_eq!(names(&basic), ["U"]);
        assert_eq!(names(&Auth::Passthrough), Vec::<String>::new());
        let json = |auth: &Auth| crate::json::stringify(&auth.to_json());
        assert_eq!(json(&basic), r#"{"type":"basic","username":"U"}"#);
        assert_eq!(
            json(&key),
            r#"{"header":"X-Key","key":"K","type":"api-key"}"#
        );
        assert_eq!(
            json(&custom),
            r#"{"headers":{"Authorization":"Basic {{ USER }}:{{PASS}} {{ }}"},"type":"custom"}"#
        );
    }

    fn broker_hangar(
        state: &Path,
        config: &str,
        vms: &[(&str, BoxState)],
        broker: FakeBroker,
    ) -> (Hangar, Rc<FakeSandbox>) {
        let sandbox = Rc::new(FakeSandbox::with(vms));
        let hangar = hangar_with_broker(
            config,
            state,
            sandbox.clone(),
            Box::new(broker),
        );
        (hangar, sandbox)
    }

    #[test]
    fn only_managed_keys_are_deleted_and_the_record_comes_first() {
        let state = scratch_dir("broker-credentials");
        let token = state.join("token");
        fs::write(&token, "value\n").unwrap();
        fs::write(state.join("credential-keys"), "OLD\nKEPT\n").unwrap();
        let config = format!(
            r#"{{"tower": {{"credentialFiles": {{"KEPT": "{}"}}}}}}"#,
            token.display()
        );
        let broker = FakeBroker::default();
        broker.fail_put.set(true);
        let calls = broker.calls.clone();
        let (hangar, _) = broker_hangar(&state, &config, &[], broker);
        assert!(apply_credentials(&hangar).is_err());
        assert_eq!(*calls.borrow(), ["delete OLD", "put KEPT"]);
        assert_eq!(
            fs::read_to_string(state.join("credential-keys")).unwrap(),
            "KEPT\n"
        );
    }

    #[test]
    fn a_tower_vm_is_recorded_only_when_it_is_created() {
        let state = scratch_dir("broker-record");
        let (hangar, _) =
            broker_hangar(&state, "{}", &[], FakeBroker::default());
        preflight(&hangar).unwrap();
        start(&hangar, &Secret::new("pw".into())).unwrap();
        let record = state.join("tower-vm");
        assert_eq!(
            fs::read_to_string(&record).unwrap(),
            "port\tvault-ui\t14321\t14321\nport\tproxy\t14322\t14322\n"
        );

        fs::remove_file(&record).unwrap();
        let broker = FakeBroker::default();
        broker.created.set(false);
        let (hangar, _) = broker_hangar(&state, "{}", &[], broker);
        start(&hangar, &Secret::new("pw".into())).unwrap();
        assert!(!record.exists());
    }

    #[test]
    fn an_existing_tower_vm_without_a_matching_record_is_refused() {
        let state = scratch_dir("broker-refused");
        let running = [(TOWER_VM, BoxState::Running)];
        let error = |record: Option<&str>| {
            let _ = fs::remove_file(state.join("tower-vm"));
            if let Some(record) = record {
                fs::write(state.join("tower-vm"), record).unwrap();
            }
            let (hangar, sandbox) =
                broker_hangar(&state, "{}", &running, FakeBroker::default());
            let error = preflight(&hangar).unwrap_err().to_string();
            assert_eq!(sandbox.changes(), Vec::<String>::new());
            error
        };
        assert_eq!(
            error(None),
            format!("tower has no record of how it was created{RECREATE}")
        );
        assert_eq!(
            error(Some("port\tproxy\t15000\t14322\n")),
            format!("tower was created with proxy port 15000{RECREATE}")
        );
        assert_eq!(
            error(Some("port\tvault-ui\t14321\t14321\n")),
            format!("tower has no record of its proxy port{RECREATE}")
        );

        // Another port only warns.
        fs::write(
            state.join("tower-vm"),
            "port\tvault-ui\t9999\t14321\nport\tproxy\t14322\t14322\n",
        )
        .unwrap();
        let (hangar, _) =
            broker_hangar(&state, "{}", &running, FakeBroker::default());
        preflight(&hangar).unwrap();
    }
}
