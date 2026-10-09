//! An in-process broker for orchestration tests: state in memory, every
//! call recorded.

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::rc::Rc;

use super::{
    Access, Broker, BrokerHealth, OAuthLogin, PROXY, Policy, Route,
    StoredCredential, UiLogin,
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
}

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
