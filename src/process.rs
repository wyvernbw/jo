use std::{cmp::Ordering, collections::HashMap, ffi::OsString, sync::Arc};

use color_eyre::eyre::eyre;
use ego_tree::{NodeId, NodeRef, Tree};
use sysinfo::{Pid, Process, System, Users};

#[derive(Debug, Clone)]
pub struct ProcessData {
    pub user: Arc<str>,
    pub name: Arc<str>,
    pub pid: Pid,
    pub cpu_usage: f16,
    pub memory_percent: f16,
    pub memory_usage: u64,
}

impl ProcessData {
    pub fn from_process(value: &Process, system: &System, users: &Users) -> Self {
        let cpu_count = system.cpus().len();
        let memory = system.total_memory();
        let name = match value.exe() {
            Some(exe) => exe.as_os_str().to_os_string(),
            None => value.name().to_os_string(),
        };
        let user = value.user_id();
        let user = user
            .and_then(|id| users.get_user_by_id(id))
            .map(|user| user.name())
            .unwrap_or("unknown");
        let cpu_usage = value.cpu_usage() as f64 / (cpu_count as f64);
        let cpu_usage = cpu_usage as f16;
        let memory_usage = value.memory();
        let memory_percent = {
            let memory_percent = (memory_usage as f64) / (memory as f64);
            let memory_percent = memory_percent * 100.0;
            memory_percent as f16
        };
        let pid = value.pid();
        let name = Arc::from(name.to_string_lossy());
        let user = Arc::from(user);
        ProcessData {
            pid,
            name,
            user,
            cpu_usage,
            memory_usage,
            memory_percent,
        }
    }
}

#[derive(Debug)]
pub enum ProcessNode {
    Root,
    Process(Pid),
}

impl ProcessNode {
    /// Returns `true` if the process node is [`Root`].
    ///
    /// [`Root`]: ProcessNode::Root
    #[must_use]
    pub fn is_root(&self) -> bool {
        matches!(self, Self::Root)
    }

    /// Returns `true` if the process node is [`Process`].
    ///
    /// [`Process`]: ProcessNode::Process
    #[must_use]
    pub fn is_process(&self) -> bool {
        matches!(self, Self::Process(..))
    }
}

#[derive(Debug)]
pub struct ProcessTree(pub Tree<ProcessNode>);

impl Default for ProcessTree {
    fn default() -> Self {
        let tree = Tree::new(ProcessNode::Root);
        ProcessTree(tree)
    }
}

#[bon::bon]
impl ProcessTree {
    #[builder]
    pub fn try_new(proc: &HashMap<Pid, Process>) -> color_eyre::Result<Self> {
        let mut stack = vec![];
        stack.reserve(proc.len());

        let mut tree = Tree::new(ProcessNode::Root);
        let mut tree_map = HashMap::<Pid, NodeId>::default();
        proc.iter()
            .filter(|&(_, process)| process.parent().is_none())
            .for_each(|(&pid, _)| {
                let node = tree.root_mut().append(ProcessNode::Process(pid)).id();
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
            let id = tree
                .get_mut(*parent_id)
                .ok_or(eyre!("parent not in tree"))?
                .append(ProcessNode::Process(process.pid()))
                .id();
            assert!(tree_map.get(&process.pid()).is_none());
            tree_map.insert(process.pid(), id);
        }

        Ok(ProcessTree(tree))
    }

    pub fn deep_sort_by(
        &mut self,
        f: impl Fn(NodeRef<'_, ProcessNode>, NodeRef<'_, ProcessNode>) -> Ordering,
    ) {
        let mut stack = vec![];
        stack.reserve(self.0.values().len());
        stack.push(self.0.root().id());
        while let Some(node_id) = stack.pop() {
            let mut node = self
                .0
                .get_mut(node_id)
                .expect("created a node id out of thin air. i truly am magical.");
            node.sort_by(&f);
            let node = self
                .0
                .get(node_id)
                .expect("created a node id out of thin air. i truly am magical.");
            stack.extend(node.children().map(|child| child.id()))
        }
    }
}

// pub trait SkipPlaceholderRoot<'a> {
//     fn skip_placeholder_root(self) -> impl Iterator<Item = &'a Pid>;
// }

// impl<'a, I> SkipPlaceholderRoot<'a> for I
// where
//     I: Iterator<Item = NodeRef<'a, ProcessNode>>,
// {
//     fn skip_placeholder_root(self) -> impl Iterator<Item = &'a Pid> {
//         self.flat_map(|node| match node.value() {
//             ProcessNode::Root => None,
//             ProcessNode::Process(proc) => Some(proc),
//         })
//     }
// }
