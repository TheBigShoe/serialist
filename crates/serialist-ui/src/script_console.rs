//! The Script console: the right dock's panel for Lua scripts.
//!
//! From the top: a header with what is running and the Stop, Clear and Folder
//! buttons; the scripts in the config directory's `scripts/` folder (`*.lua` at any
//! depth, relisted when a file there changes, see
//! [`ConfigEvent::Scripts`](serialist_core::ConfigEvent::Scripts)), each with a Run
//! button; the output of every run on the session (printed lines, log lines in their
//! level's color, prompts and their answers, and how each run ended, with the error and
//! its Lua traceback); and a one-line REPL whose text runs as a script, with `=expr`
//! printing `expr`.
//!
//! The panel only shows and asks. Runs belong to the session view (one script thread
//! per session, runs queued one at a time), and the workspace passes the panel's
//! requests on ([`ScriptConsoleEvent`]) and the session's lines back
//! ([`ScriptConsole::push_lines`]).
//!
//! [`ScriptPrompt`] is the small form a script's `ui.prompt` opens in a dialog.

use std::collections::VecDeque;
use std::sync::Arc;

use serialist_script::LogLevel;

use crate::actions::context;
use crate::config::Config;
use crate::prelude::*;
use crate::script_bridge::{ConsoleKind, ConsoleLine};
use crate::script_files::ScriptEntry;
use crate::session_view::SessionView;
use crate::status::ScriptStatus;

const ROW_HEIGHT: Pixels = px(20.);
const SCRIPT_ROW_HEIGHT: Pixels = px(26.);

/// Output lines the console keeps; older ones go first.
pub const MAX_LINES: usize = 5000;

/// What the console asks the workspace to do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ScriptConsoleEvent {
    /// Run the script at this path under the scripts folder.
    Run(String),
    /// Run this REPL line.
    RunInline(String),
    /// Stop the running script.
    Stop,
}

pub struct ScriptConsole {
    scripts: Arc<Vec<ScriptEntry>>,
    lines: VecDeque<ConsoleLine>,
    input: Entity<InputState>,
    output_scroll: UniformListScrollHandle,
    /// The session whose runs the header shows.
    session: Option<WeakEntity<SessionView>>,
    focus_handle: FocusHandle,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<ScriptConsoleEvent> for ScriptConsole {}

impl Focusable for ScriptConsole {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl ScriptConsole {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let input = cx.new(|cx| {
            InputState::new(window, cx).placeholder("Lua, run on Enter  (=expr prints it)")
        });
        let input_events = cx.subscribe_in(&input, window, |this, _, event, window, cx| {
            if let InputEvent::PressEnter { .. } = event {
                this.submit_inline(window, cx);
            }
        });
        let config_changes = cx.observe_global::<Config>(|this, cx| this.refresh(cx));
        let mut console = Self {
            scripts: Arc::default(),
            lines: VecDeque::new(),
            input,
            output_scroll: UniformListScrollHandle::new(),
            session: None,
            focus_handle: cx.focus_handle(),
            _subscriptions: vec![input_events, config_changes],
        };
        console.refresh(cx);
        console
    }

    // --- Reading -----------------------------------------------------------------------

    /// The scripts listed, from the configuration.
    pub fn scripts(&self) -> &[ScriptEntry] {
        &self.scripts
    }

    /// The output lines, oldest first.
    pub fn lines(&self) -> &VecDeque<ConsoleLine> {
        &self.lines
    }

    /// The output lines' text, oldest first.
    pub fn texts(&self) -> Vec<String> {
        self.lines.iter().map(|line| line.text.clone()).collect()
    }

    /// The REPL input.
    pub fn input(&self) -> &Entity<InputState> {
        &self.input
    }

    /// The running script of the session the console shows.
    pub fn status(&self, cx: &App) -> Option<ScriptStatus> {
        self.session.as_ref()?.upgrade()?.read(cx).script_status()
    }

    // --- Changing ----------------------------------------------------------------------

    /// List the configuration's scripts again.
    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        let scripts = cx
            .try_global::<Config>()
            .map(|config| config.scripts().clone())
            .unwrap_or_default();
        if scripts != self.scripts {
            self.scripts = scripts;
            cx.notify();
        }
    }

    /// Show `session`'s runs in the header.
    pub fn set_session(
        &mut self,
        session: Option<WeakEntity<SessionView>>,
        cx: &mut Context<Self>,
    ) {
        self.session = session;
        cx.notify();
    }

