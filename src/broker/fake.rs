//! An in-process broker for orchestration tests: state in memory, every
//! call recorded.

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::rc::Rc;

use super::{
    Access, Broker, BrokerHealth, ClientSecret, OAuthClient, OAuthLogin, PROXY,
    Policy, Route, StoredCredential, UiLogin,
};
use crate::error::{Result, bail};
use crate::sandbox::PublishedPort;
use crate::secret::Secret;

pub(crate) struct FakeBroker {
    /// `deny_unlisted`, `set_routes a,b`, `put KEY`, `delete A,B`,
    /// `access BAY`, `retain A,B`, …; never values. Shared, so a test keeps
    /// it after handing the broker to a `Hangar`.
    pub(crate) calls: Rc<RefCell<Vec<String>>>,
    pub(crate) keys: RefCell<BTreeSet<String>>,
    /// The OAuth state of keys in `keys`; the others are static.
    pub(crate) oauth: RefCell<BTreeMap<String, OAuthLogin>>,
    pub(crate) unlisted: Cell<Option<Policy>>,
    /// What `ensure_running` returns.
    pub(crate) created: Cell<bool>,
    pub(crate) fail_put: Cell<bool>,
    /// The port `access` names.
    pub(crate) access_port: Cell<u16>,
    /// The URL `oauth_connect` returns instead of a consent URL that
    /// names [`FAKE_CALLBACK`].
    pub(crate) consent: RefCell<Option<String>>,
    /// Whether the user finishes a login at once: `oauth_connect` stores
    /// new tokens.
    pub(crate) logs_in: Cell<bool>,
    /// What the last `oauth_connect` got.
    pub(crate) connected: Rc<RefCell<Option<(OAuthClient, &'static str)>>>,
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
            connected: Rc::default(),
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

    fn credentials(&self) -> Result<Vec<StoredCredential>> {
        self.record("credentials".into());
        let oauth = self.oauth.borrow();
        Ok(self
            .keys
            .borrow()
            .iter()
            .map(|key| StoredCredential {
                key: key.clone(),
                oauth: oauth.get(key).cloned(),
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

    fn oauth_connect(
        &self,
        key: &str,
        client: &OAuthClient,
        secret: &ClientSecret,
    ) -> Result<String> {
        self.record(format!("oauth_connect {key}"));
        let secret = match secret {
            ClientSecret::None => "none",
            ClientSecret::Keep => "keep",
            ClientSecret::New(_) => "new",
        };
        *self.connected.borrow_mut() = Some((client.clone(), secret));
        self.keys.borrow_mut().insert(key.to_string());
        let mut oauth = self.oauth.borrow_mut();
        let login = oauth.entry(key.to_string()).or_default();
        login.client = Some(client.clone());
        if self.logs_in.get() {
            let count = login.refreshed_at.as_deref().map_or(0, str::len);
            login.refreshed_at = Some("t".repeat(count + 1));
            login.connected = true;
        }
        let consent = format!(
            "https://auth.example/authorize?client_id={}&redirect_uri={}&state=s",
            client.client_id,
            FAKE_CALLBACK.replace(':', "%3A").replace('/', "%2F")
        );
        Ok(self.consent.borrow().clone().unwrap_or(consent))
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
