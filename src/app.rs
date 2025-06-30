use std::{cmp::Ordering, fmt::Display, time::Duration};

use bytesize::ByteSize;
use crossterm::event::{Event, EventStream, KeyCode, KeyEvent, KeyEventKind};
use ratatui::{
    DefaultTerminal,
    prelude::*,
    widgets::{Cell, Row, Table, TableState},
};
use smol::{
    channel::{Receiver, Sender},
    stream::StreamExt,
};
use sysinfo::{CpuRefreshKind, System};

use crate::process::{ProcessData, ProcessNode, ProcessTree, SkipPlaceholderRoot};

#[derive(Debug, Default)]
pub struct App {
    exit: bool,
    tree_view: bool,
    system_information: Option<SystemInformation>,
    table_state: TableState,
}

#[derive(Debug)]
struct SystemInformation {
    processes: ProcessTree,
}

#[derive(Debug)]
enum LoopEvent {
    Input(Command),
    SysInfo,
}

#[derive(Debug)]
enum Command {
    Quit,
}

impl App {
    /// runs the application's main loop until the user quits
    pub async fn run(&mut self, terminal: &mut DefaultTerminal) -> color_eyre::Result<()> {
        let (tx, rx) = smol::channel::bounded(32);
        smol::spawn(async {
            let res = Self::sysinfo_task(tx).await;
            match res {
                Ok(_) => tracing::info!("sysinfo task finished"),
                Err(err) => tracing::error!(?err, "sysinfo task died"),
            }
        })
        .detach();
        let mut stream = crossterm::event::EventStream::new();
        while !self.exit {
            let sys_info_event = self.receive_system_information(&rx);
            let input_event = Self::get_input_event(&mut stream);
            let event = smol::future::race(sys_info_event, input_event).await;
            tracing::info!(?event);
            match event? {
                LoopEvent::Input(command) => {
                    if let Err(err) = self.handle_command(command) {
                        tracing::warn!(%err);
                    }
                }
                LoopEvent::SysInfo => {}
            };
            terminal.draw(|frame| self.draw(frame))?;
        }
        tracing::info!("jo event loop closed");
        Ok(())
    }

    fn draw(&mut self, frame: &mut Frame) {
        frame.render_widget(self, frame.area());
    }

    async fn get_input_event(stream: &mut EventStream) -> color_eyre::Result<LoopEvent> {
        loop {
            if let Some(Ok(input_event)) = stream.next().await {
                match input_event {
                    // it's important to check that the event is a key press event as
                    // crossterm also emits key release and repeat events on Windows.
                    Event::Key(key_event) if key_event.kind == KeyEventKind::Press => {
                        if let Some(command) = Self::map_key_to_command(&key_event) {
                            return Ok(LoopEvent::Input(command));
                        }
                    }
                    _ => {}
                };
            };
        }
    }

    fn handle_command(&mut self, command: Command) -> color_eyre::Result<()> {
        match command {
            Command::Quit => {
                self.exit = true;
                tracing::info!("jo quitting");
            }
        };
        Ok(())
    }

    fn map_key_to_command(key_event: &KeyEvent) -> Option<Command> {
        match key_event {
            KeyEvent {
                code: KeyCode::Char('q'),
                ..
            } => {
                return Some(Command::Quit);
            }
            _ => {}
        };
        None
    }

    async fn sysinfo_task(tx: Sender<SystemInformation>) -> color_eyre::Result<()> {
        let mut system = System::new();
        system.refresh_cpu_list(CpuRefreshKind::default());
        system.refresh_memory();
        let cpu_count = system.cpus().len();
        let memory = system.total_memory();
        tracing::info!(?memory);
        loop {
            system.refresh_all();
            let processes = system.processes();
            let ptree = ProcessTree::try_new()
                .proc(processes)
                .cpu_count(cpu_count)
                .memory(memory)
                .call()?;
            tx.send(SystemInformation { processes: ptree }).await?;
            smol::Timer::after(Duration::from_millis(2000)).await;
        }
    }

    async fn receive_system_information(
        &mut self,
        rx: &Receiver<SystemInformation>,
    ) -> color_eyre::Result<LoopEvent> {
        let res = rx.recv().await?;
        self.system_information = Some(res);
        Ok(LoopEvent::SysInfo)
    }

    fn sort_by_cpu(a: &ProcessData, b: &ProcessData) -> Ordering {
        b.cpu_usage.total_cmp(&a.cpu_usage)
    }

