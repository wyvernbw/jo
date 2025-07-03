use bytesize::ByteSize;
use crossterm::event::{Event, KeyCode, KeyModifiers};
use ratatui::{
    prelude::*,
    widgets::{Cell, Paragraph, Row, Table},
};
use std::{
    borrow::Cow, cmp::Ordering, collections::HashMap, fmt::Display, sync::Arc, time::Duration,
};
use tui_textarea::TextArea;

use ratatui::{
    DefaultTerminal,
    widgets::{TableState, Widget},
};
use smol::{Timer, channel::Sender, lock::Mutex, stream::StreamExt};
use sysinfo::{Pid, System, Users};

use crate::{
    default,
    process::{ProcessData, ProcessNode, ProcessTree},
};

#[derive(Debug, Clone)]
pub struct App {
    poll_rate: Duration,
    system: Arc<Mutex<System>>,
    users: Arc<Mutex<Users>>,
}

impl Default for App {
    fn default() -> Self {
        App {
            system: default(),
            users: default(),
            poll_rate: Duration::from_millis(1500),
        }
    }
}

impl App {
    async fn fetch_sysinfo_task(self: Self, tx: Sender<AppEvent>) -> color_eyre::Result<()> {
        loop {
            let mut system = self.system.lock().await;
            let mut users = self.users.lock().await;
            system.refresh_all();
            users.refresh();
            let proc = system.processes();
            let data = proc
                .iter()
                .map(|(pid, proc)| (*pid, ProcessData::from_process(proc, &system, &users)))
                .collect();
            let proc = ProcessTree::try_new().proc(proc).call();
            drop(system);
            drop(users);
            if let Ok(proc) = proc {
                tx.send(AppEvent::UpdateProcess(proc, data)).await?;
            }
            Timer::after(self.poll_rate).await;
        }
    }

    async fn input_task(tx: Sender<AppEvent>) -> color_eyre::Result<()> {
        let mut stream = crossterm::event::EventStream::new();
        loop {
            if let Some(Ok(input_event)) = stream.next().await {
                tx.send(AppEvent::Input(input_event)).await?;
            };
        }
    }

    async fn run_side_effects(&mut self, transition: &Transition) {
        match transition {
            Transition::KillProcess(pid) => {
                if let Some(process) = self.system.lock().await.process(*pid) {
                    process.kill();
                }
            }
            _ => {}
        }
    }

    pub async fn run(mut self, term: &mut DefaultTerminal) -> color_eyre::Result<()> {
        let mut state = State::default();
        let event_channel = smol::channel::unbounded();
        smol::spawn(self.clone().fetch_sysinfo_task(event_channel.0.clone())).detach();
        smol::spawn(Self::input_task(event_channel.0.clone())).detach();

        term.draw(|frame| frame.render_stateful_widget(&self, frame.area(), &mut state))?;
        state.process_table_state.select_first();
        loop {
            let event = event_channel.1.recv().await?;
            let transition = event.into_transition(&state);

            self.run_side_effects(&transition).await;
            state = transition.transition(state);
            state = state.prepare();
            if state.exit {
                break Ok(());
            }
            term.draw(|frame| frame.render_stateful_widget(&self, frame.area(), &mut state))?;
        }
    }
}

enum AppEvent {
    UpdateProcess(ProcessTree, HashMap<Pid, ProcessData>),
    Input(Event),
}

impl AppEvent {
    fn into_transition(self, state: &State) -> Transition {
        let selected_pid = match (&state.process_rows, state.process_table_state.selected()) {
            (Some(process_rows), Some(idx)) => Some(process_rows[idx].2),
            _ => None,
        };
        match (&state.mode, self) {
            (_, AppEvent::UpdateProcess(process_tree, hash_map)) => {
                Transition::UpdateProcess(process_tree, hash_map)
            }
            (Mode::Normal, AppEvent::Input(Event::Key(key_event))) => {
                match (key_event.code, key_event.modifiers) {
                    (KeyCode::Char('q'), _) => Transition::Exit,
                    (KeyCode::Char('j'), _) => Transition::ScrollDown,
                    (KeyCode::Char('k'), _) => Transition::ScrollUp,
                    (KeyCode::Char('/'), _) => Transition::StartSearch,
                    (KeyCode::Char('n'), _) => Transition::NextSearchResult,
                    (KeyCode::Char('p'), _) => Transition::PrevSearchResult,
                    (KeyCode::Char('d'), _) if let Some(selected_pid) = selected_pid => {
                        Transition::KillProcess(selected_pid)
                    }
                    (KeyCode::Char('d'), _) if let None = selected_pid => Transition::None,
                    _ => Transition::None,
                }
            }
            (Mode::Normal, AppEvent::Input(_)) => Transition::None,
            (Mode::Search(_), AppEvent::Input(Event::Key(key_event))) => {
                match (key_event.code, key_event.modifiers) {
                    (KeyCode::Char(ch), KeyModifiers::NONE) => {
                        Transition::SearchType(SearchTypeTransition::Type(ch))
                    }
                    (KeyCode::Char(ch), KeyModifiers::SHIFT) => {
                        Transition::SearchType(SearchTypeTransition::Type(ch.to_ascii_uppercase()))
                    }
                    (KeyCode::Delete | KeyCode::Backspace, _) => {
                        Transition::SearchType(SearchTypeTransition::Delete)
                    }
                    (KeyCode::Esc | KeyCode::Enter, _) => Transition::CompleteSearch,
                    _ => Transition::None,
                }
            }
            (Mode::Search(_), AppEvent::Input(_)) => Transition::None,
        }
    }
}

