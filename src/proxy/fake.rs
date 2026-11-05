//! A kind for tests: every answer is a field, nothing runs.

use std::cell::Cell;
use std::path::PathBuf;

use super::{Health, Login, Managed, ManagedVm, Proxy, Spec};
use crate::error::Result;
use crate::sandbox::{Egress, Sandbox};
use crate::secret::Secret;

pub(crate) struct FakeProxy {
    pub(crate) egress: Egress,
    pub(crate) ca: Option<Vec<u8>>,
    pub(crate) login: Option<(String, String)>,
    pub(crate) up: bool,
    pub(crate) managed: Option<FakeManaged>,
}

pub(crate) struct FakeManaged {
    pub(crate) vm: ManagedVm,
    pub(crate) starts: Cell<u32>,
}

impl FakeProxy {
    pub(crate) fn at(port: u16) -> Self {
        Self {
            egress: Egress::HostPort(port),
            ca: None,
            login: None,
            up: true,
            managed: None,
        }
    }
}

impl FakeManaged {
    pub(crate) fn new(port: u16) -> Self {
        let vm = ManagedVm {
            image: "fake:1".into(),
            cpus: 1,
            memory: "256M".into(),
            publish: (port, port),
            mounts: Vec::new(),
            fingerprint: "fake".into(),
            entrypoint: None,
        };
        Self {
            vm,
            starts: Cell::new(0),
        }
    }
}

impl Proxy for FakeProxy {
    fn kind(&self) -> &'static str {
        "fake"
    }

    fn egress(&self) -> Egress {
        self.egress
    }

    fn ca(&self) -> Result<Option<Vec<u8>>> {
        Ok(self.ca.clone())
    }

    fn login(&self, _bay: &str) -> Result<Option<Login>> {
        Ok(self.login.as_ref().map(|(user, password)| Login {
            user: Secret::new(user.clone()),
            password: Secret::new(password.clone()),
        }))
    }

    fn health(&self) -> Health {
        if self.up {
            Health::Up
        } else {
            Health::Down("down".into())
        }
    }

    fn managed(&self) -> Option<&dyn Managed> {
        self.managed.as_ref().map(|managed| managed as &dyn Managed)
    }
}

impl Managed for FakeManaged {
    fn vm(&self) -> ManagedVm {
        self.vm.clone()
    }

    fn start(&self, _sandbox: &dyn Sandbox, _vm: &str) -> Result<()> {
        self.starts.set(self.starts.get() + 1);
        Ok(())
    }

    fn log_file(&self) -> PathBuf {
        PathBuf::from("fake.log")
    }
}

/// Reads `port` (default 3128), `ca` (PEM text), `login` (`user:password`)
/// and `managed` (a flag).
pub(crate) fn build(spec: &Spec) -> Result<Box<dyn Proxy>> {
    let fields = &spec.fields;
    super::only(fields, &["port", "ca", "login", "managed"])?;
    let port = if fields.map.contains_key("port") {
        fields.number("port")?
    } else {
        3128
    };
    let login = fields.optional_string("login")?.and_then(|text| {
        let (user, password) = text.split_once(':')?;
        Some((user.to_string(), password.to_string()))
    });
    Ok(Box::new(FakeProxy {
        ca: fields.optional_string("ca")?.map(String::into_bytes),
        login,
        managed: fields
            .flag_or("managed", false)?
            .then(|| FakeManaged::new(port)),
        ..FakeProxy::at(port)
    }))
}
