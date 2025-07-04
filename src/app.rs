pub mod state;
mod tests;

use crossterm::event::{Event, KeyCode, KeyModifiers};
use ratatui::{
    prelude::*,
    widgets::{Paragraph, Row, Table},
};
use std::{borrow::Cow, collections::HashMap, sync::Arc, time::Duration};
use tui_textarea::TextArea;

use ratatui::{DefaultTerminal, widgets::Widget};
use smol::{Timer, channel::Sender, lock::Mutex, stream::StreamExt};
use sysinfo::{Pid, System, Users};

use crate::app::state::Mode;
use crate::app::state::SearchTypeTransition;
use crate::app::state::State;
use crate::{
    app::state::Transition,
    default,
    process::{ProcessData, ProcessTree},
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

        let term_height = crossterm::terminal::size().map(|(_, r)| r).unwrap_or(64) as usize;
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
                state
                    .process_view
                    .make_row(idx, *depth, prefix, proc, &state.search_matches)
            });
        let search_hooked = match (&state.mode, &state.old_search_state) {
            (Mode::Search(state), _) | (Mode::Normal, Some(state)) => state.hooked,
            _ => false,
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
        .row_highlight_style(if !search_hooked {
            Style::new().bg(Color::Cyan).fg(Color::Black)
        } else {
            Style::new().bg(Color::Yellow).fg(Color::Black)
        });

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

pub fn grey_out(condition: bool) -> Style {
    match condition {
        true => Style::new().dark_gray(),
        false => Style::new(),
    }
}