type ProcessRow = (usize, TreePrefix, Pid);

#[derive(Debug, Default)]
pub struct State {
    exit: bool,
    process_view: ProcessView,
    sort_mode: SortMode,
    mode: Mode,
    old_search_state: Option<SearchState>,
    process_tree: ProcessTree,
    process_rows: Option<Vec<ProcessRow>>,
    process_data: HashMap<Pid, ProcessData>,
    process_table_state: TableState,
}

#[derive(Debug, Default, Clone, Copy)]
enum SortMode {
    #[default]
    Cpu,
}

impl State {
    fn update_search(self, search_state: SearchState) -> State {
        let Some(process_rows) = &self.process_rows else {
            match self.mode {
                Mode::Search(_) => {
                    return State {
                        mode: Mode::Search(search_state),
                        ..self
                    };
                }
                Mode::Normal => {
                    return self;
                }
            }
        };
        let matches = process_rows
            .iter()
            .enumerate()
            .flat_map(|(idx, (_, _, pid))| Some(idx).zip(self.process_data.get(pid)))
            .filter(|&(_, proc)| {
                proc.pid.as_u32().to_string() == search_state.term.as_ref()
                    || proc
                        .name
                        .to_ascii_lowercase()
                        .to_string_lossy()
                        .contains(search_state.term.as_ref())
            })
            .collect::<Vec<_>>();

        let result = match matches.len() {
            0 => None,
            len => Some(matches[(search_state.idx.rem_euclid(len as isize)) as usize].0),
        };

        match self.mode {
            Mode::Normal => State {
                process_table_state: self.process_table_state.with_selected(result),
                old_search_state: Some(search_state),
                ..self
            },
            Mode::Search(_) => State {
                process_table_state: self.process_table_state.with_selected(result),
                mode: Mode::Search(search_state),
                ..self
            },
        }
    }
    fn refresh_search(self) -> State {
        if let Some(search_state) = self.old_search_state.clone() {
            self.update_search(search_state)
        } else {
            self
        }
    }
    fn hook_search(mut self) -> State {
        if let Some(search_state) = &mut self.old_search_state {
            search_state.hooked = true;
        }
        self
    }
    fn unhook_search(mut self) -> State {
        if let Some(search_state) = &mut self.old_search_state {
            search_state.hooked = false;
        }
        self
    }
    fn refresh_search_if_hooked(self) -> State {
        match (self.mode.clone(), self.old_search_state.clone()) {
            (Mode::Normal, Some(search_state)) | (Mode::Search(search_state), _)
                if search_state.hooked =>
            {
                self.update_search(search_state)
            }
            _ => self,
        }
    }
    fn prepare(mut self) -> State {
        match self.process_view {
            ProcessView::Tree => {
                self.process_tree.deep_sort_by(|a, b| -> Ordering {
                    match (a.value(), b.value()) {
                        (ProcessNode::Root, ProcessNode::Root) => {
                            unreachable!("only one root for the tree")
                        }
                        (ProcessNode::Root, ProcessNode::Process(_)) => Ordering::Less,
                        (ProcessNode::Process(_), ProcessNode::Root) => Ordering::Greater,
                        (ProcessNode::Process(a), ProcessNode::Process(b)) => {
                            if let Some((a, b)) =
                                self.process_data.get(a).zip(self.process_data.get(b))
                            {
                                self.sort_mode.call((a, b))
                            } else {
                                Ordering::Equal
                            }
                        }
                    }
                });
            }
            ProcessView::Flat => {}
        };

        let mut depth = 0usize;
        let mut proc_data = self
            .process_tree
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
                ProcessNode::Process(proc) => {
                    Some((depth, prefix, self.process_data.get(proc).unwrap().clone()))
                }
            })
            .collect::<Vec<_>>();

        match self.process_view {
            ProcessView::Tree => {}
            ProcessView::Flat => proc_data.sort_by(|(_, _, a), (_, _, b)| self.sort_mode.cmp(a, b)),
        }

        let proc_data = proc_data
            .into_iter()
            .map(|(depth, prefix, proc)| (depth, prefix, proc.pid))
            .collect();

        self.process_rows = Some(proc_data);

        self
    }
}

