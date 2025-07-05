pub mod state;
mod tests;

use color_eyre::SectionExt;
use crossterm::event::{Event, KeyCode, KeyModifiers};
use notify::{RecursiveMode, Watcher};
use ratatui::{
    layout::Offset,
    prelude::*,
    widgets::{Paragraph, Row, Table},
};
use serde::{Deserialize, Serialize};
use std::{
    borrow::Cow,
    collections::HashMap,
    fmt::Display,
    sync::{Arc, mpsc},
    time::{Duration, Instant},
};
use tachyonfx::{Duration as FxDuration, EffectRenderer};

use ratatui::{DefaultTerminal, widgets::Widget};
use smol::{
    Timer,
    channel::{Receiver, Sender},
    fs::DirEntry,
    io::AsyncReadExt,
    lock::Mutex,
    stream::StreamExt,
};
use sysinfo::{MemoryRefreshKind, Pid, ProcessRefreshKind, RefreshKind, System, Users};

use crate::{CONFIG_DIR, app::state::SearchTypeTransition};
use crate::{CONFIG_PATH, app::state::Mode};
use crate::{THEMES_DIR, app::state::State};
use crate::{
    app::state::Transition,
    default,
    process::{ProcessData, ProcessTree},
};

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Config {
    theme: Arc<str>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Theme {
    name: Arc<str>,
    header_color: Color,
    highlight_color: Color,
}

#[derive(Debug, Clone)]
pub struct App {
    poll_rate: Duration,
    themes: HashMap<Arc<str>, Theme>,
    config: Config,
    system: Arc<Mutex<System>>,
    users: Arc<Mutex<Users>>,
}

impl Default for App {
    fn default() -> Self {
        App {
            themes: default(),
            config: default(),
            system: default(),
            users: default(),
            poll_rate: Duration::from_millis(1500),
        }
    }
}

#[bon::bon]
impl App {
    async fn config_hot_reload_task(tx: Sender<AppEvent>) -> color_eyre::Result<()> {
        let event_channel = mpsc::channel::<notify::Result<notify::Event>>();
        let mut watcher = notify::recommended_watcher(event_channel.0)?;

        // Add a path to be watched. All files and directories at that path and
        // below will be monitored for changes.
        watcher.watch(&CONFIG_DIR, RecursiveMode::Recursive)?;
        // Block forever, printing out events as they come in
        loop {
            if let Ok(Ok(event)) = event_channel.1.try_recv() {
                match event.kind {
                    notify::EventKind::Create(_) | notify::EventKind::Modify(_) => {
                        tx.send(AppEvent::ReloadConfig).await?;
                    }
                    _ => {}
                }
            }
            Timer::after(Duration::from_millis(1000)).await;
        }
    }
    async fn fetch_sysinfo_task(self: Self, tx: Sender<AppEvent>) -> color_eyre::Result<()> {
        loop {
            let mut system = self.system.lock().await;
            let mut users = self.users.lock().await;
            users.refresh();
            system.refresh_specifics(
                RefreshKind::nothing()
                    .with_memory(MemoryRefreshKind::nothing().with_ram()) // if you need it
                    .with_processes(
                        ProcessRefreshKind::nothing()
                            .with_cpu()
                            .with_user(sysinfo::UpdateKind::Always)
                            .with_memory()
                            .with_cmd(sysinfo::UpdateKind::Always)
                            .with_tasks(),
                    ),
            );
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

    pub async fn load_config(self) -> color_eyre::Result<Self> {
        let Some(config_path) = &*CONFIG_PATH else {
            return Ok(self);
        };
        let mut file = smol::fs::File::open(config_path.as_path()).await?;
        let mut config = String::default();
        file.read_to_string(&mut config).await?;
        let config: Config = ron::de::from_str(&config)?;
        Ok(Self { config, ..self })
    }

    pub async fn load_themes(self) -> color_eyre::Result<Self> {
        async fn process_theme_file(
            entry: smol::io::Result<DirEntry>,
        ) -> color_eyre::Result<Option<Theme>> {
            let entry = entry?;
            let file_type = entry.file_type().await?;
            if file_type.is_dir() {
                return Ok(None);
            }
            if !entry.file_name().to_string_lossy().ends_with(".ron") {
                return Ok(None);
            }
            let mut file = smol::fs::File::open(entry.file_name()).await?;
            let mut theme = String::default();
            file.read_to_string(&mut theme).await?;
            let theme: Theme = ron::de::from_str(&theme)?;
            Ok(Some(theme))
        }
        let Some(themes_dir) = &*THEMES_DIR else {
            return Ok(self);
        };
        let mut dir = smol::fs::read_dir(&themes_dir).await?;
        let mut themes = HashMap::new();
        while let Some(res) = dir.next().await {
            match process_theme_file(res).await {
                Ok(Some(theme)) => {
                    themes.insert(theme.name.clone(), theme);
                }
                Ok(None) => {}
                Err(err) => {
                    tracing::error!(%err, "error processing theme");
                }
            }
        }
        Ok(Self { themes, ..self })
    }

    pub async fn run(mut self, term: &mut DefaultTerminal) -> color_eyre::Result<()> {
        let mut state = State::default();
        let event_channel = smol::channel::unbounded();
        let shader_event_channel = smol::channel::unbounded();
        let _handle_1 = smol::spawn(self.clone().fetch_sysinfo_task(event_channel.0.clone()));
        let _handle_2 = smol::spawn(Self::input_task(event_channel.0.clone()));
        let _handle_3 = smol::spawn(Self::config_hot_reload_task(event_channel.0.clone()));
        let _handle_4 = smol::spawn(Self::effects_task(
            event_channel.0.clone(),
            shader_event_channel.1,
        ));

        state.effects.table_slide_in.0.start();
        state.effects.modeline_slide_in_left.0.start();
        state.effects.modeline_slide_in_right.0.start();

        state.process_table_state.select_first();
        loop {
            if state.effects.running() {
                shader_event_channel.0.send(()).await?;
            }
            let event = event_channel.1.recv().await?;
            tracing::info!(%event);
            if let AppEvent::ReloadConfig = event {
                self = self.load_themes().await?;
                self = self.load_config().await?;
                tracing::info!("reloaded config");
                tracing::info!(?self.themes);
                tracing::info!(?self.config.theme);
            }
            let process_effects = matches!(event, AppEvent::ProcessEffects(_));
            let transition = event.into_transition(&state);
            tracing::info!(%transition);

            self.run_side_effects(&transition).await;
            state = transition.transition(state);
            state = state.prepare();
            if state.exit {
                break Ok(());
            }
            term.draw(|frame| {
                self.draw()
                    .frame(frame)
                    .state(&mut state)
                    .process_effects(process_effects)
                    .call()
            })?;
        }
    }

    async fn effects_task(tx: Sender<AppEvent>, rx: Receiver<()>) -> color_eyre::Result<()> {
        let mut now;
        let fps = Duration::from_secs_f64(1.0 / 60.0);
        loop {
            let _ = rx.recv().await?;
            now = Instant::now();
            tx.send(AppEvent::ProcessEffects(fps.into())).await?;
            let elapsed = now.elapsed();
            if elapsed < fps {
                Timer::after(fps - elapsed).await;
            }
        }
    }

    #[builder]
    fn draw(self: &App, frame: &mut Frame<'_>, state: &mut State, process_effects: bool) {
        // frame.render_stateful_widget(self, frame.area(), state)
        let area = frame.area();
        let buf = frame.buffer_mut();

        let layout = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(10), Constraint::Length(1)])
            .spacing(1)
            .split(area);

        let modeline_layout = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Min(8), Constraint::Min(8)])
            .split(layout[1]);

        let search_layout = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Max(8), Constraint::Min(1)])
            .split(modeline_layout[0]);

        let selected_idx = state.process_table_state.selected().unwrap_or_default();
        let term_height = crossterm::terminal::size().map(|(_, r)| r).unwrap_or(64) as usize;
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
        let killed_pid = state.last_killed_pid;
        let mut killed_idx = None;
        let rows = proc_data
            .iter()
            .enumerate()
            .map(|(idx, (depth, prefix, proc))| {
                if idx + state.process_table_state.offset() + term_height < selected_idx {
                    return Row::default();
                }
                let row =
                    state
                        .process_view
                        .make_row(idx, *depth, prefix, proc, &state.search_matches);
                if killed_pid == Some(proc.pid) {
                    killed_idx = Some(idx);
                    row.style(Style::new().dark_gray())
                } else {
                    row
                }
            })
            .collect::<Vec<_>>();

        let search_hooked = match (&state.mode, &state.old_search_state) {
            (Mode::Search(state), _) | (Mode::Normal, Some(state)) => state.hooked,
            _ => false,
        };

        let table_highlight_style = match (search_hooked, killed_idx == Some(selected_idx)) {
            (true, false) => Style::new().on_yellow().black(),
            (_, true) => Style::new().on_black().dark_gray(),
            (false, false) => Style::new().on_cyan().black(),
        };

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
        .style(Style::new().bg(Color::Reset))
        .row_highlight_style(table_highlight_style);

        StatefulWidget::render(table, layout[0], buf, &mut state.process_table_state);

        let (selected_pid, selected_name) = match state.process_table_state.selected() {
            Some(idx) => (
                Cow::Owned(format!("@ {}", proc_data[idx].2.pid,)),
                Cow::Owned(format!("{}", proc_data[idx].2.name)),
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
                Paragraph::new(format!(" {}: ", state.mode))
                    .style(Style::new().bg(Color::Black))
                    .render(search_layout[0], buf);

                let text_area = Paragraph::new(search_state.term.as_ref());
                // text_area.move_cursor(tui_textarea::CursorMove::End);
                text_area.render(search_layout[1], buf);
            }
        };

        Paragraph::new(selected_name)
            .style(Style::new().bg(Color::Black))
            .right_aligned()
            .render(modeline_layout[1], buf);

        if process_effects {
            let inner_table_layout = layout[0].offset(Offset { x: 0, y: 1 });
            state
                .effects
                .table_slide_in
                .0
                .update(buf, inner_table_layout, state.dt);
            state
                .effects
                .modeline_slide_in_left
                .0
                .update(buf, modeline_layout[0], state.dt);
            state
                .effects
                .modeline_slide_in_right
                .0
                .update(buf, modeline_layout[1], state.dt);

            let mut kill_area = inner_table_layout.offset(Offset {
                x: 0,
                y: (selected_idx - state.process_table_state.offset()) as i32,
            });
            kill_area.height = 1;
            state.effects.kill_effect.0.update(buf, kill_area, state.dt);
        }
    }
}

#[derive(Debug)]
enum AppEvent {
    ReloadConfig,
    UpdateProcess(ProcessTree, HashMap<Pid, ProcessData>),
    Input(Event),
    ProcessEffects(FxDuration),
}

impl Display for AppEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AppEvent::ReloadConfig => write!(f, "ReloadConfig"),
            AppEvent::UpdateProcess(_, _) => write!(f, "UpdateProcess"),
            AppEvent::Input(event) => write!(f, "Input({:?})", event),
            AppEvent::ProcessEffects(dt) => write!(f, "ProcessEffects({:?})", dt),
        }
    }
}

impl AppEvent {
    fn into_transition(self, state: &State) -> Transition {
        let selected_pid = match (&state.process_rows, state.process_table_state.selected()) {
            (Some(process_rows), Some(idx)) => Some(process_rows[idx].2),
            _ => None,
        };
        match (&state.mode, self) {
            (_, AppEvent::ProcessEffects(dt)) => Transition::UpdateDt(dt),
            (_, AppEvent::ReloadConfig) => Transition::None,
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

pub fn grey_out(condition: bool) -> Style {
    match condition {
        true => Style::new().dark_gray(),
        false => Style::new(),
    }
}
