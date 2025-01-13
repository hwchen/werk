use std::{
    fmt::{Display, Write as _},
    io::Write,
};

use anstream::stream::{AsLockedWrite, IsTerminal};
use indexmap::IndexMap;
use owo_colors::OwoColorize;
use parking_lot::{Mutex, MutexGuard};
use werk_runner::{BuildStatus, Outdatedness, ShellCommandLine, TaskId};

use crate::ColorChoice;

#[derive(Clone, Copy, Debug)]
pub struct OutputSettings {
    /// Logging is enabled, so don't try to modify terminal contents in-place.
    pub logging_enabled: bool,
    pub color: ColorChoice,
    pub print_recipe_commands: bool,
    pub print_fresh: bool,
    pub dry_run: bool,
    pub no_capture: bool,
    pub explain: bool,
}

#[cfg(not(windows))]
pub trait ConWrite: Write + AsLockedWrite {}
#[cfg(not(windows))]
impl<S> ConWrite for S where S: Write + AsLockedWrite {}
#[cfg(windows)]
pub trait ConWrite: Write + AsLockedWrite + anstyle_wincon::WinconStream {}
#[cfg(windows)]
impl<S> ConWrite for S where S: Write + AsLockedWrite + anstyle_wincon::WinconStream {}

/// Similar to `anstream::AutoStream`, but with a predetermined choice.
pub enum AutoStream<S: ConWrite> {
    Passthrough(S),
    Strip(anstream::StripStream<S>),
    #[cfg(windows)]
    Wincon(anstream::WinconStream<S>),
}

impl<S: ConWrite> AutoStream<S> {
    pub fn new(stream: S, kind: AutoStreamKind) -> Self {
        match kind {
            AutoStreamKind::Ansi => AutoStream::Passthrough(stream),
            AutoStreamKind::Strip => AutoStream::Strip(anstream::StripStream::new(stream)),
            #[cfg(windows)]
            AutoStreamKind::Wincon => AutoStream::Wincon(anstream::WinconStream::new(stream)),
        }
    }

    pub fn advanced_rendering(&self) -> bool {
        !matches!(self, AutoStream::Strip(_))
    }
}