impl SortMode {
    fn cmp(&self, a: &ProcessData, b: &ProcessData) -> Ordering {
        match self {
            SortMode::Cpu => b.cpu_usage.total_cmp(&a.cpu_usage),
        }
    }
}

impl FnOnce<(&ProcessData, &ProcessData)> for SortMode {
    type Output = Ordering;
    extern "rust-call" fn call_once(self, (a, b): (&ProcessData, &ProcessData)) -> Self::Output {
        self.cmp(a, b)
    }
}

impl FnMut<(&ProcessData, &ProcessData)> for SortMode {
    extern "rust-call" fn call_mut(
        &mut self,
        (a, b): (&ProcessData, &ProcessData),
    ) -> Self::Output {
        self.cmp(a, b)
    }
}

impl Fn<(&ProcessData, &ProcessData)> for SortMode {
    extern "rust-call" fn call(&self, (a, b): (&ProcessData, &ProcessData)) -> Self::Output {
        self.cmp(a, b)
    }
}

#[derive(Debug, Default, Clone)]
enum Mode {
    #[default]
    Normal,
    Search(SearchState),
}

#[derive(Debug, Clone)]
struct SearchState {
    term: Arc<str>,
    idx: isize,
    hooked: bool,
}

impl Default for SearchState {
    fn default() -> Self {
        SearchState {
            term: "".into(),
            idx: 0,
            hooked: true,
        }
    }
}

impl Display for Mode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Mode::Normal => write!(f, "NORM"),
            Mode::Search(_) => write!(f, "SEARCH"),
        }
    }
}

#[derive(Debug, Default, Clone)]
enum ProcessView {
    Tree,
    #[default]
    Flat,
}

impl ProcessView {
    fn make_row(&self, depth: usize, prefix: &TreePrefix, proc: &ProcessData) -> Row<'static> {
        match self {
            ProcessView::Tree => Row::new([
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
            ]),
            ProcessView::Flat => Row::new([
                Cell::new(proc.pid.to_string()),
                Cell::new(proc.user.to_string()).style(grey_out(proc.user == "root")),
                Cell::new(format!("{:.2}%", proc.cpu_usage)).style(grey_out(proc.cpu_usage < 0.01)),
                Cell::new(ByteSize::b(proc.memory_usage).display().iec().to_string())
                    .style(grey_out(proc.memory_usage == 0)),
                Cell::new(format!("{:.2}%", proc.memory_percent))
                    .style(grey_out(proc.memory_percent < 0.01)),
                Cell::new(format!("{}", proc.name.display())),
            ]),
        }
    }
}

#[derive(Debug, Clone, Copy)]
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

#[derive(Debug)]
enum Transition {
    None,
    UpdateProcess(ProcessTree, HashMap<Pid, ProcessData>),
    Exit,
    ScrollUp,
    ScrollDown,
    SearchType(SearchTypeTransition),
    StartSearch,
    CompleteSearch,
    NextSearchResult,
    PrevSearchResult,
    KillProcess(Pid),
}

#[derive(Debug)]
enum SearchTypeTransition {
    Type(char),
    Delete,
}

