//! What `ps` and `inspect` show, read without changing anything.

use std::path::PathBuf;

use crate::bay::Bay;
use crate::hangar::Hangar;
use crate::lifecycle::Resource;
use crate::mounts::{self, MountStatus};
use crate::proxy::{Managed, Proxy, proxy_vm};
use crate::sandbox::data_volume;
use crate::vm_record::VmRecord;

pub(crate) struct Overview {
    pub(crate) bays: Vec<BayStatus>,
    pub(crate) proxies: Vec<ProxyStatus>,
    pub(crate) leftovers: Vec<(Resource, String)>,
}

pub(crate) struct BayStatus {
    pub(crate) name: String,
    pub(crate) state: String,
    pub(crate) proxy: Option<String>,
    pub(crate) image: String,
}

pub(crate) struct ProxyStatus {
    pub(crate) name: String,
    pub(crate) kind: &'static str,
    /// The VM's state and image; `None` for a proxy that runs elsewhere.
    pub(crate) state: Option<String>,
    pub(crate) image: Option<String>,
}

pub(crate) enum Detail {
    Bay(BayDetail),
    Proxy(ProxyDetail),
}

pub(crate) struct BayDetail {
    pub(crate) status: BayStatus,
    pub(crate) vm: String,
    pub(crate) mounts: Vec<MountStatus>,
    pub(crate) packages: Vec<String>,
    pub(crate) apps: Vec<String>,
    pub(crate) home: PathBuf,
    pub(crate) cache: PathBuf,
    pub(crate) volume: String,
}

pub(crate) struct ProxyDetail {
    pub(crate) status: ProxyStatus,
    pub(crate) vm: Option<String>,
    pub(crate) data: PathBuf,
    pub(crate) log: Option<PathBuf>,
}

pub(crate) fn collect(hangar: &Hangar) -> Overview {
    Overview {
        bays: hangar
            .bays()
            .iter()
            .map(|bay| bay_status(hangar, bay))
            .collect(),
        proxies: hangar
            .proxies
            .iter()
            .map(|(name, proxy)| proxy_status(hangar, name, proxy.as_ref()))
            .collect(),
        leftovers: hangar.leftovers(),
    }
}

pub(crate) fn bay_detail(hangar: &Hangar, bay: &Bay) -> Detail {
    let (home, cache) = bay.own_mounts();
    let recorded = VmRecord::load(&bay.dir.vm()).map(|record| record.mounts);
    let settings = bay.settings;
    Detail::Bay(BayDetail {
        status: bay_status(hangar, bay),
        vm: bay.vm.clone(),
        mounts: mounts::status(
            &settings.mounts,
            home.as_ref(),
            cache.as_ref(),
            &hangar.host_home,
            recorded.as_deref(),
        ),
        packages: settings.packages.clone(),
        apps: settings.apps.iter().map(|app| app.name.clone()).collect(),
        home: bay.dir.home(),
        cache: bay.cache.clone(),
        volume: data_volume(bay.name),
    })
}

pub(crate) fn proxy_detail(
    hangar: &Hangar,
    name: &str,
    proxy: &dyn Proxy,
) -> Detail {
    let managed = proxy.managed();
    Detail::Proxy(ProxyDetail {
        status: proxy_status(hangar, name, proxy),
        vm: managed.map(|_| proxy_vm(name)),
        data: hangar.state.proxy(name).data(),
        log: managed.map(Managed::log_file),
    })
}

fn bay_status(hangar: &Hangar, bay: &Bay) -> BayStatus {
    BayStatus {
        name: bay.name.to_string(),
        state: vm_state(hangar, &bay.vm),
        proxy: bay.settings.proxy.clone(),
        image: bay.settings.image_ref(),
    }
}

fn proxy_status(hangar: &Hangar, name: &str, proxy: &dyn Proxy) -> ProxyStatus {
    let managed = proxy.managed();
    ProxyStatus {
        name: name.to_string(),
        kind: proxy.kind(),
        state: managed.map(|_| vm_state(hangar, &proxy_vm(name))),
        image: managed.map(|managed| managed.vm().image),
    }
}

/// `unknown`, with a warning, when the sandbox can't tell.
fn vm_state(hangar: &Hangar, vm: &str) -> String {
    match hangar.sandbox.state(vm) {
        Ok(state) => state.as_str().to_string(),
        Err(error) => {
            log::warn!("{vm}: {error}");
            "unknown".to_string()
        }
    }
}