impl<S: ConWrite> Write for AutoStream<S> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match self {
            AutoStream::Passthrough(inner) => inner.write(buf),
            AutoStream::Strip(inner) => inner.write(buf),
            #[cfg(windows)]
            AutoStream::Wincon(inner) => inner.write(buf),
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            AutoStream::Passthrough(inner) => inner.flush(),
            AutoStream::Strip(inner) => inner.flush(),
            #[cfg(windows)]
            AutoStream::Wincon(inner) => inner.flush(),
        }
    }

    fn write_vectored(&mut self, bufs: &[std::io::IoSlice<'_>]) -> std::io::Result<usize> {
        match self {
            AutoStream::Passthrough(inner) => inner.write_vectored(bufs),
            AutoStream::Strip(inner) => inner.write_vectored(bufs),
            #[cfg(windows)]
            AutoStream::Wincon(inner) => inner.write_vectored(bufs),
        }
    }

    fn write_all(&mut self, buf: &[u8]) -> std::io::Result<()> {
        match self {
            AutoStream::Passthrough(inner) => inner.write_all(buf),
            AutoStream::Strip(inner) => inner.write_all(buf),
            #[cfg(windows)]
            AutoStream::Wincon(inner) => inner.write_all(buf),
        }
    }

    fn write_fmt(&mut self, fmt: std::fmt::Arguments<'_>) -> std::io::Result<()> {
        match self {
            AutoStream::Passthrough(inner) => inner.write_fmt(fmt),
            AutoStream::Strip(inner) => inner.write_fmt(fmt),
            #[cfg(windows)]
            AutoStream::Wincon(inner) => inner.write_fmt(fmt),
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub enum AutoStreamKind {
    Ansi,
    Strip,
    #[cfg(windows)]
    Wincon,
}

impl AutoStreamKind {
    pub fn detect(choice: ColorChoice) -> Self {
        match choice {
            ColorChoice::Auto => {
                let clicolor_force = anstyle_query::clicolor_force();
                let no_color = anstyle_query::no_color();

                if no_color {
                    return AutoStreamKind::Strip;
                }

                let is_terminal = std::io::stdout().is_terminal();

                let ansi = if clicolor_force || is_terminal {
                    anstyle_query::windows::enable_ansi_colors().unwrap_or(true)
                } else {
                    false
                };
                let term_supports_ansi_color = ansi || anstyle_query::term_supports_ansi_color();

                if term_supports_ansi_color {
                    tracing::info!("Terminal supports ANSI color");
                    AutoStreamKind::Ansi
                } else {
                    tracing::info!("Terminal does not support ANSI color");

                    #[cfg(windows)]
                    {
                        if is_terminal {
                            tracing::info!("Falling back to Wincon backend");
                            return AutoStreamKind::Wincon;
                        }
                    }

                    AutoStreamKind::Strip
                }
            }
            ColorChoice::Always => {
                if let Some(false) = anstyle_query::windows::enable_ansi_colors() {
                    tracing::warn!("Failed to enable virtual terminal processing");
                    return AutoStreamKind::Strip;
                } else {
                    return AutoStreamKind::Ansi;
                }
            }
            ColorChoice::Never => AutoStreamKind::Strip,
        }
    }
}

pub struct StdoutWatcher {
    inner: Mutex<Inner>,
    kind: AutoStreamKind,
    settings: OutputSettings,
}

impl StdoutWatcher {
    pub fn new(settings: OutputSettings) -> Self {
        #[cfg(windows)]
        {
            anstyle_query::windows::enable_ansi_colors();
        }
        let kind = AutoStreamKind::detect(settings.color);

        Self {
            inner: Mutex::new(Inner {
                current_tasks: IndexMap::new(),
                num_tasks: 0,
                num_completed_tasks: 0,
                render_buffer: String::with_capacity(1024),
                width: crossterm::terminal::size().map_or(80, |(w, _)| w as usize),
            }),
            settings,
            kind,
        }
    }

    #[inline]
    pub fn enable_color(&self) -> bool {
        !matches!(self.kind, AutoStreamKind::Strip)
    }

    pub fn lock(&self) -> StdioLock {
        StdioLock {
            inner: self.inner.lock(),
            stdout: AutoStream::new(std::io::stdout().lock(), self.kind),
            settings: &self.settings,
        }
    }
}

struct Inner {
    current_tasks: IndexMap<TaskId, (usize, usize)>,
    num_tasks: usize,
    num_completed_tasks: usize,
    render_buffer: String,
    width: usize,
}

pub struct StdioLock<'a> {
    inner: MutexGuard<'a, Inner>,
    pub stdout: AutoStream<std::io::StdoutLock<'static>>,
    settings: &'a OutputSettings,
}

impl<'a> StdioLock<'a> {
    pub fn start_advanced_rendering(&mut self) {
        if self.stdout.advanced_rendering() {
            crossterm::execute!(
                &mut self.stdout,
                crossterm::cursor::Hide,
                crossterm::terminal::DisableLineWrap
            )
            .unwrap();
        }
    }

    pub fn finish_advanced_rendering(&mut self) {
        if self.stdout.advanced_rendering() {
            crossterm::execute!(
                &mut self.stdout,
                crossterm::cursor::Show,
                crossterm::terminal::EnableLineWrap
            )
            .unwrap();
        }
    }

    fn clear_current_line(&mut self) {
        if self.stdout.advanced_rendering() && !self.settings.logging_enabled {
            crossterm::execute!(
                &mut self.stdout,
                crossterm::cursor::MoveToColumn(0),
                crossterm::terminal::Clear(crossterm::terminal::ClearType::CurrentLine)
            )
            .unwrap();
        }
    }

    fn render(&mut self) {
        if self.stdout.advanced_rendering() && !self.settings.logging_enabled {
            let inner = &mut *self.inner;
            let buffer = &mut inner.render_buffer;
            if inner.current_tasks.is_empty() {
                return;
            }
            buffer.clear();
            _ = write!(
                buffer,
                "{} Building: ",
                Bracketed(Step(inner.num_completed_tasks, inner.num_tasks)).bright_cyan()
            );

            // Write the name of the last task in the map.
            if let Some((last_id, _)) = inner.current_tasks.last() {
                _ = write!(buffer, "{}", last_id);
            }

            if inner.current_tasks.len() > 1 {
                _ = write!(buffer, ", and {} more", inner.current_tasks.len() - 1);
            }

            crossterm::queue!(&mut self.stdout, crossterm::terminal::DisableLineWrap).unwrap();
            self.stdout.write_all(buffer.as_bytes()).unwrap();
            crossterm::queue!(&mut self.stdout, crossterm::terminal::EnableLineWrap).unwrap();

            self.stdout.flush().unwrap();
        }
    }

    fn will_build(&mut self, task_id: &TaskId, num_steps: usize, outdated: &Outdatedness) {
        self.inner
            .current_tasks
            .insert(task_id.clone(), (0, num_steps));
        self.clear_current_line();

        if self.settings.explain && outdated.is_outdated() {
            if let Some(path) = task_id.as_path() {
                _ = writeln!(
                    self.stdout,
                    "{} rebuilding `{path}`",
                    Bracketed(Step(0, num_steps)).bright_yellow(),
                );
            } else {
                _ = writeln!(
                    self.stdout,
                    "{} running task `{}`",
                    Bracketed(Step(0, num_steps)).bright_yellow(),
                    task_id.as_str(),
                );
            };

            for reason in &outdated.reasons {
                _ = writeln!(self.stdout, "  {} {reason}", "Cause:".yellow());
            }
        }

        self.render();
    }

    fn did_build(
        &mut self,
        task_id: &TaskId,
        result: &Result<werk_runner::BuildStatus, werk_runner::Error>,
    ) {
        self.inner
            .current_tasks
            .shift_remove(task_id)
            .unwrap_or_default();

        self.clear_current_line();
        match result {
            Ok(BuildStatus::Complete(_task_id, outdatedness)) => {
                if outdatedness.is_outdated() {
                    _ = writeln!(
                        &mut self.stdout,
                        "{} {task_id}{}",
                        Bracketed(" ok ").bright_green(),
                        if self.settings.dry_run {
                            " (dry-run)"
                        } else {
                            ""
                        }
                    );
                } else if self.settings.print_fresh {
                    _ = writeln!(
                        &mut self.stdout,
                        "{} {task_id}",
                        Bracketed(" -- ").bright_blue()
                    );
                }
            }
            Ok(BuildStatus::Exists(..)) => {
                // Print nothing for file existence checks.
            }
            Err(err) => {
                _ = writeln!(
                    &mut self.stdout,
                    "{} {task_id}\n{err}",
                    Bracketed("ERROR").bright_red()
                );
            }
        }
        self.render();
    }

    fn will_execute(
        &mut self,
        task_id: &TaskId,
        command: &ShellCommandLine,
        step: usize,
        num_steps: usize,
    ) {
        *self
            .inner
            .current_tasks
            .get_mut(task_id)
            .expect("task not registered") = (step + 1, num_steps);
        self.clear_current_line();
        if self.settings.dry_run || self.settings.print_recipe_commands {
            _ = writeln!(
                self.stdout,
                "{} {task_id}: {}",
                Bracketed(Step(step + 1, num_steps)).bright_yellow(),
                command.display()
            );
        }
        self.render();
    }

    fn on_child_process_stdout_line(
        &mut self,
        _task_id: &TaskId,
        _command: &ShellCommandLine,
        line_without_eol: &[u8],
    ) {
        self.clear_current_line();
        _ = self.stdout.write_all(line_without_eol);
        _ = self.stdout.write(&[b'\n']);
        self.render();
    }

    fn on_child_process_stderr_line(
        &mut self,
        _task_id: &TaskId,
        _command: &ShellCommandLine,
        line_without_eol: &[u8],
    ) {
        self.clear_current_line();
        _ = self.stdout.write_all(line_without_eol);
        _ = self.stdout.write(&[b'\n']);
        self.render();
    }

    fn did_execute(
        &mut self,
        task_id: &TaskId,
        command: &ShellCommandLine,
        result: &Result<std::process::ExitStatus, std::io::Error>,
        step: usize,
        num_steps: usize,
    ) {
        match result {
            Ok(status) => {
                if !status.success() {
                    self.clear_current_line();
                    _ = writeln!(
                        self.stdout,
                        "{} Command failed while building '{task_id}': {}",
                        Bracketed(Step(step, num_steps)).red(),
                        command.display(),
                    );
                    self.render();
                }
            }
            Err(err) => {
                self.clear_current_line();
                _ = writeln!(
                    self.stdout,
                    "{} Error evaluating command while building '{task_id}': {}\n{err}",
                    Bracketed(Step(step + 1, num_steps)).red(),
                    command.display(),
                );
                self.render();
            }
        }
    }

    fn message(&mut self, task_id: Option<&TaskId>, message: &str) {
        self.clear_current_line();
        if let Some(task_id) = task_id {
            _ = writeln!(self.stdout, "{} {}", Bracketed(task_id).cyan(), message);
        } else {
            _ = writeln!(self.stdout, "{} {}", "[info]".cyan(), message);
        }
        self.render();
    }

    fn warning(&mut self, task_id: Option<&TaskId>, message: &str) {
        self.clear_current_line();
        if let Some(task_id) = task_id {
            _ = writeln!(self.stdout, "{} {}", Bracketed(task_id).yellow(), message);
        } else {
            _ = writeln!(self.stdout, "{} {}", "[warn]".yellow(), message);
        }
        self.render();
    }
}

impl werk_runner::Watcher for StdoutWatcher {
    fn will_build(&self, task_id: &TaskId, num_steps: usize, outdated: &Outdatedness) {
        self.lock().will_build(task_id, num_steps, outdated);
    }

    fn did_build(
        &self,
        task_id: &TaskId,
        result: &Result<werk_runner::BuildStatus, werk_runner::Error>,
    ) {
        self.lock().did_build(task_id, result);
    }

    fn will_execute(
        &self,
        task_id: &TaskId,
        command: &ShellCommandLine,
        step: usize,
        num_steps: usize,
    ) {
        self.lock().will_execute(task_id, command, step, num_steps);
    }

    fn on_child_process_stdout_line(
        &self,
        task_id: &TaskId,
        command: &ShellCommandLine,
        line_without_eol: &[u8],
        capture: bool,
    ) {
        if !capture || self.settings.no_capture {
            self.lock()
                .on_child_process_stdout_line(task_id, command, line_without_eol);
        }
    }

    fn on_child_process_stderr_line(
        &self,
        task_id: &TaskId,
        command: &ShellCommandLine,
        line_without_eol: &[u8],
    ) {
        self.lock()
            .on_child_process_stderr_line(task_id, command, line_without_eol);
    }

    fn did_execute(
        &self,
        task_id: &TaskId,
        command: &ShellCommandLine,
        result: &Result<std::process::ExitStatus, std::io::Error>,
        step: usize,
        num_steps: usize,
        _print_successful: bool,
    ) {
        self.lock()
            .did_execute(task_id, command, result, step, num_steps);
    }

    fn message(&self, task_id: Option<&TaskId>, message: &str) {
        self.lock().message(task_id, message)
    }

    fn warning(&self, task_id: Option<&TaskId>, message: &str) {
        self.lock().warning(task_id, message)
    }
}

struct Bracketed<T>(pub T);
impl<T: Display> Display for Bracketed<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_char('[')?;
        self.0.fmt(f)?;
        f.write_char(']')
    }
}

struct Step(usize, usize);
impl Display for Step {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", self.0, self.1)
    }
}
