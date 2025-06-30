use std::{borrow::Cow, cmp::Ordering, fmt::Display, ops::Index, time::Duration};

use bytesize::ByteSize;
use crossterm::event::{Event, EventStream, KeyCode, KeyEvent, KeyEventKind};
use ratatui::{
    DefaultTerminal,
    prelude::*,
    widgets::{Block, Cell, Paragraph, Row, Table, TableState},
};
use smol::{
    Timer,
    channel::{Receiver, Sender},
    stream::StreamExt,
};
use sysinfo::{System, Users};

use crate::process::{ProcessData, ProcessNode, ProcessTree, SkipPlaceholderRoot};

#[derive(Debug, Default)]
pub struct App {
    exit: bool,
    pub tree_view: bool,
    system_information: Option<SystemInformation>,
    system: System,
    users: Users,
    table_state: TableState,
    mode: Mode,
}

#[derive(Debug, Default, Clone)]
enum Mode {
    #[default]
    Normal,
    Go,
    Search(String),
}

impl Display for Mode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Mode::Normal => write!(f, "NORM"),
            Mode::Go => write!(f, "GO"),
            Mode::Search(_) => write!(f, "SEARCH"),
        }
    }
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
    Down,
    Up,
    Search,
    Go,
    Cancel,
}

enum TreeData<'a> {
    Tree(Vec<(usize, TreePrefix, &'a ProcessData)>),
    Flat(Vec<&'a ProcessData>),
}

impl<'a> Index<usize> for TreeData<'a> {
    type Output = ProcessData;

    fn index(&self, index: usize) -> &Self::Output {
        match self {
            TreeData::Tree(items) => items[index].2,
            TreeData::Flat(process_datas) => process_datas[index],
        }
    }
}

impl App {
    /// runs the application's main loop until the user quits
    pub async fn run(&mut self, terminal: &mut DefaultTerminal) -> color_eyre::Result<()> {
        let input_channel = smol::channel::bounded(2);
        let mode_channel = smol::channel::bounded(2);
        mode_channel.0.send(self.mode.clone()).await?;
        smol::spawn(Self::input_task(input_channel.0, mode_channel.1)).detach();
        self.fetch_system_information()?;
        self.table_state.select_first();
        let mut duration = Duration::from_millis(1500);
        while !self.exit {
            terminal.draw(|frame| self.draw(frame))?;
            let input_event = Self::receive_input(&input_channel.1);
            let event = smol::future::race(
                async {
                    Timer::after(duration).await;
                    Ok(LoopEvent::SysInfo)
                },
                input_event,
            )
            .await;
            tracing::info!(?event);
            match event? {
                LoopEvent::Input(command) => {
                    if let Err(err) = self.handle_command(command) {
                        tracing::warn!(%err);
                    }
                    duration = Duration::from_millis(500);
                    mode_channel.0.send(self.mode.clone()).await?;
                }
                LoopEvent::SysInfo => {
                    self.fetch_system_information()?;
                    duration = Duration::from_millis(1500);
                }
            };
        }
        tracing::info!("jo event loop closed");
        Ok(())
    }

    fn draw(&mut self, frame: &mut Frame) {
        frame.render_widget(self, frame.area());
    }

    fn fetch_system_information(&mut self) -> color_eyre::Result<()> {
        self.users.refresh();
        self.system.refresh_all();
        let processes = self.system.processes();
        let ptree = ProcessTree::try_new()
            .proc(&processes)
            .system(&self.system)
            .users(&self.users)
            .call()?;
        self.system_information = Some(SystemInformation { processes: ptree });
        Ok(())
    }

    async fn input_task(tx: Sender<LoopEvent>, rx: Receiver<Mode>) -> color_eyre::Result<()> {
        let mut stream = crossterm::event::EventStream::new();
        loop {
            let mode = rx.recv().await?;
            if let Ok(res) = Self::get_input_event(&mut stream, mode).await {
                tx.send(res).await?;
            };
        }
    }

    async fn get_input_event(
        stream: &mut EventStream,
        current_mode: Mode,
    ) -> color_eyre::Result<LoopEvent> {
        loop {
            if let Some(Ok(input_event)) = stream.next().await {
                match input_event {
                    // it's important to check that the event is a key press event as
                    // crossterm also emits key release and repeat events on Windows.
                    Event::Key(key_event) if key_event.kind == KeyEventKind::Press => {
                        if let Some(command) = Self::map_key_to_command(&key_event, &current_mode) {
                            return Ok(LoopEvent::Input(command));
                        }
                    }
                    _ => {}
                };
            };
        }
    }

    fn handle_command(&mut self, command: Command) -> color_eyre::Result<()> {
        match (&self.mode, command) {
            (Mode::Normal, Command::Quit) => {
                self.exit = true;
                tracing::info!("jo quitting");
            }
            (Mode::Normal | Mode::Go, Command::Down) => {
                self.table_state.scroll_down_by(1);
            }
            (Mode::Normal | Mode::Go, Command::Up) => {
                self.table_state.scroll_up_by(1);
                self.mode = Mode::Normal;
            }
            (Mode::Normal, Command::Search) => {
                self.mode = Mode::Search("".to_string());
            }
            (Mode::Normal, Command::Go) => {
                self.mode = Mode::Go;
            }
            (Mode::Normal, Command::Cancel) => {}
            (Mode::Go, Command::Quit) => {}
            (Mode::Go, Command::Search) => {}
            (Mode::Go, Command::Cancel) => {
                self.mode = Mode::Normal;
            }
            (Mode::Go, Command::Go) => unreachable!(),
            (Mode::Search(_), Command::Quit) => unreachable!(),
            (Mode::Search(_), Command::Down) => unreachable!(),
            (Mode::Search(_), Command::Up) => unreachable!(),
            (Mode::Search(_), Command::Search) => unreachable!(),
            (Mode::Search(_), Command::Cancel) => {
                self.mode = Mode::Normal;
            }
            (Mode::Search(_), Command::Go) => unreachable!(),
        };
        Ok(())
    }

    fn map_key_to_command(key_event: &KeyEvent, current_mode: &Mode) -> Option<Command> {
        match (current_mode, key_event) {
            (
                Mode::Normal,
                KeyEvent {
                    code: KeyCode::Char('q'),
                    ..
                },
            ) => {
                return Some(Command::Quit);
            }
            (
                Mode::Normal | Mode::Go,
                KeyEvent {
                    code: KeyCode::Char('j') | KeyCode::Down,
                    ..
                },
            ) => return Some(Command::Down),
            (
                Mode::Normal | Mode::Go,
                KeyEvent {
                    code: KeyCode::Char('k') | KeyCode::Up,
                    ..
                },
            ) => return Some(Command::Up),
            (
                Mode::Normal,
                KeyEvent {
                    code: KeyCode::Char('/'),
                    ..
                },
            ) => return Some(Command::Search),
            (
                Mode::Search(_) | Mode::Go,
                KeyEvent {
                    code: KeyCode::Esc, ..
                },
            ) => return Some(Command::Cancel),
            (
                Mode::Normal,
                KeyEvent {
                    code: KeyCode::Char('g'),
                    ..
                },
            ) => return Some(Command::Go),
            _ => {}
        };
        None
    }

    async fn receive_input(rx: &Receiver<LoopEvent>) -> color_eyre::Result<LoopEvent> {
        let res = rx.recv().await?;
        Ok(res)
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

    fn tree_get_process_data_points<'a>(&self) -> Vec<(usize, TreePrefix, &ProcessData)> {
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
            .collect::<Vec<_>>();
        rows
    }

    fn tree_map_process_rows<'b>(data: &[(usize, TreePrefix, &ProcessData)]) -> Vec<Row<'b>> {
        data.into_iter()
            .map(|(depth, prefix, proc)| {
                Row::new([
                    Cell::new(proc.pid.to_string()),
                    Cell::new(proc.user.to_string()),
                    Cell::new(format!("{:.2}%", proc.cpu_usage)),
                    Cell::new(ByteSize::b(proc.memory_usage).display().iec().to_string()),
                    Cell::new(format!("{:.2}%", proc.memory_percent)),
                    Cell::new(format!(
                        "{}{} {}",
                        " │ ".repeat(depth.saturating_sub(2)),
                        prefix,
                        proc.name.display()
                    )),
                ])
            })
            .collect::<Vec<_>>()
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
            Cell::new(proc.user.to_string()).style(grey_out(proc.user == "root")),
            Cell::new(format!("{:.2}%", proc.cpu_usage)).style(grey_out(proc.cpu_usage < 0.01)),
            Cell::new(ByteSize::b(proc.memory_usage).display().iec().to_string())
                .style(grey_out(proc.memory_usage == 0)),
            Cell::new(format!("{:.2}%", proc.memory_percent))
                .style(grey_out(proc.memory_percent < 0.01)),
            Cell::new(format!("{}", proc.name.display())),
        ])
    }

    fn flat_get_process_rows<'a>(&self) -> Vec<Row<'a>> {
        self.flatten_tree()
            .into_iter()
            .map(Self::flat_map_process_to_row)
            .collect::<Vec<_>>()
    }

    fn get_process_data<'a>(&'a mut self) -> TreeData<'a> {
        match self.tree_view {
            true => {
                self.tree_sort_processes();
                let data = self.tree_get_process_data_points();
                TreeData::Tree(data)
            }
            false => {
                let mut rows = self.flatten_tree();
                rows.sort_by(|&a, &b| Self::sort_by_cpu(a, b));
                TreeData::Flat(rows)
            }
        }
    }

    fn map_process_data<'a>(data: &TreeData<'a>) -> Vec<Row<'a>> {
        match data {
            TreeData::Tree(data) => Self::tree_map_process_rows(&data),
            TreeData::Flat(data) => data
                .iter()
                .cloned()
                .map(Self::flat_map_process_to_row)
                .collect(),
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
        let layout = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(10), Constraint::Length(1)])
            .spacing(1)
            .split(area);
        let mut state = self.table_state.clone();
        let proc_data = self.get_process_data();
        let rows = App::map_process_data(&proc_data);

        let table = Table::new(
            rows,
            [
                Constraint::Length(8),
                Constraint::Max(10),
                Constraint::Length(8),
                Constraint::Length(8),
                Constraint::Length(8),
                Constraint::Min(20),
            ],
        )
        .header(
            Row::new(["PID", "USER", "CPU%", "MEM(B)", "MEM%", "Command"])
                .style(Style::new().bg(Color::Green).fg(Color::Indexed(0))),
        )
        .row_highlight_style(Style::new().bg(Color::White).fg(Color::Black));
        StatefulWidget::render(table, layout[0], buf, &mut state);

        let modeline_layout = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Max(32), Constraint::Min(2), Constraint::Max(32)])
            .split(layout[1]);

        let (selected_pid, selected_name) = match state.selected() {
            Some(idx) => (
                Cow::Owned(format!("@ {}", proc_data[idx].pid,)),
                Cow::Owned(format!("{}", proc_data[idx].name.display())),
            ),
            None => (Cow::Borrowed(""), Cow::Borrowed("")),
        };
        Paragraph::new(format!(" {} {}", self.mode, selected_pid))
            .style(Style::new().bg(Color::Black))
            .render(modeline_layout[0], buf);
        Block::new()
            .style(Style::new().bg(Color::Black))
            .render(modeline_layout[1], buf);

        Paragraph::new(selected_name)
            .style(Style::new().bg(Color::Black))
            .right_aligned()
            .render(modeline_layout[2], buf);

        self.table_state = state;
    }
}

fn grey_out(condition: bool) -> Style {
    match condition {
        true => Style::new().dark_gray(),
        false => Style::new(),
    }
}
