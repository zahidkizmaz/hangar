//! What `up`, `down` and `destroy` act on, and how `up` reconciles an
//! existing VM with the one wanted.

use crate::error::Error;
use crate::sandbox::BoxState;
use crate::vm_record::VmRecord;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Resource {
    Bay,
    Proxy,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Plan {
    Create,
    Start,
    Recreate(Vec<&'static str>),
    Keep,
}

#[derive(Debug)]
pub(crate) enum Outcome {
    Created,
    Recreated(Vec<String>),
    Updated(Vec<String>),
    Unchanged,
    Stopped,
    Removed,
    Failed(Error),
}

#[derive(Debug)]
pub(crate) struct Change {
    pub(crate) name: String,
    pub(crate) resource: Resource,
    pub(crate) outcome: Outcome,
}

pub(crate) fn plan(
    state: BoxState,
    recorded: Option<&VmRecord>,
    wanted: &VmRecord,
) -> Plan {
    if state == BoxState::Missing {
        return Plan::Create;
    }
    let Some(recorded) = recorded else {
        return Plan::Recreate(vec!["no record"]);
    };
    let changes = vm_changes(recorded, wanted);
    match state {
        _ if !changes.is_empty() => Plan::Recreate(changes),
        BoxState::Stopped => Plan::Start,
        _ => Plan::Keep,
    }
}

pub(crate) fn vm_changes(
    recorded: &VmRecord,
    wanted: &VmRecord,
) -> Vec<&'static str> {
    [
        ("image", recorded.image != wanted.image),
        ("proxy", recorded.egress != wanted.egress),
        ("mounts", recorded.mounts != wanted.mounts),
        ("ports", recorded.ports != wanted.ports),
        ("resources", recorded.resources != wanted.resources),
        ("fingerprint", recorded.fingerprint != wanted.fingerprint),
    ]
    .into_iter()
    .filter_map(|(reason, changed)| changed.then_some(reason))
    .collect()
}

pub(crate) fn resources(cpus: u32, memory: &str, disk: Option<&str>) -> String {
    match disk {
        Some(disk) => format!("cpus={cpus} memory={memory} disk={disk}"),
        None => format!("cpus={cpus} memory={memory}"),
    }
}

pub(crate) fn disk_changed(recorded: &VmRecord, wanted: &VmRecord) -> bool {
    disk(recorded) != disk(wanted)
}

fn disk(record: &VmRecord) -> Option<&str> {
    record
        .resources
        .as_deref()?
        .split(' ')
        .find_map(|field| field.strip_prefix("disk="))
}

#[cfg(test)]
mod tests {
    use super::{Plan, disk_changed, plan, resources, vm_changes};
    use crate::sandbox::BoxState;
    use crate::vm_record::VmRecord;

    type Edit = fn(&mut VmRecord);

    fn record() -> VmRecord {
        VmRecord {
            image: Some("example/agent:1".into()),
            egress: Some("host:14322".into()),
            resources: Some(resources(4, "6G", Some("40G"))),
            fingerprint: Some("abc".into()),
            ports: vec![("web".into(), 3100, 3100)],
            mounts: vec!["/home/pilot\t/h\trw".into()],
        }
    }

    #[test]
    fn missing_creates() {
        let wanted = record();
        assert_eq!(
            plan(BoxState::Missing, Some(&wanted), &wanted),
            Plan::Create
        );
        assert_eq!(plan(BoxState::Missing, None, &wanted), Plan::Create);
    }

    #[test]
    fn stopped_and_matching_starts() {
        let wanted = record();
        assert_eq!(
            plan(BoxState::Stopped, Some(&wanted), &wanted),
            Plan::Start
        );
    }

    #[test]
    fn running_and_matching_keeps() {
        let wanted = record();
        assert_eq!(plan(BoxState::Running, Some(&wanted), &wanted), Plan::Keep);
    }

    #[test]
    fn running_without_record_recreates() {
        assert_eq!(
            plan(BoxState::Running, None, &record()),
            Plan::Recreate(vec!["no record"])
        );
    }

    #[test]
    fn each_vm_level_field_forces_recreate_with_its_reason() {
        let changes: [(&str, Edit); 6] = [
            ("image", |r| r.image = Some("other".into())),
            ("proxy", |r| r.egress = Some("open".into())),
            ("mounts", |r| r.mounts.clear()),
            ("ports", |r| r.ports.clear()),
            ("resources", |r| r.resources = None),
            ("fingerprint", |r| r.fingerprint = None),
        ];
        for (reason, change) in changes {
            let wanted = record();
            let mut recorded = record();
            change(&mut recorded);
            assert_eq!(vm_changes(&recorded, &wanted), vec![reason]);
            assert_eq!(
                plan(BoxState::Running, Some(&recorded), &wanted),
                Plan::Recreate(vec![reason])
            );
        }
    }

    #[test]
    fn several_changes_list_every_reason_in_order() {
        let wanted = record();
        let recorded = VmRecord::default();
        assert_eq!(
            vm_changes(&recorded, &wanted),
            vec![
                "image",
                "proxy",
                "mounts",
                "ports",
                "resources",
                "fingerprint"
            ]
        );
    }

    #[test]
    fn stopped_with_changes_recreates_too() {
        let wanted = record();
        let mut recorded = record();
        recorded.image = None;
        assert_eq!(
            plan(BoxState::Stopped, Some(&recorded), &wanted),
            Plan::Recreate(vec!["image"])
        );
    }

    #[test]
    fn resources_render_with_and_without_disk() {
        assert_eq!(
            resources(4, "6G", Some("40G")),
            "cpus=4 memory=6G disk=40G"
        );
        assert_eq!(resources(2, "1G", None), "cpus=2 memory=1G");
    }

    #[test]
    fn disk_change_is_detected_inside_resources() {
        let mut wanted = record();
        wanted.resources = Some(resources(4, "6G", Some("60G")));
        assert!(disk_changed(&record(), &wanted));

        wanted.resources = Some(resources(8, "6G", Some("40G")));
        assert!(!disk_changed(&record(), &wanted));
    }

    #[test]
    fn env_packages_and_copies_never_force_recreate() {
        assert!(!record().render().contains("env"));
    }
}
