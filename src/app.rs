use std::{cmp::Ordering, time::Duration};

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
use sysinfo::{CpuRefreshKind, Pid, System};

use crate::process::{ProcessData, ProcessNode, ProcessTree, SkipPlaceholderRoot};

#[derive(Debug, Default)]
pub struct App {
    exit: bool,
    system_information: Option<SystemInformation>,
    table_state: TableState,
}

#[derive(Debug, Clone)]
struct ProcessBundle {
    data: ProcessData,
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
        let cpu_count = system.cpus().len();
        loop {
            system.refresh_all();
            let processes = system.processes();
            let ptree = ProcessTree::try_new()
                .proc(processes)
                .cpu_count(cpu_count)
                .call()?;
            tx.send(SystemInformation { processes: ptree }).await?;
            smol::Timer::after(Duration::from_millis(1000)).await;
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
}

impl Widget for &mut App {
    fn render(self, area: Rect, buf: &mut Buffer)
    where
        Self: Sized,
    {
        let Some(ref system_information) = self.system_information else {
            tracing::warn!("no system information");
            return;
        };
        let rows = system_information
            .processes
            .0
            .root()
            .traverse()
            .flat_map(|edge| match edge {
                ego_tree::iter::Edge::Open(node_ref) => Some(node_ref),
                ego_tree::iter::Edge::Close(_) => None,
            })
            .skip_placeholder_root()
            .map(|proc| {
                Row::new([
                    Cell::new(proc.pid.to_string()),
                    Cell::new(format!("{}%", proc.cpu_usage)),
                    Cell::new(ByteSize::b(proc.memory_usage).display().iec().to_string()),
                    Cell::new(proc.name.to_string_lossy()),
                ])
            })
            .collect::<Vec<_>>();
        let table = Table::new(
            rows,
            [
                Constraint::Min(20),
                Constraint::Min(20),
                Constraint::Min(20),
                Constraint::Min(20),
            ],
        );
        StatefulWidget::render(table, area, buf, &mut self.table_state);
    }
}
