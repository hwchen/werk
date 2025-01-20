use indexmap::IndexMap;
use owo_colors::OwoColorize as _;
use parking_lot::Mutex;
use werk_runner::{BuildStatus, Error, Outdatedness, ShellCommandLine, TaskId};

use std::{fmt::Write as _, io::Write, sync::Arc};

use crate::watcher::Bracketed;

use super::{AutoStream, OutputSettings, Step};

/// A watcher that outputs to the terminal, emitting "destructive" ANSI escape
/// codes that modify the existing terminal (i.e. overwriting the bottom line(s)
/// with current status).
pub struct TerminalWatcher<const LINEAR: bool> {
    inner: Arc<Mutex<Renderer<LINEAR>>>,
    _render_task: Option<smol::Task<()>>,
}

impl<const LINEAR: bool> TerminalWatcher<LINEAR> {
    pub fn new(
        settings: OutputSettings,
        stdout: AutoStream<std::io::Stdout>,
        stderr: AutoStream<std::io::Stderr>,
    ) -> Self {
        let inner = Arc::new(Mutex::new(Renderer {
            stdout,
            stderr,
            state: RenderState {
                current_tasks: IndexMap::new(),
                num_tasks: 0,
                num_completed_tasks: 0,
                render_buffer: String::with_capacity(128),
                spinner_frame: 0,
                last_spinner_tick: std::time::Instant::now(),
                settings,
            },
            needs_clear: false,
        }));

        let render_task = if !LINEAR {
            // Spawn a task that automatically updates the terminal with the current
            // status when a long-running task is present.
            let renderer = Arc::downgrade(&inner);
            Some(smol::spawn(async move {
                loop {
                    smol::Timer::after(std::time::Duration::from_millis(100)).await;
                    let Some(renderer) = renderer.upgrade() else {
                        return;
                    };
                    Renderer::render_now(&*renderer);
                }
            }))
        } else {
            None
        };

        Self {
            inner,
            _render_task: render_task,
        }
    }
}

struct Renderer<const LINEAR: bool> {
    stdout: AutoStream<std::io::Stdout>,
    stderr: AutoStream<std::io::Stderr>,
    state: RenderState<LINEAR>,
    needs_clear: bool,
}

impl<const LINEAR: bool> Renderer<LINEAR> {
    /// Render zero or more lines above the status and re-render the status.
    fn render_lines<F>(&mut self, render: F) -> std::io::Result<()>
    where
        F: FnOnce(&mut dyn Write, &mut RenderState<LINEAR>) -> std::io::Result<()>,
    {
        if LINEAR {
            render(&mut self.stdout, &mut self.state)
        } else {
            if self.needs_clear {
                self.stdout.write_all(b"\x1B[K")?;
                self.needs_clear = false;
            }
            render(&mut self.stdout, &mut self.state)?;
            self.state.render_progress(&mut self.stdout);
            self.needs_clear = true;
            Ok(())
        }
    }

    fn render_lines_stderr<F>(&mut self, render: F) -> std::io::Result<()>
    where
        F: FnOnce(&mut dyn Write) -> std::io::Result<()>,
    {
        if LINEAR {
            render(&mut self.stderr)
        } else {
            if self.needs_clear {
                self.stderr.write_all(b"\x1B[K")?;
                self.needs_clear = false;
            }
            render(&mut self.stderr)?;
            self.state.render_progress(&mut self.stdout);
            self.needs_clear = true;
            Ok(())
        }
    }
}

struct RenderState<const LINEAR: bool> {
    current_tasks: IndexMap<TaskId, (usize, usize)>,
    num_tasks: usize,
    num_completed_tasks: usize,
    render_buffer: String,
    spinner_frame: u64,
    last_spinner_tick: std::time::Instant,
    settings: OutputSettings,
}

impl<const LINEAR: bool> Renderer<LINEAR> {
    pub fn render_now(this: &Mutex<Self>) {
        if !LINEAR {
            let mut this = this.lock();
            _ = this.render_lines(|_, _| Ok(()));
        }
    }
}

