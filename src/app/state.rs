use std::{cmp::Ordering, collections::HashMap, fmt::Display, sync::Arc};

use bit_vec::BitVec;
use bytesize::ByteSize;
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Style},
    widgets::{Cell, Row, TableState},
};
use sysinfo::Pid;
use tachyonfx::{Duration, Effect, EffectRenderer, EffectTimer, Interpolation, Shader, fx};

use crate::{
    app::{SystemInformation, grey_out},
    process::{ProcessData, ProcessNode, ProcessTree},
};

pub type ProcessRow = (usize, TreePrefix, Pid);

#[derive(Debug, Default)]
pub struct State {
    pub exit: bool,
    pub process_view: ProcessView,
    pub sort_mode: SortMode,
    pub mode: Mode,
    pub old_search_state: Option<SearchState>,
    pub search_matches: BitVec,
    pub sysinfo: SystemInformation,
    pub process_rows: Option<Vec<ProcessRow>>,
    pub process_table_state: TableState,
    pub effects: TuiEffects,
    pub dt: Duration,
    pub last_killed_pid: Option<Pid>,
}

#[derive(Debug, Default, Clone, Copy)]
pub enum SortMode {
    #[default]
    Cpu,
}

impl State {
    fn update_search(self, search_state: SearchState) -> State {
        let Some(process_rows) = &self.process_rows else {
            match self.mode {
                Mode::Search(_) => {
                    return State {
                        mode: Mode::Search(search_state.clone()),
                        old_search_state: Some(search_state),
                        ..self
                    };
                }
                Mode::Normal => {
                    return State {
                        old_search_state: Some(search_state),
                        ..self
                    };
                }
            }
        };
        let matches = process_rows
            .iter()
            .enumerate()
            .flat_map(|(idx, (_, _, pid))| Some(idx).zip(self.sysinfo.process_data.get(pid)))
            .filter(|&(_, proc)| {
                proc.pid.as_u32().to_string() == search_state.term.as_ref() || {
                    proc.name
                        .to_ascii_lowercase()
                        .contains(search_state.term.as_ref())
                        && search_state.term.len() > 0
                }
            })
            .collect::<Vec<_>>();
        let search_matches = (0..process_rows.len())
            .map(|i| matches.iter().any(|&(j, _)| j == i))
            .collect::<BitVec>();

        let result = match matches.len() {
            0 => None,
            len => Some(matches[(search_state.idx.rem_euclid(len as isize)) as usize].0),
        };

        match self.mode {
            Mode::Normal => State {
                process_table_state: self.process_table_state.with_selected(result),
                old_search_state: Some(search_state),
                search_matches,
                ..self
            },
            Mode::Search(_) => State {
                process_table_state: self.process_table_state.with_selected(result),
                mode: Mode::Search(search_state),
                search_matches,
                ..self
            },
        }
    }
    pub fn refresh_search(self) -> State {
        if let Some(search_state) = self.old_search_state.clone() {
            self.update_search(search_state)
        } else {
            self
        }
    }
    pub fn hook_search(mut self) -> State {
        if let Some(search_state) = &mut self.old_search_state {
            search_state.hooked = true;
        }
        self
    }
    pub fn unhook_search(mut self) -> State {
        if let Some(search_state) = &mut self.old_search_state {
            search_state.hooked = false;
        }
        self
    }
    pub fn refresh_search_if_hooked(self) -> State {
        match (self.mode.clone(), self.old_search_state.clone()) {
            (Mode::Normal, Some(search_state)) | (Mode::Search(search_state), _)
                if search_state.hooked =>
            {
                self.update_search(search_state)
            }
            _ => self,
        }
    }
    pub fn prepare(mut self) -> State {
        match self.process_view {
            ProcessView::Tree => {
                self.sysinfo.process_tree.deep_sort_by(|a, b| -> Ordering {
                    match (a.value(), b.value()) {
                        (ProcessNode::Root, ProcessNode::Root) => {
                            unreachable!("only one root for the tree")
                        }
                        (ProcessNode::Root, ProcessNode::Process(_)) => Ordering::Less,
                        (ProcessNode::Process(_), ProcessNode::Root) => Ordering::Greater,
                        (ProcessNode::Process(a), ProcessNode::Process(b)) => {
                            if let Some((a, b)) = self
                                .sysinfo
                                .process_data
                                .get(a)
                                .zip(self.sysinfo.process_data.get(b))
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
            .sysinfo
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
                ProcessNode::Process(proc) => Some((
                    depth,
                    prefix,
                    self.sysinfo.process_data.get(proc).unwrap().clone(),
                )),
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
pub enum Mode {
    #[default]
    Normal,
    Search(SearchState),
}

#[derive(Debug, Clone)]
pub struct SearchState {
    pub(crate) term: Arc<str>,
    pub(crate) idx: isize,
    pub(crate) hooked: bool,
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
pub enum ProcessView {
    Tree,
    #[default]
    Flat,
}

impl ProcessView {
    pub fn make_row(
        &self,
        idx: usize,
        depth: usize,
        prefix: &TreePrefix,
        proc: &ProcessData,
        search_matches: &BitVec,
    ) -> Row<'static> {
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
                    proc.name
                )),
            ]),
            ProcessView::Flat => Row::new([
                Cell::new(proc.pid.to_string()),
                Cell::new(proc.user.to_string()).style(grey_out(proc.user.as_ref() == "root")),
                Cell::new(format!("{:.2}%", proc.cpu_usage)).style(grey_out(proc.cpu_usage < 0.01)),
                Cell::new(ByteSize::b(proc.memory_usage).display().iec().to_string())
                    .style(grey_out(proc.memory_usage == 0)),
                Cell::new(format!("{:.2}%", proc.memory_percent))
                    .style(grey_out(proc.memory_percent < 0.01)),
                Cell::new(format!("{}", proc.name)).style(
                    if search_matches.get(idx).unwrap_or(false) {
                        Style::new().fg(Color::Green)
                    } else {
                        Style::new()
                    },
                ),
            ]),
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub enum TreePrefix {
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

#[derive(Debug, strum::Display)]
pub enum Transition {
    None,
    UpdateDt(Duration),
    #[strum(to_string = "UpdateProcess")]
    UpdateSysInfo(SystemInformation),
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
pub enum SearchTypeTransition {
    Type(char),
    Delete,
}

impl Transition {
    pub fn transition(self, state: State) -> State {
        match (state.mode.clone(), self) {
            (_, Transition::None) => state,
            (_, Transition::Exit) => State {
                exit: true,
                ..state
            },
            (_, Transition::UpdateDt(dt)) => State { dt, ..state },
            (_, Transition::UpdateSysInfo(sysinfo)) => {
                let new_state = State { sysinfo, ..state };
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
            (_, Transition::KillProcess(pid)) => {
                let mut state = state.refresh_search();
                state.last_killed_pid = Some(pid);
                state.effects.kill_effect.0.start();
                state.unhook_search()
            }
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
                let state = state.update_search(search_state);
                let state = state.hook_search();
                state
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

#[derive(Debug, Clone)]
pub struct TuiEffect {
    pub effect: Effect,
    started: bool,
    finished: bool,
}

impl TuiEffect {
    pub fn new(effect: Effect) -> Self {
        Self {
            effect,
            started: false,
            finished: false,
        }
    }
    pub fn started(&self) -> bool {
        self.started
    }
    pub fn finished(&self) -> bool {
        self.finished
    }
    pub fn update(&mut self, buf: &mut Buffer, area: Rect, dt: Duration) {
        if !self.started() {
            return;
        }
        if self.finished() {
            self.started = false;
            return;
        }
        buf.render_effect(&mut self.effect, area, dt);
        if self.started() {
            self.finished = !self.effect.running();
        }
    }
    pub fn start(&mut self) -> &mut Self {
        self.started = true;
        self.effect.reset();
        tracing::info!("started effect");
        self
    }
    pub fn stop(&mut self) -> &mut Self {
        self.started = false;
        tracing::info!("stopped effect");
        self
    }
    pub fn running(&self) -> bool {
        self.started() && !self.finished()
    }
    pub fn set_running(&mut self, value: bool) -> &mut Self {
        match (self.finished(), value) {
            (true, true) => self.start(),
            (false, true) => {
                self.started = true;
                self
            }
            (true, false) => self,
            (false, false) => self.stop(),
        }
    }
}

impl From<Effect> for TuiEffect {
    fn from(effect: Effect) -> Self {
        Self::new(effect)
    }
}

#[derive(Debug, Clone)]
pub struct TableSlideInEffect(pub TuiEffect);

impl Default for TableSlideInEffect {
    fn default() -> Self {
        let effect = fx::slide_in(
            tachyonfx::Motion::UpToDown,
            12,
            0,
            Color::Reset,
            EffectTimer::from_ms(500, tachyonfx::Interpolation::CubicInOut),
        )
        .into();
        Self(effect)
    }
}

#[derive(Debug, Clone)]
pub struct ModelineSlideInLeftEffect(pub TuiEffect);

impl Default for ModelineSlideInLeftEffect {
    fn default() -> Self {
        let effect = fx::slide_in(
            tachyonfx::Motion::RightToLeft,
            12,
            0,
            Color::White,
            EffectTimer::from_ms(600, tachyonfx::Interpolation::CubicInOut),
        )
        .into();
        Self(effect)
    }
}

#[derive(Debug, Clone)]
pub struct ModelineSlideInRightEffect(pub TuiEffect);

impl Default for ModelineSlideInRightEffect {
    fn default() -> Self {
        let effect = fx::slide_in(
            tachyonfx::Motion::LeftToRight,
            12,
            0,
            Color::White,
            EffectTimer::from_ms(600, tachyonfx::Interpolation::CubicInOut),
        )
        .into();
        Self(effect)
    }
}

#[derive(Debug, Clone)]
pub struct KillEffect(pub TuiEffect);

impl Default for KillEffect {
    fn default() -> Self {
        let effect = fx::slide_in(
            tachyonfx::Motion::LeftToRight,
            12,
            0,
            Color::White,
            EffectTimer::from_ms(600, tachyonfx::Interpolation::CubicInOut),
        )
        .into();
        Self(effect)
    }
}

#[derive(Debug, Clone)]
pub struct HighUsagePulseEffect(pub TuiEffect);

impl Default for HighUsagePulseEffect {
    fn default() -> Self {
        let timer = EffectTimer::from_ms(5000, Interpolation::Linear);
        let effect = fx::sequence(&[
            fx::fade_from_fg(Color::Indexed(3), timer.clone()),
            fx::fade_to_fg(Color::Indexed(3), timer),
        ]);
        let effect = fx::repeating(effect);
        HighUsagePulseEffect(effect.into())
    }
}

#[derive(Debug, Clone, Default)]
pub struct TuiEffects {
    pub table_slide_in: TableSlideInEffect,
    pub modeline_slide_in_left: ModelineSlideInLeftEffect,
    pub modeline_slide_in_right: ModelineSlideInRightEffect,
    pub kill_effect: KillEffect,
}

impl TuiEffects {
    pub fn running(&self) -> bool {
        let effects = match &self {
            TuiEffects {
                table_slide_in,
                modeline_slide_in_left,
                modeline_slide_in_right,
                kill_effect,
            } => [
                &table_slide_in.0,
                &modeline_slide_in_left.0,
                &modeline_slide_in_right.0,
                &kill_effect.0,
            ],
        };
        effects.iter().any(|effect| effect.running())
    }
}
