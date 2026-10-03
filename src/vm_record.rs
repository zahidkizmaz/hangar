//! What a VM was created with, as `key\tvalue` lines in `stateDir`
//! (`bays/<name>/vm`, `tower-vm`): its image, egress, ports and mounts are
//! fixed at create, so later `up`s compare against this instead of asking
//! the sandbox.

use std::fmt::Write as _;
use std::fs;
use std::path::Path;

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct VmRecord {
    pub(crate) image: Option<String>,
    /// The one host port the VM may reach.
    pub(crate) egress: Option<u16>,
    /// `(name, host, vm)`.
    pub(crate) ports: Vec<(String, u16, u16)>,
    /// `vm\thost\tro|rw`, as `mounts::lines` writes them; none means the
    /// VM was created without mounts.
    pub(crate) mounts: Vec<String>,
}

impl VmRecord {
    pub(crate) fn load(path: &Path) -> Option<Self> {
        fs::read_to_string(path).ok().map(|text| Self::parse(&text))
    }

    /// Lines it doesn't understand are skipped: a missing value counts as
    /// unknown, never as a match.
    fn parse(text: &str) -> Self {
        let mut record = Self::default();
        for (key, value) in
            text.lines().filter_map(|line| line.split_once('\t'))
        {
            match key {
                "image" => record.image = Some(value.to_string()),
                "egress" => record.egress = value.parse().ok(),
                "port" => record.ports.extend(port(value)),
                "mount" => record.mounts.push(value.to_string()),
                _ => {}
            }
        }
        record
    }

    pub(crate) fn render(&self) -> String {
        let mut text = String::new();
        if let Some(image) = &self.image {
            let _ = writeln!(text, "image\t{image}");
        }
        if let Some(port) = self.egress {
            let _ = writeln!(text, "egress\t{port}");
        }
        for (name, host, vm) in &self.ports {
            let _ = writeln!(text, "port\t{name}\t{host}\t{vm}");
        }
        for mount in &self.mounts {
            let _ = writeln!(text, "mount\t{mount}");
        }
        text
    }
}

fn port(value: &str) -> Option<(String, u16, u16)> {
    let mut fields = value.split('\t');
    let name = fields.next()?.to_string();
    let host = fields.next()?.parse().ok()?;
    let vm = fields.next()?.parse().ok()?;
    Some((name, host, vm))
}

#[cfg(test)]
mod tests {
    use super::VmRecord;
    use crate::testing::scratch_dir;

    fn record() -> VmRecord {
        VmRecord {
            image: Some("example/agent:1".into()),
            egress: Some(14322),
            ports: vec![("paperclip".into(), 3100, 3100)],
            mounts: vec!["/home/pilot\t/home/you/hangar/home\trw".into()],
        }
    }

    #[test]
    fn a_record_reads_back_what_was_written() {
        let text = record().render();
        assert_eq!(
            text,
            "image\texample/agent:1\negress\t14322\n\
             port\tpaperclip\t3100\t3100\n\
             mount\t/home/pilot\t/home/you/hangar/home\trw\n"
        );
        assert_eq!(VmRecord::parse(&text), record());
        assert_eq!(VmRecord::parse(""), VmRecord::default());
    }

    #[test]
    fn unreadable_values_are_unknown_not_a_match() {
        let parsed = VmRecord::parse(
            "egress\tlots\nport\tx\t1\nport\ty\t2\tz\nnew\tkey\nnoise\n",
        );
        assert_eq!(parsed, VmRecord::default());
    }

    #[test]
    fn only_an_existing_file_is_a_record() {
        let dir = scratch_dir("vm-record");
        assert_eq!(VmRecord::load(&dir.join("vm")), None);
        std::fs::write(dir.join("vm"), record().render()).unwrap();
        assert_eq!(VmRecord::load(&dir.join("vm")), Some(record()));
    }
}