impl<const LINEAR: bool> RenderState<LINEAR> {
    pub fn render_progress(&mut self, out: &mut dyn Write) {
        if LINEAR {
            return;
        }

        let now = std::time::Instant::now();
        if now.duration_since(self.last_spinner_tick) > std::time::Duration::from_millis(100) {
            self.spinner_frame += 1;
            self.last_spinner_tick = now;
        }

        const SPINNER_CHARS: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
        let spinner = SPINNER_CHARS[(self.spinner_frame % 10) as usize];

        let buffer = &mut self.render_buffer;
        if self.current_tasks.is_empty() {
            return;
        }
        buffer.clear();
        _ = write!(
            buffer,
            "  {spinner} {} ",
            Bracketed(Step(self.num_completed_tasks, self.num_tasks)).bright_cyan()
        );

        // Write the name of the last task in the map.
        let mut width_written = 20;
        let max_width = 100;

        for (index, (id, _)) in self.current_tasks.iter().enumerate() {
            if width_written > max_width {
                let num_remaining = self.current_tasks.len() - (index + 1);
                if num_remaining > 0 {
                    if index > 0 {
                        _ = write!(buffer, " + {} more", num_remaining);
                    } else {
                        _ = write!(buffer, "{} recipes", num_remaining);
                    }
                }
                break;
            }

            if index != 0 {
                _ = write!(buffer, ", ");
                width_written += 2;
            }

            let short_name = id.short_name();
            buffer.push_str(short_name);
            // Note: Overaccounts for Unicode characters. Probably fine for now.
            width_written += short_name.len();
        }

        // Place the cursor at column 0.
        buffer.push('\r');
        out.write_all(buffer.as_bytes()).unwrap();
        _ = out.flush();
    }
}

impl<const LINEAR: bool> Renderer<LINEAR> {
    pub fn will_build(&mut self, task_id: &TaskId, num_steps: usize, outdatedness: &Outdatedness) {
        self.state
            .current_tasks
            .insert(task_id.clone(), (0, num_steps));
        self.state.num_tasks += 1;

        _ = self.render_lines(|out, state| {
            if state.settings.explain && outdatedness.is_outdated() {
                if let Some(path) = task_id.as_path() {
                    writeln!(
                        out,
                        "{} rebuilding `{path}`",
                        Bracketed(Step(0, num_steps)).bright_yellow().bold(),
                    )?
                } else {
                    writeln!(
                        out,
                        "{} running task `{}`",
                        Bracketed(Step(0, num_steps)).bright_yellow().bold(),
                        task_id.as_str(),
                    )?
                }

                for reason in &outdatedness.reasons {
                    // Use normal writeln because we already wrote at least one line
                    // (so no overwrite needed).
                    _ = writeln!(out, "  {} {reason}", "Cause:".bright_yellow());
                }
            }

            Ok(())
        });
    }

    fn did_build(&mut self, task_id: &TaskId, result: &Result<BuildStatus, Error>) {
        self.state
            .current_tasks
            .shift_remove(task_id)
            .unwrap_or_default();
        self.state.num_completed_tasks += 1;

        _ = self.render_lines(|out, state| {
            match result {
                Ok(BuildStatus::Complete(_task_id, outdatedness)) => {
                    if outdatedness.is_outdated() {
                        writeln!(
                            out,
                            "{} {task_id}{}",
                            Bracketed(" ok ").bright_green().bold(),
                            if state.settings.dry_run {
                                " (dry-run)"
                            } else {
                                ""
                            }
                        )?
                    } else if state.settings.print_fresh {
                        writeln!(out, "{} {task_id}", Bracketed(" -- ").bright_blue())?
                    }
                }
                Ok(BuildStatus::Exists(..)) => {
                    // Print nothing for file existence checks.
                }
                Err(err) => writeln!(
                    out,
                    "{} {task_id}\n{err}",
                    Bracketed("ERROR").bright_red().bold()
                )?,
            }
            Ok(())
        });
    }

    fn will_execute(
        &mut self,
        task_id: &TaskId,
        command: &ShellCommandLine,
        step: usize,
        num_steps: usize,
    ) {
        *self
            .state
            .current_tasks
            .get_mut(task_id)
            .expect("task not registered") = (step + 1, num_steps);

        // Avoid taking the stdout lock if we aren't actually going to render anything.
        let print_something =
            self.state.settings.dry_run || self.state.settings.print_recipe_commands;

        if print_something {
            _ = self.render_lines(|out, _status| {
                write!(
                    out,
                    "{} {task_id}: {}",
                    Bracketed(Step(step + 1, num_steps)).dimmed(),
                    command.display()
                )
            });
        } else if !LINEAR {
            _ = self.render_lines(|_, _| Ok(()));
        }
    }

