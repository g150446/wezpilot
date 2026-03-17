use crate::overlay::confirm::run_confirmation;
use anyhow::{anyhow, bail, Context};
use mux::pane::PaneId;
use mux::termwiztermtab::TermWizTerminal;
use mux::Mux;
use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};
use std::time::Duration;
use termwiz::color::ColorAttribute;
use termwiz::input::{InputEvent, KeyCode, KeyEvent};
use termwiz::surface::{Change, CursorVisibility, Position};
use termwiz::terminal::Terminal;
use wezterm_term::{KeyCode as PaneKeyCode, KeyModifiers, StableRowIndex};

const OPENROUTER_URL: &str = "https://openrouter.ai/api/v1/chat/completions";
const OPENROUTER_MODEL: &str = "moonshotai/kimi-k2.5";
const OPENROUTER_API_KEY_ENV: &str = "OPENROUTER_API_KEY";
const CONTEXT_LINES: usize = 48;
const POLL_INTERVAL: Duration = Duration::from_millis(250);

const SYSTEM_PROMPT: &str = r#"You are an automation assistant for a terminal emulator.
You receive:
- the user's current instruction
- a recent plain-text snapshot of the active terminal pane

Respond with exactly one JSON object and no surrounding prose.
Schema:
{
  "message": "short natural-language reply for the chat window",
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
- Keep "message" short and practical.
- Never include markdown fences.
"#;

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
    RunCommand { command: String },
    SendInput { text: String, #[serde(default)] submit: bool },
    SendKeys { keys: Vec<String> },
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

struct AiChatOverlay {
    pane_id: PaneId,
    pane_title: String,
    client: Client,
    transcript: Vec<TranscriptEntry>,
    input_buffer: String,
    status: String,
    automation_directive: Option<String>,
    last_automation_snapshot: Option<String>,
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
                    "Ready. Enter a prompt, or use `/watch <instruction>` to keep monitoring the active pane. `/watch off` disables automation."
                ),
            }],
            input_buffer: String::new(),
            status: format!(
                "Model: {OPENROUTER_MODEL} | API key env: {OPENROUTER_API_KEY_ENV}"
            ),
            automation_directive: None,
            last_automation_snapshot: None,
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
                if !input.is_empty() {
                    self.handle_submission(input, term)?;
                } else {
                    self.status = "Type a prompt or press Esc to close.".to_string();
                }
            }
            KeyCode::Backspace => {
                self.input_buffer.pop();
            }
            KeyCode::Char(c) if !event.modifiers.contains(termwiz::input::Modifiers::CTRL) => {
                self.input_buffer.push(c);
            }
            _ => {}
        }

        self.render(term)?;
        Ok(false)
    }

    fn handle_submission(&mut self, input: String, term: &mut TermWizTerminal) -> anyhow::Result<()> {
        if input == "/watch off" || input == "/unwatch" {
            self.automation_directive = None;
            self.last_automation_snapshot = None;
            self.push_system("Automation disabled.");
            self.status = "Automation disabled.".to_string();
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

        self.status = "Waiting for OpenRouter...".to_string();
        self.render(term)?;

        match self.query_openrouter(instruction, &pane_snapshot, false) {
            Ok(reply) => {
                self.apply_reply(reply, &pane_snapshot, false, term)?;
            }
            Err(err) => {
                self.push_system(format!("OpenRouter request failed: {err:#}"));
                self.status = "OpenRouter request failed.".to_string();
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

        self.status = "Automation is checking the latest pane output...".to_string();
        self.render(term)?;

        match self.query_openrouter(&directive, &snapshot, true) {
            Ok(reply) => {
                if !reply.message.trim().is_empty() || !reply.actions.is_empty() {
                    self.apply_reply(reply, &snapshot, true, term)?;
                } else {
                    self.status = "Automation idle.".to_string();
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
            self.status = if automated {
                "Automation observed output but took no action.".to_string()
            } else {
                "No terminal input was sent.".to_string()
            };
            self.render(term)?;
            return Ok(());
        }

        for action in reply.actions {
            self.execute_action(action, pane_snapshot, term)?;
        }

        self.status = if automated {
            "Automation action applied.".to_string()
        } else {
            "Action applied.".to_string()
        };
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
                self.push_system(format!("Ran command: {command}"));
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
                            self.push_system(format!("Blocked prompt response: {text}"));
                            self.status = "Prompt response blocked.".to_string();
                            return Ok(());
                        }
                    }
                }

                pane.send_paste(text)?;
                if submit {
                    pane.key_down(PaneKeyCode::Char('\r'), KeyModifiers::NONE)?;
                }
                self.push_system(format!(
                    "Sent input: `{text}`{}",
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
                            self.push_system(format!("Blocked key sequence: {}", keys.join(", ")));
                            self.status = "Key sequence blocked.".to_string();
                            return Ok(());
                        }
                    }
                }

                for key in &keys {
                    pane.key_down(parse_pane_key(key)?, KeyModifiers::NONE)?;
                }
                self.push_system(format!("Sent keys: {}", keys.join(", ")));
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

        let response: OpenRouterResponse = response.json().context("decoding OpenRouter response")?;
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
        self.transcript.push(TranscriptEntry {
            role: "user",
            text: text.into(),
        });
    }

    fn push_assistant(&mut self, text: impl Into<String>) {
        self.transcript.push(TranscriptEntry {
            role: "assistant",
            text: text.into(),
        });
    }

    fn push_system(&mut self, text: impl Into<String>) {
        self.transcript.push(TranscriptEntry {
            role: "system",
            text: text.into(),
        });
    }

    fn render(&self, term: &mut TermWizTerminal) -> anyhow::Result<()> {
        let size = term.get_screen_size()?;
        let width = size.cols.max(40);
        let height = size.rows.max(12);
        let box_width = (width * 85 / 100).max(40).min(width.saturating_sub(2));
        let box_height = (height * 70 / 100).max(10).min(height.saturating_sub(2));
        let x = (width.saturating_sub(box_width)) / 2;
        let y = (height.saturating_sub(box_height)) / 2;
        let inner_width = box_width.saturating_sub(4).max(10);
        let transcript_height = box_height.saturating_sub(8).max(1);

        let mut changes = vec![
            Change::ClearScreen(ColorAttribute::Default),
            Change::CursorVisibility(CursorVisibility::Hidden),
        ];

        let horizontal = format!("+{}+", "-".repeat(box_width.saturating_sub(2)));
        push_line(&mut changes, x, y, &horizontal);
        for row in 1..box_height.saturating_sub(1) {
            push_line(
                &mut changes,
                x,
                y + row,
                &format!("|{}|", " ".repeat(box_width.saturating_sub(2))),
            );
        }
        push_line(&mut changes, x, y + box_height.saturating_sub(1), &horizontal);

        let title = format!(" AI Chat - {} ", self.pane_title);
        push_line(&mut changes, x + 2, y, &truncate_left(&title, box_width.saturating_sub(4)));

        push_line(
            &mut changes,
            x + 2,
            y + 1,
            &truncate_left(
                "Enter=send  Esc=close  /watch <instruction>=auto mode  /watch off=disable",
                inner_width,
            ),
        );

        let transcript_lines = self.render_transcript_lines(inner_width);
        let start = transcript_lines.len().saturating_sub(transcript_height);
        for (idx, line) in transcript_lines[start..].iter().enumerate() {
            push_line(&mut changes, x + 2, y + 3 + idx, line);
        }

        push_line(
            &mut changes,
            x + 2,
            y + box_height.saturating_sub(4),
            &truncate_left(
                &format!(
                    "Automation: {}",
                    self.automation_directive
                        .as_deref()
                        .unwrap_or("off")
                ),
                inner_width,
            ),
        );
        push_line(
            &mut changes,
            x + 2,
            y + box_height.saturating_sub(3),
            &truncate_left(&format!("Status: {}", self.status), inner_width),
        );

        let prompt = format!("> {}", self.input_buffer);
        let rendered_prompt = truncate_left(&prompt, inner_width);
        let prompt_row = y + box_height.saturating_sub(2);
        push_line(&mut changes, x + 2, prompt_row, &rendered_prompt);
        changes.push(Change::CursorPosition {
            x: Position::Absolute(x + 2 + rendered_prompt.len()),
            y: Position::Absolute(prompt_row),
        });
        changes.push(Change::CursorVisibility(CursorVisibility::Visible));

        term.render(&changes)?;
        term.flush()?;
        Ok(())
    }

    fn render_transcript_lines(&self, width: usize) -> Vec<String> {
        let mut lines = Vec::new();
        for entry in &self.transcript {
            let prefix = match entry.role {
                "user" => "You: ",
                "assistant" => "AI: ",
                _ => "Info: ",
            };
            let rendered = format!("{prefix}{}", entry.text);
            let wrapped = textwrap::wrap(&rendered, width);
            if wrapped.is_empty() {
                lines.push(String::new());
            } else {
                lines.extend(wrapped.into_iter().map(|line| line.into_owned()));
            }
        }
        if lines.is_empty() {
            lines.push(String::new());
        }
        lines
    }
}

fn push_line(changes: &mut Vec<Change>, x: usize, y: usize, text: &str) {
    changes.push(Change::CursorPosition {
        x: Position::Absolute(x),
        y: Position::Absolute(y),
    });
    changes.push(Change::Text(text.to_string()));
}

fn truncate_left(text: &str, width: usize) -> String {
    if text.chars().count() <= width {
        return text.to_string();
    }
    if width <= 3 {
        return ".".repeat(width);
    }
    let suffix: String = text
        .chars()
        .rev()
        .take(width - 3)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    format!("...{suffix}")
}

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

pub fn ai_chat_overlay(mut term: TermWizTerminal, pane_id: PaneId) -> anyhow::Result<()> {
    let mut overlay = AiChatOverlay::new(pane_id)?;
    overlay.run_loop(&mut term)
}

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
        assert_eq!(assess_prompt_risk("yes", "GitHub Copilot wants approval"), ActionRisk::Safe);
    }
}
