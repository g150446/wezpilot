use crate::overlay::confirm::run_confirmation;
use anyhow::{anyhow, bail, Context};
use mux::pane::PaneId;
use mux::termwiztermtab::TermWizTerminal;
use mux::Mux;
use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};
use std::time::Duration;
use termwiz::cell::{unicode_column_width, AttributeChange, Intensity};
use termwiz::color::{AnsiColor, ColorAttribute};
use termwiz::input::{InputEvent, KeyCode, KeyEvent};
use termwiz::surface::{Change, CursorVisibility, Position};
use termwiz::terminal::Terminal;
use wezterm_term::{KeyCode as PaneKeyCode, KeyModifiers, StableRowIndex};

const OPENROUTER_URL: &str = "https://openrouter.ai/api/v1/chat/completions";
const OPENROUTER_MODEL: &str = "moonshotai/kimi-k2.5";
const OPENROUTER_API_KEY_ENV: &str = "OPENROUTER_API_KEY";
const CONTEXT_LINES: usize = 48;
const POLL_INTERVAL: Duration = Duration::from_millis(250);
const SCROLL_STEP: usize = 5;

/// Fallback for GUI apps (macOS Dock/Finder launch) that don't inherit shell env vars.
/// Spawns a login shell to read a single environment variable.
fn read_env_from_login_shell(var: &str) -> Option<String> {
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".to_string());
    let output = std::process::Command::new(&shell)
        .args(["-l", "-c", &format!("printenv {}", var)])
        .output()
        .ok()?;
    if output.status.success() {
        let value = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if !value.is_empty() {
            return Some(value);
        }
    }
    None
}

const SYSTEM_PROMPT: &str = r#"You are an automation assistant for a terminal emulator.
You receive:
- the user's current instruction
- a recent plain-text snapshot of the active terminal pane

Respond with exactly one JSON object and no surrounding prose.
Schema:
{
  "message": "reply shown in the chat window (may be multi-line for translations/explanations)",
  "actions": [
    { "kind": "run_command", "command": "cd .." },
    { "kind": "send_input", "text": "yes", "submit": true },
    { "kind": "send_keys", "keys": ["DownArrow", "Enter"] }
  ]
}

Rules:
- Use "run_command" for shell commands that should be pasted and submitted.
- Use "send_input" to type plain text into an interactive program.
- Use "send_keys" for prompt navigation. Allowed keys: Enter, Tab, Escape, UpArrow, DownArrow, LeftArrow, RightArrow, Home, End.
- Return an empty actions array when no terminal input should be sent.
- Never include markdown fences.
- For display-only tasks (translation, explanation, summarisation):
  - Put the full result in "message", preserving line breaks with \n.
  - Return an empty actions array.
  - When translating, translate ALL lines of the terminal output faithfully.
"#;

// ── Styled transcript types ───────────────────────────────────────────────────

struct StyledSpan {
    text: String,
    fg: Option<AnsiColor>,
    bold: bool,
    dim: bool,
}

#[derive(Default)]
struct StyledLine(Vec<StyledSpan>);

impl StyledLine {
    fn plain(text: impl Into<String>) -> Self {
        Self(vec![StyledSpan {
            text: text.into(),
            fg: None,
            bold: false,
            dim: false,
        }])
    }

    fn colored(text: impl Into<String>, fg: AnsiColor, bold: bool) -> Self {
        Self(vec![StyledSpan {
            text: text.into(),
            fg: Some(fg),
            bold,
            dim: false,
        }])
    }

    fn dim(text: impl Into<String>, fg: AnsiColor) -> Self {
        Self(vec![StyledSpan {
            text: text.into(),
            fg: Some(fg),
            bold: false,
            dim: true,
        }])
    }

    fn emit(&self, changes: &mut Vec<Change>) {
        for span in &self.0 {
            if span.bold {
                changes.push(Change::Attribute(AttributeChange::Intensity(
                    Intensity::Bold,
                )));
            } else if span.dim {
                changes.push(Change::Attribute(AttributeChange::Intensity(
                    Intensity::Half,
                )));
            }
            if let Some(fg) = span.fg {
                changes.push(Change::Attribute(AttributeChange::Foreground(
                    ColorAttribute::PaletteIndex(fg as u8),
                )));
            }
            changes.push(Change::Text(span.text.clone()));
            changes.push(Change::Attribute(AttributeChange::Intensity(
                Intensity::Normal,
            )));
            changes.push(Change::Attribute(AttributeChange::Foreground(
                ColorAttribute::Default,
            )));
        }
    }
}

// ── Data model ────────────────────────────────────────────────────────────────