    fn on_child_process_stdout_line(
        &mut self,
        _task_id: &TaskId,
        _command: &ShellCommandLine,
        line_without_eol: &[u8],
    ) {
        // Forward stdout with line wrapping enabled.
        _ = self.render_lines(|out, _status| {
            out.write_all(line_without_eol)?;
            out.write_all(b"\n")?;
            Ok(())
        });
    }

    fn on_child_process_stderr_line(
        &mut self,
        _task_id: &TaskId,
        _command: &ShellCommandLine,
        line_without_eol: &[u8],
    ) {
        _ = self.render_lines_stderr(|out| {
            out.write_all(line_without_eol)?;
            out.write_all(b"\n")?;
            Ok(())
        });
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
                    _ = self.render_lines(|out, _status| {
                        writeln!(
                            out,
                            "{} Command failed while building '{task_id}': {}",
                            Bracketed(Step(step, num_steps)).bright_red().bold(),
                            command.display(),
                        )
                    });
                }
            }
            Err(err) => {
                _ = self.render_lines(|out, _status| {
                    writeln!(
                        out,
                        "{} Error evaluating command while building '{task_id}': {}\n{err}",
                        Bracketed(Step(step + 1, num_steps)).bright_red().bold(),
                        command.display(),
                    )
                });
            }
        }
    }

    fn message(&mut self, _task_id: Option<&TaskId>, message: &str) {
        _ = self
            .render_lines(|out, _status| write!(out, "{} {}", "[info]".bright_green(), message));
    }

    fn warning(&mut self, _task_id: Option<&TaskId>, message: &str) {
        _ = self
            .render_lines(|out, _status| write!(out, "{} {}", "[warn]".bright_yellow(), message));
    }
}

impl<const LINEAR: bool> werk_runner::Watcher for TerminalWatcher<LINEAR> {
    fn will_build(&self, task_id: &TaskId, num_steps: usize, outdatedness: &Outdatedness) {
        self.inner
            .lock()
            .will_build(task_id, num_steps, outdatedness);
    }

    fn did_build(&self, task_id: &TaskId, result: &Result<BuildStatus, Error>) {
        self.inner.lock().did_build(task_id, result);
    }

    fn will_execute(
        &self,
        task_id: &TaskId,
        command: &ShellCommandLine,
        step: usize,
        num_steps: usize,
    ) {
        self.inner
            .lock()
            .will_execute(task_id, command, step, num_steps);
    }

    fn did_execute(
        &self,
        task_id: &TaskId,
        command: &ShellCommandLine,
        status: &std::io::Result<std::process::ExitStatus>,
        step: usize,
        num_steps: usize,
    ) {
        self.inner
            .lock()
            .did_execute(task_id, command, status, step, num_steps);
    }

    fn message(&self, task_id: Option<&TaskId>, message: &str) {
        self.inner.lock().message(task_id, message)
    }

    fn warning(&self, task_id: Option<&TaskId>, message: &str) {
        self.inner.lock().warning(task_id, message)
    }

    fn on_child_process_stdout_line(
        &self,
        task_id: &TaskId,
        command: &ShellCommandLine,
        line_without_eol: &[u8],
        capture: bool,
    ) {
        if !capture {
            self.inner
                .lock()
                .on_child_process_stdout_line(task_id, command, line_without_eol);
        }
    }

    fn on_child_process_stderr_line(
        &self,
        task_id: &TaskId,
        command: &ShellCommandLine,
        line_without_eol: &[u8],
    ) {
        self.inner
            .lock()
            .on_child_process_stderr_line(task_id, command, line_without_eol);
    }

    fn write_raw_stdout(&self, bytes: &[u8]) -> std::io::Result<()> {
        self.inner.lock().stdout.write_all(bytes)?;
        Ok(())
    }

    fn write_raw_stderr(&self, bytes: &[u8]) -> std::io::Result<()> {
        self.inner.lock().stderr.write_all(bytes)?;
        Ok(())
    }
}
