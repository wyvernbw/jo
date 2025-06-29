use std::ffi::OsString;

use sysinfo::Process;

#[derive(Debug)]
pub struct ProcessData {
    pub name: OsString,
    pub cpu_usage: f32,
}

impl ProcessData {
    pub fn from_process(value: &Process, cpu_count: u16) -> Self {
        let name = value.name().to_os_string();
        let cpu_usage = value.cpu_usage() / f32::from(cpu_count);
        ProcessData { name, cpu_usage }
    }
}