#[derive(Clone, Debug)]
struct TranscriptEntry {
    role: &'static str,
    text: String,
}

#[derive(Debug, Default, Deserialize, PartialEq, Eq)]
struct AssistantReply {
    #[serde(default)]
    message: String,
    #[serde(default)]
    actions: Vec<AssistantAction>,
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum AssistantAction {
    RunCommand {
        command: String,
    },
    SendInput {
        text: String,
        #[serde(default)]
        submit: bool,
    },
    SendKeys {
        keys: Vec<String>,
    },
}

#[derive(Serialize)]
struct OpenRouterRequest {
    model: &'static str,
    temperature: f32,
    messages: Vec<OpenRouterMessage>,
}

#[derive(Serialize)]
struct OpenRouterMessage {
    role: &'static str,
    content: String,
}

#[derive(Deserialize)]
struct OpenRouterResponse {
    choices: Vec<OpenRouterChoice>,
}

#[derive(Deserialize)]
struct OpenRouterChoice {
    message: OpenRouterResponseMessage,
}

#[derive(Deserialize)]
struct OpenRouterResponseMessage {
    content: OpenRouterContent,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum OpenRouterContent {
    Text(String),
    Parts(Vec<OpenRouterContentPart>),
}

#[derive(Deserialize)]
struct OpenRouterContentPart {
    #[serde(default)]
    text: String,
}

impl OpenRouterContent {
    fn into_text(self) -> String {
        match self {
            Self::Text(text) => text,
            Self::Parts(parts) => parts.into_iter().map(|part| part.text).collect(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ActionRisk {
    Safe,
    NeedsConfirmation(&'static str),
}

// ── Overlay struct ────────────────────────────────────────────────────────────

struct AiChatOverlay {
    pane_id: PaneId,
    pane_title: String,
    client: Client,
    transcript: Vec<TranscriptEntry>,
    input_buffer: String,
    status: String,
    automation_directive: Option<String>,
    last_automation_snapshot: Option<String>,
    /// Lines scrolled up from the bottom (0 = pinned to latest message).
    scroll_offset: usize,
    /// Cursor position in input buffer (0 = start, len = end)
    cursor_position: usize,
}

impl AiChatOverlay {
    fn new(pane_id: PaneId) -> anyhow::Result<Self> {
        let pane = Mux::get()
            .get_pane(pane_id)
            .ok_or_else(|| anyhow!("pane {pane_id} no longer exists"))?;

        Ok(Self {
            pane_id,
            pane_title: pane.get_title(),
            client: Client::builder()
                .timeout(Duration::from_secs(45))
                .build()
                .context("building OpenRouter client")?,
            transcript: vec![TranscriptEntry {
                role: "system",
                text: format!(
                    "Ready. Type a message, or use `/watch <instruction>` to monitor the terminal. `/watch off` to disable."
                ),
            }],
            input_buffer: String::new(),
            status: format!("{OPENROUTER_MODEL}"),
            automation_directive: None,
            last_automation_snapshot: None,
            scroll_offset: 0,
            cursor_position: 0,
        })
    }

    fn run_loop(&mut self, term: &mut TermWizTerminal) -> anyhow::Result<()> {
        term.set_raw_mode()?;
        term.no_grab_mouse_in_raw_mode();
        term.render(&[Change::Title("AI Chat".to_string())])?;
        self.render(term)?;

        loop {
            match term.poll_input(Some(POLL_INTERVAL))? {
                Some(InputEvent::Key(key)) => {
                    if self.handle_key_event(key, term)? {
                        return Ok(());
                    }
                }
                // IME-composed text (Japanese, Chinese, etc.) arrives as Paste events.
                Some(InputEvent::Paste(s)) => {
                    let byte_pos = char_to_byte_idx(&self.input_buffer, self.cursor_position);
                    self.input_buffer.insert_str(byte_pos, &s);
                    self.cursor_position += s.chars().count();
                    self.render(term)?;
                }
                Some(_) => {}
                None => {
                    self.maybe_run_automation(term)?;
                }
            }
        }
    }

    fn handle_key_event(
        &mut self,
        event: KeyEvent,
        term: &mut TermWizTerminal,
    ) -> anyhow::Result<bool> {
        match event.key {
            KeyCode::Escape => return Ok(true),
            KeyCode::Enter => {
                let input = self.input_buffer.trim().to_string();
                self.input_buffer.clear();
                self.cursor_position = 0;
                if !input.is_empty() {
                    self.handle_submission(input, term)?;
                }
            }
            KeyCode::Backspace => {
                if self.cursor_position > 0 {
                    let byte_pos = char_to_byte_idx(&self.input_buffer, self.cursor_position - 1);
                    self.input_buffer.remove(byte_pos);
                    self.cursor_position -= 1;
                }
            }
            KeyCode::Delete => {
                let char_count = self.input_buffer.chars().count();
                if self.cursor_position < char_count {
                    let byte_pos = char_to_byte_idx(&self.input_buffer, self.cursor_position);
                    self.input_buffer.remove(byte_pos);
                }
            }
            KeyCode::LeftArrow => {
                if self.cursor_position > 0 {
                    self.cursor_position -= 1;
                }
            }
            KeyCode::RightArrow => {
                if self.cursor_position < self.input_buffer.chars().count() {
                    self.cursor_position += 1;
                }
            }
            KeyCode::Home => {
                self.cursor_position = 0;
            }
            KeyCode::End => {
                self.cursor_position = self.input_buffer.chars().count();
            }
            KeyCode::PageUp => {
                self.scroll_offset = self.scroll_offset.saturating_add(SCROLL_STEP);
            }
            KeyCode::PageDown => {
                self.scroll_offset = self.scroll_offset.saturating_sub(SCROLL_STEP);
            }
            KeyCode::Char(c) if !event.modifiers.contains(termwiz::input::Modifiers::CTRL) => {
                let byte_pos = char_to_byte_idx(&self.input_buffer, self.cursor_position);
                self.input_buffer.insert(byte_pos, c);
                self.cursor_position += 1;
            }
            _ => {}
        }

        self.render(term)?;
        Ok(false)
    }

    fn handle_submission(
        &mut self,
        input: String,
        term: &mut TermWizTerminal,
    ) -> anyhow::Result<()> {
        if input == "/watch off" || input == "/unwatch" {
            self.automation_directive = None;
            self.last_automation_snapshot = None;
            self.push_system("Automation disabled.");
            self.status = "Automation off.".to_string();
            return Ok(());
        }

        if let Some(rest) = input.strip_prefix("/watch ") {
            let directive = rest.trim();
            if directive.is_empty() {
                self.push_system("Usage: /watch <instruction>");
                self.status = "Missing automation instruction.".to_string();
                return Ok(());
            }

            self.automation_directive = Some(directive.to_string());
            self.last_automation_snapshot = None;
            self.push_user(format!("/watch {directive}"));
            self.push_system(format!("Automation enabled: {directive}"));
            self.process_instruction(directive, true, term)?;
            return Ok(());
        }

        self.push_user(input.clone());
        self.process_instruction(&input, false, term)
    }

    fn process_instruction(
        &mut self,
        instruction: &str,
        from_watch_command: bool,
        term: &mut TermWizTerminal,
    ) -> anyhow::Result<()> {
        let pane_snapshot = self.capture_pane_snapshot()?;
        if from_watch_command {
            self.last_automation_snapshot = Some(pane_snapshot.clone());
        }

        self.status = "Thinking…".to_string();
        self.render(term)?;

        match self.query_openrouter(instruction, &pane_snapshot, false) {
            Ok(reply) => {
                self.apply_reply(reply, &pane_snapshot, false, term)?;
            }
            Err(err) => {
                self.push_system(format!("Error: {err:#}"));
                self.status = "Request failed.".to_string();
            }
        }
        Ok(())
    }

    fn maybe_run_automation(&mut self, term: &mut TermWizTerminal) -> anyhow::Result<()> {
        let Some(directive) = self.automation_directive.clone() else {
            return Ok(());
        };

        let snapshot = self.capture_pane_snapshot()?;
        if self.last_automation_snapshot.as_deref() == Some(snapshot.as_str()) {
            return Ok(());
        }
        self.last_automation_snapshot = Some(snapshot.clone());

        self.status = "Watching…".to_string();
        self.render(term)?;

        match self.query_openrouter(&directive, &snapshot, true) {
            Ok(reply) => {
                if !reply.message.trim().is_empty() || !reply.actions.is_empty() {
                    self.apply_reply(reply, &snapshot, true, term)?;
                } else {
                    self.status = format!("{OPENROUTER_MODEL} │ watching");
                }
            }
            Err(err) => {
                self.push_system(format!("Automation check failed: {err:#}"));
                self.status = "Automation check failed.".to_string();
            }
        }

        Ok(())
    }

    fn apply_reply(
        &mut self,
        reply: AssistantReply,
        pane_snapshot: &str,
        automated: bool,
        term: &mut TermWizTerminal,
    ) -> anyhow::Result<()> {
        if !reply.message.trim().is_empty() {
            let prefix = if automated { "[watch] " } else { "" };
            self.push_assistant(format!("{prefix}{}", reply.message.trim()));
        }

        if reply.actions.is_empty() {
            self.status = OPENROUTER_MODEL.to_string();
            self.render(term)?;
            return Ok(());
        }

        for action in reply.actions {
            self.execute_action(action, pane_snapshot, term)?;
        }

        self.status = OPENROUTER_MODEL.to_string();
        self.render(term)?;
        Ok(())
    }

    fn execute_action(
        &mut self,
        action: AssistantAction,
        pane_snapshot: &str,
        term: &mut TermWizTerminal,
    ) -> anyhow::Result<()> {
        let pane = self.resolve_pane()?;

        match action {
            AssistantAction::RunCommand { command } => {
                let command = command.trim();
                if command.is_empty() {
                    return Ok(());
                }

                match assess_command_risk(command) {
                    ActionRisk::Safe => {}
                    ActionRisk::NeedsConfirmation(reason) => {
                        let allowed = run_confirmation(
                            &format!(
                                "The AI wants to run:\n\n{command}\n\nReason for confirmation: {reason}\n\nPress Y to allow or N to block."
                            ),
                            term,
                        )?;
                        if !allowed {
                            self.push_system(format!("Blocked command: {command}"));
                            self.status = "Command blocked.".to_string();
                            return Ok(());
                        }
                    }
                }

                pane.send_paste(command)?;
                pane.key_down(PaneKeyCode::Char('\r'), KeyModifiers::NONE)?;
                self.push_system(format!("Ran: {command}"));
            }
            AssistantAction::SendInput { text, submit } => {
                let text = text.trim_end();
                if text.is_empty() {
                    return Ok(());
                }

                match assess_prompt_risk(text, pane_snapshot) {
                    ActionRisk::Safe => {}
                    ActionRisk::NeedsConfirmation(reason) => {
                        let allowed = run_confirmation(
                            &format!(
                                "The AI wants to type `{text}`{}.\n\nReason for confirmation: {reason}\n\nPress Y to allow or N to block.",
                                if submit { " and press Enter" } else { "" }
                            ),
                            term,
                        )?;
                        if !allowed {
                            self.push_system(format!("Blocked input: {text}"));
                            self.status = "Input blocked.".to_string();
                            return Ok(());
                        }
                    }
                }

                pane.send_paste(text)?;
                if submit {
                    pane.key_down(PaneKeyCode::Char('\r'), KeyModifiers::NONE)?;
                }
                self.push_system(format!(
                    "Sent: `{text}`{}",
                    if submit { " + Enter" } else { "" }
                ));
            }
            AssistantAction::SendKeys { keys } => {
                if keys.is_empty() {
                    return Ok(());
                }

                match assess_key_sequence_risk(&keys, pane_snapshot) {
                    ActionRisk::Safe => {}
                    ActionRisk::NeedsConfirmation(reason) => {
                        let rendered = keys.join(", ");
                        let allowed = run_confirmation(
                            &format!(
                                "The AI wants to send keys: {rendered}\n\nReason for confirmation: {reason}\n\nPress Y to allow or N to block."
                            ),
                            term,
                        )?;
                        if !allowed {
                            self.push_system(format!("Blocked keys: {}", keys.join(", ")));
                            self.status = "Key sequence blocked.".to_string();
                            return Ok(());
                        }
                    }
                }

                for key in &keys {
                    pane.key_down(parse_pane_key(key)?, KeyModifiers::NONE)?;
                }
                self.push_system(format!("Keys: {}", keys.join(", ")));
            }
        }

        Ok(())
    }

    fn query_openrouter(
        &self,
        instruction: &str,
        pane_snapshot: &str,
        automated: bool,
    ) -> anyhow::Result<AssistantReply> {
        let api_key = std::env::var(OPENROUTER_API_KEY_ENV)
            .ok()
            .or_else(|| read_env_from_login_shell(OPENROUTER_API_KEY_ENV))
            .with_context(|| format!("missing environment variable {OPENROUTER_API_KEY_ENV}"))?;

        let request = OpenRouterRequest {
            model: OPENROUTER_MODEL,
            temperature: 0.2,
            messages: vec![
                OpenRouterMessage {
                    role: "system",
                    content: SYSTEM_PROMPT.to_string(),
                },
                OpenRouterMessage {
                    role: "user",
                    content: format!(
                        "Mode: {}\nPane title: {}\nUser instruction:\n{}\n\nRecent terminal output:\n{}\n",
                        if automated { "automation" } else { "interactive" },
                        self.pane_title,
                        instruction,
                        pane_snapshot
                    ),
                },
            ],
        };

        let response = self
            .client
            .post(OPENROUTER_URL)
            .bearer_auth(api_key)
            .header("HTTP-Referer", "https://github.com/wez/wezterm")
            .header("X-Title", "wezterm ai chat")
            .json(&request)
            .send()
            .context("sending OpenRouter request")?
            .error_for_status()
            .context("OpenRouter returned an error status")?;

        let response: OpenRouterResponse =
            response.json().context("decoding OpenRouter response")?;
        let raw = response
            .choices
            .into_iter()
            .next()
            .ok_or_else(|| anyhow!("OpenRouter response had no choices"))?
            .message
            .content
            .into_text();

        parse_assistant_reply(&raw)
    }

    fn capture_pane_snapshot(&self) -> anyhow::Result<String> {
        let pane = self.resolve_pane()?;
        let dims = pane.get_dimensions();
        let nlines = CONTEXT_LINES.max(dims.viewport_rows.min(CONTEXT_LINES));
        let bottom_row = dims.physical_top + dims.viewport_rows as StableRowIndex;
        let top_row = bottom_row.saturating_sub(nlines as StableRowIndex);
        let lines = pane.get_logical_lines(top_row..bottom_row);
        let mut text = String::new();
        for line in lines {
            for cell in line.logical.visible_cells() {
                text.push_str(cell.str());
            }
            let trimmed = text.trim_end().len();
            text.truncate(trimmed);
            text.push('\n');
        }
        let trimmed = text.trim_end().len();
        text.truncate(trimmed);
        Ok(text)
    }

    fn resolve_pane(&self) -> anyhow::Result<std::sync::Arc<dyn mux::pane::Pane>> {
        Mux::get()
            .get_pane(self.pane_id)
            .ok_or_else(|| anyhow!("pane {} no longer exists", self.pane_id))
    }

    fn push_user(&mut self, text: impl Into<String>) {
        self.scroll_offset = 0;
        self.transcript.push(TranscriptEntry {
            role: "user",
            text: text.into(),
        });
    }

    fn push_assistant(&mut self, text: impl Into<String>) {
        self.scroll_offset = 0;
        self.transcript.push(TranscriptEntry {
            role: "assistant",
            text: text.into(),
        });
    }

    fn push_system(&mut self, text: impl Into<String>) {
        self.scroll_offset = 0;
        self.transcript.push(TranscriptEntry {
            role: "system",
            text: text.into(),
        });
    }

    // ── Rendering ─────────────────────────────────────────────────────────────

    fn render(&self, term: &mut TermWizTerminal) -> anyhow::Result<()> {
        let size = term.get_screen_size()?;
        let w = size.cols.max(20);
        let h = size.rows.max(10);

        // 7:3 split between transcript (top) and input (bottom).
        //
        // Row 0             : header
        // Row 1             : separator
        // Row 2..split-1    : transcript  (split-2 rows)
        // Row split         : separator
        // Row split+1       : status / hints
        // Row split+2       : separator
        // Row split+3..h-1  : input area  (h-split-3 rows)

        let split = ((h * 7 / 10).max(5)).min(h.saturating_sub(5));
        let transcript_h = split.saturating_sub(2).max(1);
        let input_first_row = split + 3;
        let input_h = h.saturating_sub(input_first_row).max(1);
        // Width available for input text after the "  ❯ " prefix (4 chars).
        let input_text_w = w.saturating_sub(4).max(1);

        let mut c: Vec<Change> = vec![
            Change::ClearScreen(ColorAttribute::Default),
            Change::CursorVisibility(CursorVisibility::Hidden),
        ];

        // ── Header ──────────────────────────────────────────────────────────
        at(&mut c, 0, 0);
        c.push(Change::Attribute(AttributeChange::Reverse(true)));
        c.push(Change::Attribute(AttributeChange::Intensity(
            Intensity::Bold,
        )));
        let header = format!("  AI Chat — {}", self.pane_title);
        c.push(Change::Text(pad_right(&header, w)));
        c.push(Change::Attribute(AttributeChange::Reverse(false)));
        c.push(Change::Attribute(AttributeChange::Intensity(
            Intensity::Normal,
        )));

        // ── Top separator ────────────────────────────────────────────────────
        self.draw_separator(&mut c, 0, 1, w);

        // ── Transcript ───────────────────────────────────────────────────────
        let lines = self.render_transcript(w);
        let total = lines.len();
        let max_scroll = total.saturating_sub(transcript_h);
        let scroll = self.scroll_offset.min(max_scroll);
        let lo = total.saturating_sub(transcript_h + scroll);
        let hi = lo + transcript_h.min(total);

        for (i, line) in lines[lo..hi.min(total)].iter().enumerate() {
            at(&mut c, 0, 2 + i);
            line.emit(&mut c);
            c.push(Change::ClearToEndOfLine(ColorAttribute::Default));
        }
        for i in (hi - lo)..transcript_h {
            at(&mut c, 0, 2 + i);
            c.push(Change::ClearToEndOfLine(ColorAttribute::Default));
        }

        // Scroll indicator (top-right corner of transcript area)
        if scroll > 0 {
            let indicator = format!(" ↑ {} lines ", scroll);
            let ix = w.saturating_sub(indicator.chars().count() + 1);
            at(&mut c, ix, 2);
            c.push(Change::Attribute(AttributeChange::Intensity(
                Intensity::Half,
            )));
            c.push(Change::Text(indicator));
            c.push(Change::Attribute(AttributeChange::Intensity(
                Intensity::Normal,
            )));
        }

        // ── Mid separator ────────────────────────────────────────────────────
        self.draw_separator(&mut c, 0, split, w);

        // ── Status / hints ───────────────────────────────────────────────────
        at(&mut c, 0, split + 1);
        c.push(Change::Attribute(AttributeChange::Intensity(
            Intensity::Half,
        )));
        let auto_label = match &self.automation_directive {
            Some(d) => format!("/watch: {}  ", d),
            None => String::new(),
        };
        let hints = "PgUp/PgDn: scroll  Esc: close";
        let status_left = format!("  {}  {}", self.status, auto_label);
        let status_right = format!("{}  ", hints);
        let gap = w.saturating_sub(status_left.chars().count() + status_right.chars().count());
        c.push(Change::Text(status_left));
        c.push(Change::Text(" ".repeat(gap)));
        c.push(Change::Text(status_right));
        c.push(Change::Attribute(AttributeChange::Intensity(
            Intensity::Normal,
        )));

        // ── Input separator ──────────────────────────────────────────────────
        self.draw_separator(&mut c, 0, split + 2, w);

        // ── Input area (word-wrapped, multi-row) ─────────────────────────────
        // Wrap the buffer into lines that fit input_text_w.
        let wrapped_input: Vec<String> = if self.input_buffer.is_empty() {
            vec![String::new()]
        } else {
            textwrap::wrap(&self.input_buffer, input_text_w)
                .into_iter()
                .map(|s| s.into_owned())
                .collect()
        };

        // Calculate cursor position based on wrapped lines
        let mut cursor_row = 0;
        let mut cursor_col = 0;
        let mut remaining_pos = self.cursor_position;

        for (line_idx, line) in wrapped_input.iter().enumerate() {
            let line_char_count = line.chars().count();
            if remaining_pos <= line_char_count {
                // Cursor is in this line; use display width for correct full-width char handling.
                let prefix_byte = char_to_byte_idx(line, remaining_pos);
                cursor_row = input_first_row + line_idx;
                cursor_col = 4 + unicode_column_width(&line[..prefix_byte], None); // 4 for "  ❯ "
                break;
            } else {
                // Cursor is after this line
                remaining_pos -= line_char_count;
                // Account for newline character (except for last line)
                if line_idx < wrapped_input.len() - 1 {
                    remaining_pos -= 1; // for '\n'
                }
            }
        }

        // First row: prompt glyph + first wrapped line.
        at(&mut c, 0, input_first_row);
        c.push(Change::Attribute(AttributeChange::Foreground(
            ColorAttribute::PaletteIndex(AnsiColor::Aqua as u8),
        )));
        c.push(Change::Attribute(AttributeChange::Intensity(
            Intensity::Bold,
        )));
        c.push(Change::Text("  ❯ ".to_string()));
        c.push(Change::Attribute(AttributeChange::Intensity(
            Intensity::Normal,
        )));
        c.push(Change::Attribute(AttributeChange::Foreground(
            ColorAttribute::Default,
        )));
        c.push(Change::Text(wrapped_input[0].clone()));
        c.push(Change::ClearToEndOfLine(ColorAttribute::Default));

        // Continuation rows (indented to align with text after ❯).
        for (i, line) in wrapped_input.iter().enumerate().skip(1) {
            let row = input_first_row + i;
            if row >= h {
                break;
            }
            at(&mut c, 0, row);
            c.push(Change::Text(format!("    {}", line)));
            c.push(Change::ClearToEndOfLine(ColorAttribute::Default));
        }
        // Clear remaining input rows.
        for i in wrapped_input.len()..input_h {
            let row = input_first_row + i;
            if row >= h {
                break;
            }
            at(&mut c, 0, row);
            c.push(Change::ClearToEndOfLine(ColorAttribute::Default));
        }

        // Place cursor at calculated position.
        c.push(Change::CursorPosition {
            x: Position::Absolute(cursor_col.min(w.saturating_sub(1))),
            y: Position::Absolute(cursor_row.min(h.saturating_sub(1))),
        });
        c.push(Change::CursorVisibility(CursorVisibility::Visible));

        term.render(&c)?;
        term.flush()?;
        Ok(())
    }

    fn draw_separator(&self, changes: &mut Vec<Change>, x: usize, y: usize, width: usize) {
        at(changes, x, y);
        changes.push(Change::Attribute(AttributeChange::Foreground(
            ColorAttribute::PaletteIndex(AnsiColor::Navy as u8),
        )));
        changes.push(Change::Text("─".repeat(width)));
        changes.push(Change::Attribute(AttributeChange::Foreground(
            ColorAttribute::Default,
        )));
    }

    fn render_transcript(&self, width: usize) -> Vec<StyledLine> {
        let mut lines: Vec<StyledLine> = Vec::new();
        let text_width = width.saturating_sub(4).max(1);

        for entry in &self.transcript {
            match entry.role {
                "user" => {
                    lines.push(StyledLine::default());
                    lines.push(StyledLine::colored("  You", AnsiColor::Aqua, true));
                    for wrapped in wrap_preserving_newlines(&entry.text, text_width) {
                        lines.push(StyledLine::plain(format!("    {}", wrapped)));
                    }
                }
                "assistant" => {
                    lines.push(StyledLine::default());
                    lines.push(StyledLine::colored("  AI", AnsiColor::Lime, true));
                    for wrapped in wrap_preserving_newlines(&entry.text, text_width) {
                        lines.push(StyledLine::plain(format!("    {}", wrapped)));
                    }
                }
                _ => {
                    // System messages: dim, no label
                    for wrapped in wrap_preserving_newlines(&entry.text, text_width) {
                        lines.push(StyledLine::dim(format!("  {}", wrapped), AnsiColor::Yellow));
                    }
                }
            }
        }

        if lines.is_empty() {
            lines.push(StyledLine::default());
        }
        lines
    }
}

// ── Text helpers ──────────────────────────────────────────────────────────────

/// Convert a char-index cursor position to the corresponding byte offset in `s`.
fn char_to_byte_idx(s: &str, char_idx: usize) -> usize {
    s.char_indices()
        .nth(char_idx)
        .map(|(i, _)| i)
        .unwrap_or(s.len())
}

// ── Rendering helpers ─────────────────────────────────────────────────────────

fn at(changes: &mut Vec<Change>, x: usize, y: usize) {
    changes.push(Change::CursorPosition {
        x: Position::Absolute(x),
        y: Position::Absolute(y),
    });
}

/// Split `text` by newlines, then word-wrap each paragraph to `width`.
/// Empty lines (blank lines in the original) are preserved as empty strings.
fn wrap_preserving_newlines(text: &str, width: usize) -> Vec<String> {
    let mut out = Vec::new();
    for paragraph in text.split('\n') {
        if paragraph.trim().is_empty() {
            out.push(String::new());
        } else {
            for line in textwrap::wrap(paragraph, width) {
                out.push(line.into_owned());
            }
        }
    }
    if out.is_empty() {
        out.push(String::new());
    }
    out
}

fn pad_right(text: &str, width: usize) -> String {
    let len = text.chars().count();
    if len >= width {
        text.chars().take(width).collect()
    } else {
        format!("{}{}", text, " ".repeat(width - len))
    }
}

// ── API / logic helpers ───────────────────────────────────────────────────────

fn parse_assistant_reply(raw: &str) -> anyhow::Result<AssistantReply> {
    if let Ok(reply) = serde_json::from_str(raw) {
        return Ok(reply);
    }

    let json = extract_json_object(raw).ok_or_else(|| anyhow!("assistant did not return JSON"))?;
    serde_json::from_str(&json).context("parsing assistant JSON")
}

fn extract_json_object(raw: &str) -> Option<String> {
    let start = raw.find('{')?;
    let end = raw.rfind('}')?;
    if end < start {
        return None;
    }
    Some(raw[start..=end].to_string())
}

fn assess_command_risk(command: &str) -> ActionRisk {
    let lowered = command.to_ascii_lowercase();
    let blocked = [
        ("sudo", "commands requesting elevated privileges"),
        ("rm ", "commands that remove files"),
        ("rm\t", "commands that remove files"),
        ("rm-", "commands that remove files"),
        ("dd ", "raw disk write commands"),
        ("mkfs", "filesystem formatting commands"),
        ("fdisk", "disk partitioning commands"),
        ("diskutil erase", "disk erase commands"),
        ("shutdown", "system shutdown commands"),
        ("reboot", "system reboot commands"),
        ("poweroff", "system power commands"),
        ("halt", "system power commands"),
        ("chmod -r", "recursive permission changes"),
        ("chown -r", "recursive ownership changes"),
    ];

    for (needle, reason) in blocked {
        if lowered.contains(needle) {
            return ActionRisk::NeedsConfirmation(reason);
        }
    }

    ActionRisk::Safe
}

fn assess_prompt_risk(response: &str, snapshot: &str) -> ActionRisk {
    if is_affirmative(response) && context_contains_danger(snapshot) {
        return ActionRisk::NeedsConfirmation(
            "the active terminal output looks like a destructive or privileged confirmation prompt",
        );
    }

    ActionRisk::Safe
}

fn assess_key_sequence_risk(keys: &[String], snapshot: &str) -> ActionRisk {
    for key in keys {
        if parse_pane_key(key).is_err() {
            return ActionRisk::NeedsConfirmation("the key sequence contains an unsupported key");
        }
    }

    if keys.iter().any(|key| key == "Enter" || key == "Tab") && context_contains_danger(snapshot) {
        return ActionRisk::NeedsConfirmation(
            "confirming the current prompt looks risky based on the terminal output",
        );
    }

    ActionRisk::Safe
}

fn is_affirmative(response: &str) -> bool {
    matches!(
        response.trim().to_ascii_lowercase().as_str(),
        "y" | "yes" | "ok" | "okay" | "approve" | "approved" | "continue"
    )
}

fn context_contains_danger(snapshot: &str) -> bool {
    let lowered = snapshot.to_ascii_lowercase();
    let needles = [
        "sudo",
        "rm ",
        " rm",
        "delete",
        "destroy",
        "overwrite",
        "drop database",
        "format disk",
        "erase disk",
        "permanently remove",
        "irreversible",
        "administrator privileges",
    ];
    needles.iter().any(|needle| lowered.contains(needle))
}

fn parse_pane_key(key: &str) -> anyhow::Result<PaneKeyCode> {
    Ok(match key {
        "Enter" => PaneKeyCode::Char('\r'),
        "Tab" => PaneKeyCode::Char('\t'),
        "Escape" => PaneKeyCode::Char('\u{1b}'),
        "UpArrow" => PaneKeyCode::UpArrow,
        "DownArrow" => PaneKeyCode::DownArrow,
        "LeftArrow" => PaneKeyCode::LeftArrow,
        "RightArrow" => PaneKeyCode::RightArrow,
        "Home" => PaneKeyCode::Home,
        "End" => PaneKeyCode::End,
        other => bail!("unsupported key `{other}`"),
    })
}

// ── Public entry point ────────────────────────────────────────────────────────

pub fn ai_chat_overlay(mut term: TermWizTerminal, pane_id: PaneId) -> anyhow::Result<()> {
    let mut overlay = AiChatOverlay::new(pane_id)?;
    overlay.run_loop(&mut term)
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn parses_plain_json_reply() {
        let reply = parse_assistant_reply(
            r#"{"message":"done","actions":[{"kind":"run_command","command":"cd .."}]}"#,
        )
        .unwrap();
        assert_eq!(
            reply,
            AssistantReply {
                message: "done".to_string(),
                actions: vec![AssistantAction::RunCommand {
                    command: "cd ..".to_string()
                }],
            }
        );
    }

    #[test]
    fn parses_fenced_json_reply() {
        let reply = parse_assistant_reply(
            "```json\n{\"message\":\"ok\",\"actions\":[{\"kind\":\"send_keys\",\"keys\":[\"Enter\"]}]}\n```",
        )
        .unwrap();
        assert_eq!(
            reply,
            AssistantReply {
                message: "ok".to_string(),
                actions: vec![AssistantAction::SendKeys {
                    keys: vec!["Enter".to_string()]
                }],
            }
        );
    }

    #[test]
    fn flags_dangerous_commands() {
        assert!(matches!(
            assess_command_risk("sudo rm -rf /tmp/demo"),
            ActionRisk::NeedsConfirmation(_)
        ));
        assert_eq!(assess_command_risk("cd .."), ActionRisk::Safe);
    }

    #[test]
    fn flags_risky_prompt_approvals() {
        assert!(matches!(
            assess_prompt_risk("yes", "sudo wants to delete files permanently"),
            ActionRisk::NeedsConfirmation(_)
        ));
        assert_eq!(
            assess_prompt_risk("yes", "GitHub Copilot wants approval"),
            ActionRisk::Safe
        );
    }
}
