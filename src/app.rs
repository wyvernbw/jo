pub mod state;
mod tests;

use bon::{Builder, bon, builder};
use chrono::format::DelayedFormat;
use color_eyre::{SectionExt, owo_colors::OwoColorize};
use crossterm::event::{Event, KeyCode, KeyModifiers};
use notify::{RecursiveMode, Watcher};
use ratatui::{
    layout::Offset,
    prelude::*,
    style::Styled,
    widgets::{Paragraph, Row, Table},
};
use serde::{Deserialize, Serialize};
use std::{
    borrow::Cow,
    collections::HashMap,
    fmt::Display,
    ops::Add,
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
use sysinfo::{
    CpuRefreshKind, MemoryRefreshKind, Pid, ProcessRefreshKind, RefreshKind, System, Users,
};

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
    #[serde(default)]
    animations: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Theme {
    name: Arc<str>,
    header_color: Color,
    highlight_color: Color,
}

impl Default for Theme {
    fn default() -> Self {
        Theme {
            name: "default".into(),
            header_color: Color::Green,
            highlight_color: Color::White,
        }
    }
}

#[derive(Debug, Clone)]
pub struct App {
    poll_rate: Duration,
    themes: HashMap<Arc<str>, Arc<Theme>>,
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
                    .with_memory(MemoryRefreshKind::nothing().with_ram())
                    .with_cpu(CpuRefreshKind::nothing().with_cpu_usage())
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
            let mem = system.used_memory() as f16 / system.total_memory() as f16;
            let cpu_usage = system
                .cpus()
                .iter()
                .map(|cpu| cpu.cpu_usage() as f16 / 100.0)
                .collect();
            drop(system);
            drop(users);
            if let Ok(proc) = proc {
                tx.send(AppEvent::UpdateProcess(
                    SystemInformation::builder()
                        .process_tree(proc)
                        .process_data(data)
                        .mem_usage(mem)
                        .cpu_usage(cpu_usage)
                        .build(),
                ))
                .await?;
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
                    themes.insert(theme.name.clone(), Arc::new(theme));
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
        self = self.load_themes().await?;
        self = self.load_config().await?;
        tracing::info!("loaded config");
        tracing::info!(?self.themes);
        tracing::info!(?self.config.theme);

        let mut state = State::default();
        let event_channel = smol::channel::unbounded();
        let shader_event_channel = smol::channel::unbounded();
        let _handle_1 = smol::spawn(self.clone().fetch_sysinfo_task(event_channel.0.clone()));
        let _handle_2 = smol::spawn(Self::input_task(event_channel.0.clone()));
        let _handle_3 = smol::spawn(Self::config_hot_reload_task(event_channel.0.clone()));
        let _handle_4 = if self.config.animations {
            Some(smol::spawn(Self::effects_task(
                event_channel.0.clone(),
                shader_event_channel.1,
            )))
        } else {
            None
        };

        state.effects.table_slide_in.0.start();
        state.effects.modeline_slide_in_left.0.start();
        state.effects.modeline_slide_in_right.0.start();

        state.process_table_state.select_first();

        let default_theme = Theme::default();
        let mut current_theme = self
            .themes
            .get(&self.config.theme)
            .cloned()
            .unwrap_or_else(|| Arc::new(default_theme));

        loop {
            if state.effects.running() && self.config.animations {
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
                if let Some(theme) = self.themes.get(&self.config.theme) {
                    current_theme = theme.clone();
                } else {
                    tracing::warn!(?self.config.theme, "theme not found.")
                }
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
                    .theme(&current_theme)
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
    fn draw(
        self: &App,
        frame: &mut Frame<'_>,
        state: &mut State,
        process_effects: bool,
        theme: &Theme,
    ) {
        // frame.render_stateful_widget(self, frame.area(), state)
        let area = frame.area();

        let layout = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Max(10), Constraint::Fill(6)])
            .split(area);

        let bottom_layout = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(10), Constraint::Length(1)])
            .spacing(1)
            .split(layout[1]);

        let modeline_layout = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Min(8), Constraint::Min(8)])
            .split(bottom_layout[1]);

        let search_layout = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Max(8), Constraint::Min(1)])
            .split(modeline_layout[0]);

        let cpu_count = state.sysinfo.cpu_usage.len();
        let cpu_column_width = 32;
        let cpu_margin = 1;
        let cpu_per_column = layout[0].height.saturating_sub(cpu_margin * 2).min(8) as usize;
        let cpu_columns = cpu_count / cpu_per_column
            + if cpu_count % cpu_per_column != 0 {
                1
            } else {
                0
            };
        let top_layout = Layout::default()
            .direction(Direction::Horizontal)
            .constraints(vec![Constraint::Max(cpu_column_width); cpu_columns])
            .spacing(2)
            .margin(cpu_margin)
            .split(layout[0]);
        for ((col_idx, cpu_usage), col_rect) in state
            .sysinfo
            .cpu_usage
            .chunks(cpu_per_column)
            .enumerate()
            .zip(top_layout.iter())
        {
            let lines = cpu_usage.iter().enumerate().map(|(idx, cpu)| {
                let max_ch = col_rect.width as usize - 2 - 3;
                let percentage = cpu * 100.0;
                let percentage = format!("{percentage:.2}%");
                let ch_count = cpu.clamp(0.0, 1.0) * max_ch as f16;
                let ch_count = (ch_count as usize).clamp(0, max_ch - percentage.len());
                let empty_count = max_ch
                    .saturating_sub(ch_count)
                    .saturating_sub(percentage.len());
                let cpu_idx = idx + col_idx * cpu_per_column;
                let line = Line::from(vec![
                    Span::from(format!("{cpu_idx:>2} ")),
                    Span::from("[").style(Style::new().bold()),
                    Span::from(format!(
                        "{}{}",
                        "|".repeat(ch_count),
                        " ".repeat(empty_count)
                    ))
                    .style(Style::new().red()),
                    Span::from(percentage).style(Style::new().bold().fg(match *cpu {
                        0.0..0.25 => Color::DarkGray,
                        0.25..0.5 => Color::Green,
                        0.5..0.75 => Color::Red,
                        0.75.. => Color::White,
                        _ => Color::DarkGray,
                    })),
                    Span::from("]").style(Style::new().bold()),
                ]);
                let mut percentage_area = col_rect.clone().offset(Offset {
                    x: 0,
                    y: idx as i32,
                });
                percentage_area.height = 1;
                line
            });
            let text = Text::from_iter(lines);
            frame.render_widget(text, *col_rect);
        }

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
                Some((depth, prefix)).zip(state.sysinfo.process_data.get(&proc))
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
            (false, false) => Style::new().bg(theme.highlight_color).black(),
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
            Row::new([" PID", "USER", "CPU%", "MEM(B)", "MEM%", "Command"])
                .style(Style::new().bg(theme.header_color).fg(Color::Indexed(0))),
        )
        .style(Style::new().bg(Color::Reset))
        .row_highlight_style(table_highlight_style);

        StatefulWidget::render(
            table,
            bottom_layout[0],
            frame.buffer_mut(),
            &mut state.process_table_state,
        );

        let (selected_pid, selected_name) = match state.process_table_state.selected() {
            Some(idx) => (
                Cow::Owned(format!("{}", proc_data[idx].2.pid)),
                Cow::Owned(format!("{}", proc_data[idx].2.name)),
            ),
            None => (Cow::Borrowed(""), Cow::Borrowed("")),
        };

        match &state.mode {
            Mode::Normal => {
                Line::from(vec![
                    Span::from(format!(" {}", state.mode)),
                    " @ ".into(),
                    Span::from(selected_pid).set_style(if search_hooked {
                        Style::new().bold()
                    } else {
                        Style::new()
                    }),
                ])
                .render(modeline_layout[0], frame.buffer_mut());
            }
            Mode::Search(search_state) => {
                Paragraph::new("SEARCH: ")
                    .style(Style::new().bg(Color::Black))
                    .render(search_layout[0], frame.buffer_mut());
                frame.set_cursor_position(Position::new(
                    search_layout[0].x + search_state.term.chars().count() as u16 + 8,
                    search_layout[0].y + 1,
                ));

                let text_area = Paragraph::new(search_state.term.as_ref())
                    .style(Style::new().add_modifier(Modifier::RAPID_BLINK));
                // text_area.move_cursor(tui_textarea::CursorMove::End);
                text_area.render(search_layout[1], frame.buffer_mut());
            }
        };

        Paragraph::new(selected_name)
            .style(Style::new().bg(Color::Black))
            .right_aligned()
            .render(modeline_layout[1], frame.buffer_mut());

        if process_effects {
            let inner_table_layout = bottom_layout[0].offset(Offset { x: 0, y: 1 });
            state
                .effects
                .table_slide_in
                .0
                .update(frame.buffer_mut(), inner_table_layout, state.dt);
            state.effects.modeline_slide_in_left.0.update(
                frame.buffer_mut(),
                modeline_layout[0],
                state.dt,
            );
            state.effects.modeline_slide_in_right.0.update(
                frame.buffer_mut(),
                modeline_layout[1],
                state.dt,
            );

            let mut kill_area = inner_table_layout.offset(Offset {
                x: 0,
                y: (selected_idx - state.process_table_state.offset()) as i32,
            });
            kill_area.height = 1;
            state
                .effects
                .kill_effect
                .0
                .update(frame.buffer_mut(), kill_area, state.dt);
        }
    }
}

#[derive(Debug)]
enum AppEvent {
    ReloadConfig,
    UpdateProcess(SystemInformation),
    Input(Event),
    ProcessEffects(FxDuration),
}

#[derive(Debug, Builder, Default)]
pub struct SystemInformation {
    pub process_tree: ProcessTree,
    pub process_data: HashMap<Pid, ProcessData>,
    pub mem_usage: f16,
    pub cpu_usage: Vec<f16>,
}

impl Display for AppEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AppEvent::ReloadConfig => write!(f, "ReloadConfig"),
            AppEvent::UpdateProcess(_) => write!(f, "UpdateProcess"),
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
            (_, AppEvent::UpdateProcess(system_information)) => {
                Transition::UpdateSysInfo(system_information)
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