    fn tree_sort_processes(&mut self) {
        let Some(system_information) = &mut self.system_information else {
            tracing::warn!("no system information");
            return;
        };
        let processes = &mut system_information.processes;
        processes.deep_sort_by(|a, b| match (a.value(), b.value()) {
            (ProcessNode::Root, ProcessNode::Root) => unreachable!("only one root for the tree"),
            (ProcessNode::Root, ProcessNode::Process(_)) => Ordering::Less,
            (ProcessNode::Process(_), ProcessNode::Root) => Ordering::Greater,
            (ProcessNode::Process(a), ProcessNode::Process(b)) => Self::sort_by_cpu(a, b),
        });
    }

    fn tree_get_process_rows<'a>(&self) -> Vec<Row<'a>> {
        let Some(system_information) = &self.system_information else {
            tracing::warn!("no system information");
            return vec![];
        };
        let processes = &system_information.processes;
        let mut depth = 0usize;
        let rows = processes
            .0
            .root()
            .traverse()
            .flat_map(|edge| match edge {
                ego_tree::iter::Edge::Open(node_ref) => {
                    depth += 1;
                    if depth <= 2 {
                        return Some((depth, TreePrefix::FirstChild, node_ref));
                    }
                    if node_ref.next_sibling().is_none() {
                        return Some((depth, TreePrefix::LastChild, node_ref));
                    }
                    Some((depth, TreePrefix::MiddleChild, node_ref))
                }
                ego_tree::iter::Edge::Close(_) => {
                    depth -= 1;
                    None
                }
            })
            .flat_map(|(depth, prefix, node)| match node.value() {
                ProcessNode::Root => None,
                ProcessNode::Process(proc) => Some((depth, prefix, proc)),
            })
            .map(|(depth, prefix, proc)| {
                Row::new([
                    Cell::new(proc.pid.to_string()),
                    Cell::new(format!("{:.2}%", proc.cpu_usage)),
                    Cell::new(ByteSize::b(proc.memory_usage).display().iec().to_string()),
                    Cell::new(format!(
                        "{}{} {}",
                        " │ ".repeat(depth.saturating_sub(2)),
                        prefix,
                        proc.name.display()
                    )),
                ])
            })
            .collect::<Vec<_>>();
        rows
    }

    fn flatten_tree(&self) -> Vec<&ProcessData> {
        let Some(system_information) = &self.system_information else {
            tracing::warn!("no system information");
            return vec![];
        };
        let processes = &system_information.processes;
        let rows = processes
            .0
            .root()
            .traverse()
            .flat_map(|edge| match edge {
                ego_tree::iter::Edge::Open(node_ref) => Some(node_ref),
                ego_tree::iter::Edge::Close(_) => None,
            })
            .skip_placeholder_root()
            .collect::<Vec<_>>();
        rows
    }

    fn flat_map_process_to_row<'a>(proc: &ProcessData) -> Row<'a> {
        Row::new([
            Cell::new(proc.pid.to_string()),
            Cell::new(format!("{:.2}%", proc.cpu_usage)),
            Cell::new(ByteSize::b(proc.memory_usage).display().iec().to_string()),
            Cell::new(format!("{:.2}%", proc.memory_percent)),
            Cell::new(format!("{}", proc.name.display())),
        ])
    }

    fn flat_get_process_rows<'a>(&self) -> Vec<Row<'a>> {
        self.flatten_tree()
            .into_iter()
            .map(Self::flat_map_process_to_row)
            .collect::<Vec<_>>()
    }

    fn get_process_rows<'a>(&mut self) -> Vec<Row<'a>> {
        match self.tree_view {
            true => {
                self.tree_sort_processes();
                self.tree_get_process_rows()
            }
            false => {
                let mut rows = self.flatten_tree();
                rows.sort_by(|&a, &b| Self::sort_by_cpu(a, b));
                rows.into_iter()
                    .map(Self::flat_map_process_to_row)
                    .collect()
            }
        }
    }
}

enum TreePrefix {
    FirstChild,
    MiddleChild,
    LastChild,
}

impl Display for TreePrefix {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let res = match self {
            TreePrefix::FirstChild => "",
            TreePrefix::MiddleChild => " ├─",
            TreePrefix::LastChild => " └─",
        };
        write!(f, "{}", res)
    }
}

impl Widget for &mut App {
    fn render(self, area: Rect, buf: &mut Buffer)
    where
        Self: Sized,
    {
        let rows = self.get_process_rows();

        let table = Table::new(
            rows,
            [
                Constraint::Length(8),
                Constraint::Length(8),
                Constraint::Length(12),
                Constraint::Length(8),
                Constraint::Min(20),
            ],
        );
        StatefulWidget::render(table, area, buf, &mut self.table_state);
    }
}
