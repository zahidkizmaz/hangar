//! An in-process broker for orchestration tests: state in memory, every
//! call recorded.

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Duration;

use super::{
    Access, Broker, BrokerHealth, Credential, CredentialKind, OAuthClient,
    OAuthState, PROXY, PendingLogin, Policy, Route, UiLogin, login_timed_out,
};
use crate::error::{Result, bail};
use crate::sandbox::PublishedPort;
use crate::secret::Secret;
use crate::url::with_param;

pub(crate) struct FakeBroker {
    /// `deny_unlisted`, `set_routes a,b`, `put KEY`, `delete A,B`,
    /// `access BAY`, `retain A,B`, …; never values. Shared, so a test keeps
    /// it after handing the broker to a `Hangar`.
    pub(crate) calls: Rc<RefCell<Vec<String>>>,
    pub(crate) keys: RefCell<BTreeSet<String>>,
    /// The OAuth state of keys in `keys`; the others are static.
    pub(crate) oauth: RefCell<BTreeMap<String, OAuthState>>,
    pub(crate) unlisted: Cell<Option<Policy>>,
    /// What `ensure_running` returns.
    pub(crate) created: Cell<bool>,
    pub(crate) fail_put: Cell<bool>,
    /// The port `access` names.
    pub(crate) access_port: Cell<u16>,
    /// The URL `oauth_begin` returns instead of a consent URL that names
    /// [`FAKE_CALLBACK`].
    pub(crate) consent: RefCell<Option<String>>,
    /// Whether `oauth_wait` sees the user log in, or times out.
    pub(crate) logs_in: Cell<bool>,
}

pub(crate) const FAKE_CALLBACK: &str =
    "http://127.0.0.1:14321/v1/oauth/callback";

impl Default for FakeBroker {
    fn default() -> Self {
        Self {
            calls: Rc::default(),
            keys: RefCell::default(),
            oauth: RefCell::default(),
            unlisted: Cell::new(Some(Policy::Allow)),
            created: Cell::new(true),
            fail_put: Cell::new(false),
            access_port: Cell::new(14322),
            consent: RefCell::default(),
            logs_in: Cell::new(true),
        }
    }
}

impl FakeBroker {
    fn record(&self, call: String) {
        self.calls.borrow_mut().push(call);
    }
}

impl Broker for FakeBroker {
    fn has_data(&self) -> Option<PathBuf> {
        None
    }

    fn validate(&self, _route: &Route) -> Result<()> {
        Ok(())
    }

    fn ensure_running(&self, _password: &Secret) -> Result<bool> {
        self.record("ensure_running".into());
        Ok(self.created.get())
    }

    fn deny_unlisted(&self) -> Result<()> {
        self.record("deny_unlisted".into());
        self.unlisted.set(Some(Policy::Deny));
        Ok(())
    }

    fn set_routes(&self, routes: &[Route]) -> Result<()> {
        let names: Vec<&str> =
            routes.iter().map(|route| route.name.as_str()).collect();
        self.record(format!("set_routes {}", names.join(",")));
        Ok(())
    }

    fn credentials(&self) -> Result<Vec<Credential>> {
        self.record("credentials".into());
        let oauth = self.oauth.borrow();
        Ok(self
            .keys
            .borrow()
            .iter()
            .map(|key| Credential {
                key: key.clone(),
                kind: oauth
                    .get(key)
                    .cloned()
                    .map_or(CredentialKind::Static, CredentialKind::OAuth),
            })
            .collect())
    }

    fn put_credential(&self, key: &str, _value: &Secret) -> Result<()> {
        self.record(format!("put {key}"));
        if self.fail_put.get() {
            bail!("credential {key}: refused");
        }
        self.keys.borrow_mut().insert(key.to_string());
        Ok(())
    }

    fn delete_credentials(&self, keys: &[String]) -> Result<()> {
        self.record(format!("delete {}", keys.join(",")));
        let mut held = self.keys.borrow_mut();
        for key in keys {
            held.remove(key);
        }
        Ok(())
    }

    fn oauth_redirect_uri(&self) -> Result<String> {
        Ok(FAKE_CALLBACK.into())
    }

    fn oauth_begin(
        &self,
        key: &str,
        client: Option<&OAuthClient>,
    ) -> Result<PendingLogin> {
        let client = client.map_or("stored", |client| &client.client_id);
        self.record(format!("oauth_begin {key} {client}"));
        let mut oauth = self.oauth.borrow_mut();
        if client == "stored" && !oauth.contains_key(key) {
            bail!("{key} has no OAuth client to log in with");
        }
        self.keys.borrow_mut().insert(key.to_string());
        oauth
            .entry(key.to_string())
            .or_insert(OAuthState::NotConnected);
        let authorize = "https://auth.example/authorize";
        let url =
            with_param(authorize, "redirect_uri", FAKE_CALLBACK) + "&state=s";
        Ok(PendingLogin {
            key: key.to_string(),
            url: self.consent.borrow().clone().unwrap_or(url),
            marker: String::new(),
        })
    }

    fn oauth_wait(
        &self,
        login: &PendingLogin,
        _timeout: Duration,
    ) -> Result<()> {
        self.record(format!("oauth_wait {}", login.key));
        if !self.logs_in.get() {
            return Err(login_timed_out(&login.key));
        }
        self.oauth
            .borrow_mut()
            .insert(login.key.clone(), OAuthState::Connected);
        Ok(())
    }

    fn retain_bays(&self, bays: &[String]) -> Result<()> {
        self.record(format!("retain {}", bays.join(",")));
        Ok(())
    }

    fn access(&self, bay: &str) -> Result<Access> {
        self.record(format!("access {bay}"));
        Ok(Access {
            proxy_url: Secret::new(
                "http://fake-token:vault@host.fake:14322".into(),
            ),
            ca_pem: b"FAKE-CA\n".to_vec(),
            host_port: self.access_port.get(),
            renewed: false,
        })
    }

    fn health(&self) -> BrokerHealth {
        BrokerHealth {
            reachable: true,
            healthy: true,
            unlisted: self.unlisted.get(),
        }
    }

    fn ports(&self) -> Vec<PublishedPort> {
        let port = |name: &str, number, http| PublishedPort {
            app: None,
            bay: None,
            name: name.into(),
            host: number,
            vm: number,
            purpose: format!("{name} purpose"),
            http,
        };
        vec![port("vault-ui", 14321, true), port(PROXY, 14322, false)]
    }

    fn ui(&self) -> Result<Option<UiLogin>> {
        Ok(None)
    }
}