    /// Add lines to the output and scroll to them.
    pub fn push_lines(
        &mut self,
        lines: impl IntoIterator<Item = ConsoleLine>,
        cx: &mut Context<Self>,
    ) {
        let before = self.lines.len();
        self.lines.extend(lines);
        if self.lines.len() == before {
            return;
        }
        while self.lines.len() > MAX_LINES {
            self.lines.pop_front();
        }
        self.output_scroll
            .scroll_to_item(self.lines.len() - 1, ScrollStrategy::Bottom);
        cx.notify();
    }

    /// Empty the output.
    pub fn clear(&mut self, cx: &mut Context<Self>) {
        self.lines.clear();
        cx.notify();
    }

    /// Ask for the script at `relative` to run.
    pub fn run(&mut self, relative: &str, cx: &mut Context<Self>) {
        cx.emit(ScriptConsoleEvent::Run(relative.to_owned()));
    }

    /// Run the REPL line, and empty the input. A blank line does nothing.
    pub fn submit_inline(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.input.read(cx).value().to_string();
        if text.trim().is_empty() {
            return;
        }
        self.input
            .update(cx, |input, cx| input.set_value("", window, cx));
        self.push_lines(
            [ConsoleLine::new(
                ConsoleKind::Prompt,
                format!("\u{203a} {text}"),
            )],
            cx,
        );
        cx.emit(ScriptConsoleEvent::RunInline(text));
    }

    /// Ask for the running script to stop.
    pub fn stop(&mut self, cx: &mut Context<Self>) {
        cx.emit(ScriptConsoleEvent::Stop);
    }

    // --- Rendering ---------------------------------------------------------------------

    fn line_color(kind: ConsoleKind, theme: &Theme) -> Hsla {
        match kind {
            ConsoleKind::Output | ConsoleKind::Answer => theme.foreground,
            ConsoleKind::Log(LogLevel::Debug) | ConsoleKind::Info => theme.muted_foreground,
            ConsoleKind::Log(LogLevel::Info) | ConsoleKind::Prompt => theme.info,
            ConsoleKind::Log(LogLevel::Warn) | ConsoleKind::Notice | ConsoleKind::Stopped => {
                theme.warning
            }
            ConsoleKind::Log(LogLevel::Error) | ConsoleKind::Error => theme.danger,
            ConsoleKind::Finished => theme.success,
        }
    }

    fn render_header(&self, status: Option<&ScriptStatus>, cx: &mut Context<Self>) -> Div {
        let theme = cx.theme();
        h_flex()
            .flex_none()
            .justify_between()
            .gap_1()
            .px_3()
            .h(px(32.))
            .text_xs()
            .text_color(theme.muted_foreground)
            .child(
                h_flex()
                    .min_w_0()
                    .gap_2()
                    .child("SCRIPTS")
                    .children(status.map(|status| {
                        div()
                            .truncate()
                            .text_color(theme.info)
                            .child(SharedString::from(status.name.clone()))
                    })),
            )
            .child(
                h_flex()
                    .gap_1()
                    .child(
                        Button::new("script-stop")
                            .label("Stop")
                            .tooltip("Stop the running script")
                            .xsmall()
                            .ghost()
                            .disabled(status.is_none())
                            .on_click(cx.listener(|this, _, _, cx| this.stop(cx))),
                    )
                    .child(
                        Button::new("script-clear")
                            .label("Clear")
                            .tooltip("Clear the output")
                            .xsmall()
                            .ghost()
                            .on_click(cx.listener(|this, _, _, cx| this.clear(cx))),
                    )
                    .child(
                        Button::new("script-folder")
                            .label("Folder")
                            .tooltip("Open the scripts folder (it gets the examples if empty)")
                            .xsmall()
                            .ghost()
                            .on_click(|_, _, cx| crate::actions::open_scripts_folder(cx)),
                    ),
            )
    }

