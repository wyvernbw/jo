use std::{borrow::Cow, collections::HashMap, time::Duration};

use color_eyre::eyre::eyre;
use flume::{Receiver, Sender};
use ratatui::{
    DefaultTerminal,
    crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind},
    prelude::*,
    widgets::{Cell, Paragraph, Row, Table, TableState},
};
use sysinfo::{Pid, Process, System};

use crate::process::ProcessData;

#[derive(Debug, Default)]
pub struct App {
    exit: bool,
    system_information: SystemInformation,
    table_state: TableState,
}

#[derive(Debug, Default)]
struct SystemInformation {
    processes: Vec<(Pid, ProcessData)>,
}

impl App {
    /// runs the application's main loop until the user quits
    pub fn run(&mut self, terminal: &mut DefaultTerminal) -> color_eyre::Result<()> {
        let (tx, rx) = flume::bounded(32);
        std::thread::spawn(|| Self::sysinfo_thread(tx));
        while !self.exit {
            self.receive_system_information(&rx)?;
            terminal.draw(|frame| self.draw(frame))?;
            self.handle_events()?;
        }
        Ok(())
    }

    fn draw(&mut self, frame: &mut Frame) {
        frame.render_widget(self, frame.area());
    }

    fn handle_events(&mut self) -> color_eyre::Result<()> {
        if !event::poll(Duration::from_millis(50))? {
            return Ok(());
        }
        match event::read()? {
            // it's important to check that the event is a key press event as
            // crossterm also emits key release and repeat events on Windows.
            Event::Key(key_event) if key_event.kind == KeyEventKind::Press => {
                self.handle_key_event(&key_event)?;
            }
            _ => {}
        };
        Ok(())
    }

    fn handle_key_event(&mut self, key_event: &KeyEvent) -> color_eyre::Result<()> {
        match key_event {
            KeyEvent {
                code: KeyCode::Char('q'),
                ..
            } => self.exit = true,
            _ => {}
        };
        Ok(())
    }

    fn sysinfo_thread(tx: Sender<SystemInformation>) -> color_eyre::Result<()> {
        let mut system = System::new();
        loop {
            system.refresh_all();
            let mut processes = system
                .processes()
                .iter()
                .map(|(pid, process)| (pid.clone(), process.into()))
                .collect::<Vec<(Pid, ProcessData)>>();
            processes.sort_by(|(_, a), (_, b)| a.name.cmp(&b.name));
            let processes = processes.into_iter().collect();
            tx.send(SystemInformation { processes })?;
            std::thread::sleep(Duration::from_millis(500));
        }
    }

    fn receive_system_information(
        &mut self,
        rx: &Receiver<SystemInformation>,
    ) -> color_eyre::Result<()> {
        match rx.try_recv() {
            Ok(res) => {
                self.system_information = res;
                Ok(())
            }
            Err(flume::TryRecvError::Empty) => Ok(()),
            _ => Err(eyre!("system information receiver disconnected.")),
        }
    }
}

impl Widget for &mut App {
    fn render(self, area: Rect, buf: &mut Buffer)
    where
        Self: Sized,
    {
        let rows = self
            .system_information
            .processes
            .iter()
            .map(|(pid, process)| {
                Row::new([
                    Cell::new(pid.to_string()),
                    Cell::new(process.name.to_string_lossy()),
                ])
            })
            .collect::<Vec<_>>();
        let table = Table::new(rows, [Constraint::Min(20), Constraint::Min(20)]);
        StatefulWidget::render(table, area, buf, &mut self.table_state);
    }
}
