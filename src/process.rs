use std::ffi::OsString;

use sysinfo::Process;

#[derive(Debug, Default)]
pub struct ProcessData {
    pub name: OsString,
}

impl From<&Process> for ProcessData {
    fn from(value: &Process) -> Self {
        let name = value.name().to_os_string();
        ProcessData { name }
    }
}
