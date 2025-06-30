use std::{collections::HashMap, ffi::OsString};

use bon::builder;
use color_eyre::eyre::eyre;
use ego_tree::{NodeId, NodeRef, Tree};
use sysinfo::{Pid, Process};

#[derive(Debug, Clone)]
pub struct ProcessData {
    pub name: OsString,
    pub pid: Pid,
    pub cpu_usage: f32,
    pub memory_usage: u64,
}

impl ProcessData {
    pub fn from_process(value: &Process, cpu_count: usize) -> Self {
        let name = value.name().to_os_string();
        let cpu_usage = value.cpu_usage() as f64 / (cpu_count as f64);
        let cpu_usage = cpu_usage as f32;
        let memory_usage = value.memory();
        let pid = value.pid();
        ProcessData {
            pid,
            name,
            cpu_usage,
            memory_usage,
        }
    }
}

#[derive(Debug)]
pub enum ProcessNode {
    Root,
    Process(ProcessData),
}

#[derive(Debug)]
pub struct ProcessTree(pub Tree<ProcessNode>);

#[bon::bon]
impl ProcessTree {
    #[builder]
    pub fn try_new(proc: &HashMap<Pid, Process>, cpu_count: usize) -> color_eyre::Result<Self> {
        let mut stack = vec![];
        stack.reserve(proc.len());

        let mut tree = Tree::new(ProcessNode::Root);
        let mut tree_map = HashMap::<Pid, NodeId>::default();
        proc.iter()
            // .filter(|&(_, process)| process.parent().is_none())
            .map(|(pid, process)| (pid, ProcessData::from_process(process, cpu_count)))
            .for_each(|(&pid, data)| {
                let node = tree.root_mut().append(ProcessNode::Process(data)).id();
                tree_map.insert(pid, node);
                stack.push(pid);
            });

        while let Some(value) = stack.pop() {
            stack.extend(
                proc.iter()
                    .filter(|&(_, proc)| proc.parent() == Some(value))
                    .map(|(pid, _)| *pid),
            );
            let process = proc
                .get(&value)
                .ok_or(eyre!("created pid out of thin air. i am magical"))
                .unwrap();
            let Some(parent_pid) = process.parent() else {
                continue;
            };
            let parent_id = tree_map
                .get(&parent_pid)
                .ok_or(eyre!("pid not in tree map"))?;
            let process_data = ProcessData::from_process(process, cpu_count);
            tree.get_mut(*parent_id)
                .ok_or(eyre!("parent not in tree"))?
                .append(ProcessNode::Process(process_data));
        }

        Ok(ProcessTree(tree))
    }
}

pub trait SkipPlaceholderRoot<'a> {
    fn skip_placeholder_root(self) -> impl Iterator<Item = &'a ProcessData>;
}

impl<'a, I> SkipPlaceholderRoot<'a> for I
where
    I: Iterator<Item = NodeRef<'a, ProcessNode>>,
{
    fn skip_placeholder_root(self) -> impl Iterator<Item = &'a ProcessData> {
        self.flat_map(|node| match node.value() {
            ProcessNode::Root => None,
            ProcessNode::Process(proc) => Some(proc),
        })
    }
}