impl Transition {
    fn transition(self, state: State) -> State {
        match (state.mode.clone(), self) {
            (_, Transition::None) => state,
            (_, Transition::Exit) => State {
                exit: true,
                ..state
            },
            (_, Transition::UpdateProcess(tree, data)) => {
                let new_state = State {
                    process_tree: tree,
                    process_data: data,
                    ..state
                };
                new_state.prepare().refresh_search_if_hooked()
            }
            (_, Transition::ScrollUp) => {
                let mut table_state = state.process_table_state;
                table_state.select_previous();
                State {
                    process_table_state: table_state,
                    ..state
                }
                .unhook_search()
            }
            (_, Transition::ScrollDown) => {
                let mut table_state = state.process_table_state;
                table_state.select_next();
                State {
                    process_table_state: table_state,
                    ..state
                }
                .unhook_search()
            }
            (_, Transition::KillProcess(_)) => state.refresh_search(),
            (Mode::Normal, Transition::SearchType(_)) => unreachable!(),
            (Mode::Search(search_state), Transition::SearchType(search_type_transition)) => {
                let new_term = match search_type_transition {
                    SearchTypeTransition::Type(ch) => {
                        let term = Arc::<str>::from(format!("{}{}", search_state.term, ch));
                        term
                    }
                    SearchTypeTransition::Delete => {
                        let term = Arc::<str>::from(
                            &search_state.term[..search_state.term.len().saturating_sub(1)],
                        );
                        term
                    }
                };
                state
                    .update_search(SearchState {
                        term: new_term,
                        hooked: true,
                        ..search_state
                    })
                    .hook_search()
            }
            (Mode::Normal | Mode::Search(_), Transition::NextSearchResult) => {
                let Some(search_state) = state.old_search_state.as_ref().map(|st| SearchState {
                    idx: st.idx + 1,
                    ..st.clone()
                }) else {
                    return state;
                };
                state.update_search(search_state).hook_search()
            }
            (Mode::Normal | Mode::Search(_), Transition::PrevSearchResult) => {
                let Some(search_state) = state.old_search_state.as_ref().map(|st| SearchState {
                    idx: st.idx - 1,
                    ..st.clone()
                }) else {
                    return state;
                };
                state.update_search(search_state).hook_search()
            }
            (Mode::Normal, Transition::StartSearch) => State {
                mode: Mode::Search(SearchState::default()),
                old_search_state: None,
                ..state
            },
            (Mode::Normal, Transition::CompleteSearch) => unreachable!(),
            (Mode::Search(_), Transition::StartSearch) => unreachable!(),
            (Mode::Search(search_state), Transition::CompleteSearch) => State {
                mode: Mode::Normal,
                old_search_state: Some(search_state.clone()),
                ..state
            }
            .hook_search(),
        }
    }
}

impl StatefulWidget for &App {
    type State = State;

    fn render(self, area: Rect, buf: &mut Buffer, state: &mut Self::State)
    where
        Self: Sized,
    {
        let layout = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(10), Constraint::Length(1)])
            .spacing(1)
            .split(area);

        let term_height = crossterm::terminal::size().map(|(c, r)| r).unwrap_or(64) as usize;
        let selected_idx = state.process_table_state.selected().unwrap_or_default();
        let proc_data = state
            .process_rows
            .clone()
            .unwrap_or(vec![])
            .iter()
            .cloned()
            .take(term_height + selected_idx)
            .flat_map(|(depth, prefix, proc)| {
                Some((depth, prefix)).zip(state.process_data.get(&proc))
            })
            .map(|((depth, prefix), proc)| (depth, prefix, proc))
            .collect::<Vec<_>>();
        let rows = proc_data
            .iter()
            .enumerate()
            .map(|(idx, (depth, prefix, proc))| {
                if idx + term_height < selected_idx {
                    return Row::default();
                }
                state.process_view.make_row(*depth, prefix, proc)
            });

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

        StatefulWidget::render(
            table,
            layout[0],
            buf,
            &mut state.process_table_state.clone(),
        );

        let modeline_layout = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Min(8), Constraint::Min(8)])
            .split(layout[1]);

        let (selected_pid, selected_name) = match state.process_table_state.selected() {
            Some(idx) => (
                Cow::Owned(format!("@ {}", proc_data[idx].2.pid,)),
                Cow::Owned(format!("{}", proc_data[idx].2.name.display())),
            ),
            None => (Cow::Borrowed(""), Cow::Borrowed("")),
        };

        match &state.mode {
            Mode::Normal => {
                Paragraph::new(format!(" {} {}", state.mode, selected_pid))
                    .style(Style::new().bg(Color::Black))
                    .render(modeline_layout[0], buf);
            }
            Mode::Search(search_state) => {
                let search_layout = Layout::default()
                    .direction(Direction::Horizontal)
                    .constraints([Constraint::Max(8), Constraint::Min(1)])
                    .split(modeline_layout[0]);

                Paragraph::new(format!(" {}: ", state.mode))
                    .style(Style::new().bg(Color::Black))
                    .render(search_layout[0], buf);

                let mut text_area = TextArea::new(vec![search_state.term.to_string()]);
                text_area.move_cursor(tui_textarea::CursorMove::End);
                text_area.render(search_layout[1], buf);
            }
        };

        Paragraph::new(selected_name)
            .style(Style::new().bg(Color::Black))
            .right_aligned()
            .render(modeline_layout[1], buf);
    }
}

fn grey_out(condition: bool) -> Style {
    match condition {
        true => Style::new().dark_gray(),
        false => Style::new(),
    }
}