    fn render_scripts(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        if self.scripts.is_empty() {
            return div()
                .px_3()
                .pb_2()
                .text_xs()
                .text_color(theme.muted_foreground)
                .child("No scripts yet. Folder opens the scripts folder and adds two examples.")
                .into_any_element();
        }
        let rows = self.scripts.iter().enumerate().map(|(ix, entry)| {
            let relative = entry.relative.clone();
            h_flex()
                .id(("script-row", ix))
                .w_full()
                .h(SCRIPT_ROW_HEIGHT)
                .flex_none()
                .px_3()
                .gap_1()
                .text_sm()
                .hover(|style| style.bg(theme.list_hover))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .font_family(theme.mono_font_family.clone())
                        .child(SharedString::from(entry.relative.clone())),
                )
                .child(
                    Button::new(("run-script", ix))
                        .label("Run")
                        .tooltip("Run on the session")
                        .xsmall()
                        .ghost()
                        .on_click(cx.listener(move |this, _, _, cx| this.run(&relative, cx))),
                )
        });
        v_flex()
            .id("script-list")
            .flex_none()
            .max_h(px(180.))
            .overflow_y_scroll()
            .pb_1()
            .children(rows)
            .into_any_element()
    }

    fn render_output(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        if self.lines.is_empty() {
            return div()
                .flex_1()
                .px_3()
                .text_xs()
                .text_color(theme.muted_foreground)
                .child("Output of scripts run on the session shows here.")
                .into_any_element();
        }
        uniform_list(
            "script-output",
            self.lines.len(),
            cx.processor(|this, range: std::ops::Range<usize>, _window, cx| {
                let theme = cx.theme();
                range
                    .map(|ix| {
                        let line = &this.lines[ix];
                        let text = SharedString::from(line.text.clone());
                        div()
                            .id(("script-line", ix))
                            .h(ROW_HEIGHT)
                            .px_3()
                            .truncate()
                            .text_color(Self::line_color(line.kind, theme))
                            .child(text.clone())
                            .when(line.text.len() > 48, |row| {
                                row.tooltip(move |window, cx| {
                                    Tooltip::new(text.clone()).build(window, cx)
                                })
                            })
                    })
                    .collect::<Vec<_>>()
            }),
        )
        .track_scroll(&self.output_scroll)
        .flex_1()
        .font_family(theme.mono_font_family.clone())
        .text_xs()
        .into_any_element()
    }
}

impl Render for ScriptConsole {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let status = self.status(cx);
        let header = self.render_header(status.as_ref(), cx);
        let scripts = self.render_scripts(cx);
        let output = self.render_output(cx);
        let theme = cx.theme();
        v_flex()
            .id("script-console")
            .key_context(context::SCRIPT_CONSOLE)
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(theme.sidebar)
            .text_color(theme.sidebar_foreground)
            .border_l_1()
            .border_color(theme.border)
            .child(header)
            .child(scripts)
            .child(div().flex_none().h(px(1.)).w_full().bg(theme.border))
            .child(v_flex().flex_1().min_h_0().py_1().child(output))
            .child(
                div()
                    .flex_none()
                    .p_2()
                    .border_t_1()
                    .border_color(theme.border)
                    .child(Input::new(&self.input).id("script-repl").small()),
            )
    }
}

// --- The prompt ------------------------------------------------------------------------

/// The form a script's `ui.prompt(label, default)` opens in a dialog: the label and one
/// input, prefilled with the default. Its answer goes back to the waiting script once:
/// the text on OK (or Enter), `nil` on Cancel, Escape or closing.
pub struct ScriptPrompt {
    label: String,
    input: Entity<InputState>,
    answer: Option<async_channel::Sender<Option<String>>>,
}

impl ScriptPrompt {
    pub fn new(
        label: String,
        default: Option<String>,
        answer: async_channel::Sender<Option<String>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let input =
            cx.new(|cx| InputState::new(window, cx).default_value(default.unwrap_or_default()));
        input.update(cx, |input, cx| input.focus(window, cx));
        Self {
            label,
            input,
            answer: Some(answer),
        }
    }

    pub fn label(&self) -> &str {
        &self.label
    }

    pub fn value(&self, cx: &App) -> String {
        self.input.read(cx).value().to_string()
    }

    pub fn set_value(&mut self, text: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.input
            .update(cx, |input, cx| input.set_value(text.to_owned(), window, cx));
    }

    /// Whether the script still waits for the answer.
    pub fn is_pending(&self) -> bool {
        self.answer
            .as_ref()
            .is_some_and(|answer| !answer.is_closed())
    }

    /// Give the script its answer, `None` for `nil`; later calls do nothing. Returns
    /// whether the script was still waiting for it.
    pub fn answer(&mut self, answer: Option<String>) -> bool {
        let Some(sender) = self.answer.take() else {
            return false;
        };
        if sender.is_closed() {
            return false;
        }
        match answer {
            Some(text) => sender.try_send(Some(text)).is_ok(),
            // Dropping the sender answers `nil`.
            None => true,
        }
    }
}

impl Render for ScriptPrompt {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        v_flex()
            .gap_2()
            .child(
                div()
                    .text_sm()
                    .text_color(theme.foreground)
                    .child(SharedString::from(self.label.clone())),
            )
            .child(Input::new(&self.input).id("script-prompt-input"))
    }
}
