//! Keyboard-driven terminal UI. All USB commands run on a single worker thread.

use std::{
    io::{self, IsTerminal, Stdout},
    panic::{self, PanicHookInfo},
    sync::{Arc, mpsc},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use crossterm::{
    cursor::{Hide, Show},
    event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Cell, Paragraph, Row, Table, TableState, Wrap},
};

use crate::{
    backend::Backend,
    model::{Device, Environment},
};

const MIN_WIDTH: u16 = 60;
const MIN_HEIGHT: u16 = 17;
const POLL_INTERVAL: Duration = Duration::from_millis(100);

/// Run the interactive device picker until the user quits.
///
/// Quitting while a command is active waits for that command to finish. This
/// avoids leaving an elevation or attach command running after the UI closes.
pub fn run<B: Backend + 'static>(
    backend: B,
    environment: Environment,
    distribution: String,
    refresh_interval: Duration,
) -> Result<()> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        bail!(
            "交互模式需要终端，请直接在 PowerShell / Windows Terminal 中运行，或使用 list 子命令。"
        );
    }

    let mut session = TerminalSession::enter()?;
    let mut worker = Worker::start(backend)?;
    let mut state = UiState::new();
    worker.send(Request::Refresh)?;
    state.pending = Some(Operation::Refresh);

    let result = event_loop(
        &mut session.terminal,
        &worker,
        &mut state,
        &environment,
        &distribution,
        refresh_interval.max(Duration::from_millis(500)),
    );
    // Wait before restoring the screen, so an active operation remains visible.
    let shutdown_result = worker.stop();
    drop(session);
    result.and(shutdown_result)
}

fn event_loop(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    worker: &Worker,
    state: &mut UiState,
    environment: &Environment,
    distribution: &str,
    refresh_interval: Duration,
) -> Result<()> {
    loop {
        match worker.responses.try_recv() {
            Ok(response) => {
                let operation_error = response
                    .action
                    .as_ref()
                    .and_then(|result| result.as_ref().err())
                    .cloned();
                let queued = state.complete(response);
                if state.quitting {
                    if let Some(error) = operation_error {
                        bail!("{error}");
                    }
                    return Ok(());
                }
                if let Some(intent) = queued {
                    dispatch_action(worker, state, intent, distribution)?;
                }
            }
            Err(mpsc::TryRecvError::Empty) => {}
            Err(mpsc::TryRecvError::Disconnected) => bail!("设备操作线程意外退出。"),
        }

        if state.pending.is_none() && state.last_refresh.elapsed() >= refresh_interval {
            worker.send(Request::Refresh)?;
            state.pending = Some(Operation::Refresh);
        }

        terminal
            .draw(|frame| render(frame, state, environment, distribution))
            .context("无法绘制终端界面")?;
        state.tick = state.tick.wrapping_add(1);

        if !event::poll(POLL_INTERVAL).context("无法读取终端事件")? {
            continue;
        }
        let Event::Key(key) = event::read().context("无法读取按键")? else {
            continue;
        };
        if key.kind == KeyEventKind::Release {
            continue;
        }
        if is_quit(key) {
            if state.pending.is_none() {
                return Ok(());
            }
            state.request_quit();
            continue;
        }
        if state.quitting {
            continue;
        }
        if key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
        {
            continue;
        }
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => state.move_selection(false),
            KeyCode::Down | KeyCode::Char('j') => state.move_selection(true),
            KeyCode::Home => state.select_first(),
            KeyCode::End => state.select_last(),
            _ if key.kind != KeyEventKind::Press => {}
            KeyCode::Char('r' | 'R') if state.pending.is_none() => {
                worker.send(Request::Refresh)?;
                state.pending = Some(Operation::Refresh);
            }
            KeyCode::Enter | KeyCode::Char('a' | 'A' | 'd' | 'D') => {
                let size = terminal.size().context("无法读取终端尺寸")?;
                if size.width < MIN_WIDTH || size.height < MIN_HEIGHT {
                    state.notice = Notice::error("请先放大终端窗口，再操作设备。".into());
                    continue;
                }
                if let Some(intent) = state.action_key(key) {
                    dispatch_action(worker, state, intent, distribution)?;
                }
            }
            _ => {}
        }
    }
}

fn dispatch_action(
    worker: &Worker,
    state: &mut UiState,
    intent: ActionIntent,
    distribution: &str,
) -> Result<()> {
    let operation = match intent.action {
        DeviceAction::Attach => {
            worker.send(Request::Attach {
                device: intent.device,
                distribution: distribution.to_owned(),
            })?;
            Operation::Attach
        }
        DeviceAction::Detach => {
            worker.send(Request::Detach(intent.device))?;
            Operation::Detach
        }
    };
    state.pending = Some(operation);
    Ok(())
}

fn is_quit(key: KeyEvent) -> bool {
    matches!(key.code, KeyCode::Esc | KeyCode::Char('q' | 'Q'))
        || (key.modifiers.contains(KeyModifiers::CONTROL)
            && matches!(key.code, KeyCode::Char('c' | 'C')))
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Operation {
    Refresh,
    Attach,
    Detach,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DeviceAction {
    Attach,
    Detach,
}

impl DeviceAction {
    fn label(self) -> &'static str {
        match self {
            Self::Attach => "映射",
            Self::Detach => "断开",
        }
    }
}

struct ActionIntent {
    action: DeviceAction,
    device: Device,
}

impl Operation {
    fn label(self) -> &'static str {
        match self {
            Self::Refresh => "正在刷新设备",
            Self::Attach => "正在映射；首次共享可能需要在 UAC 提示中授权",
            Self::Detach => "正在断开设备",
        }
    }
}

struct Notice {
    text: String,
    color: Color,
}

impl Notice {
    fn info(text: String) -> Self {
        Self {
            text,
            color: Color::Cyan,
        }
    }

    fn error(text: String) -> Self {
        Self {
            text,
            color: Color::LightRed,
        }
    }

    fn success(text: String) -> Self {
        Self {
            text,
            color: Color::LightGreen,
        }
    }
}

struct UiState {
    devices: Vec<Device>,
    table: TableState,
    notice: Notice,
    pending: Option<Operation>,
    queued: Option<ActionIntent>,
    quitting: bool,
    loaded: bool,
    last_refresh: Instant,
    tick: usize,
}

impl UiState {
    fn new() -> Self {
        Self {
            devices: Vec::new(),
            table: TableState::default(),
            notice: Notice::info("选择 USB 设备后按 Enter 映射到 WSL2。".into()),
            pending: None,
            queued: None,
            quitting: false,
            loaded: false,
            last_refresh: Instant::now(),
            tick: 0,
        }
    }

    fn selected(&self) -> Option<&Device> {
        self.table
            .selected()
            .and_then(|index| self.devices.get(index))
    }

    fn move_selection(&mut self, forward: bool) {
        if self.devices.is_empty() {
            self.table.select(None);
            return;
        }
        let last = self.devices.len() - 1;
        let index = match (self.table.selected(), forward) {
            (Some(index), true) => index.saturating_add(1).min(last),
            (Some(index), false) => index.saturating_sub(1),
            (None, true) => 0,
            (None, false) => last,
        };
        self.table.select(Some(index));
    }

    fn select_first(&mut self) {
        self.table.select((!self.devices.is_empty()).then_some(0));
    }

    fn select_last(&mut self) {
        self.table.select(self.devices.len().checked_sub(1));
    }

    /// Accept one deliberate press even if a periodic refresh is in progress.
    /// Additional presses never replace a queued device snapshot.
    fn action_key(&mut self, key: KeyEvent) -> Option<ActionIntent> {
        if key.kind != KeyEventKind::Press
            || key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
            || self.quitting
            || matches!(self.pending, Some(Operation::Attach | Operation::Detach))
        {
            return None;
        }
        let action = match key.code {
            KeyCode::Enter | KeyCode::Char('a' | 'A') => DeviceAction::Attach,
            KeyCode::Char('d' | 'D') => DeviceAction::Detach,
            _ => return None,
        };
        let Some(device) = self.selected().cloned() else {
            self.notice = Notice::info("请使用 ↑ / ↓ 选择一个已连接的 USB 设备。".into());
            return None;
        };
        let intent = ActionIntent { action, device };
        if self.pending == Some(Operation::Refresh) {
            if self.queued.is_none() {
                self.queued = Some(intent);
            }
            None
        } else {
            Some(intent)
        }
    }

    fn request_quit(&mut self) {
        self.quitting = true;
        self.queued = None;
    }

    fn reconcile(&mut self, devices: Vec<Device>) {
        let previous = self
            .selected()
            .map(|device| (device.bus_id.clone(), device.instance_id.clone()));
        let selected = if let Some((bus_id, instance_id)) = previous {
            devices
                .iter()
                .position(|device| device.bus_id == bus_id && device.instance_id == instance_id)
        } else if !self.loaded || self.devices.is_empty() {
            (!devices.is_empty()).then_some(0)
        } else {
            None
        };
        self.devices = devices;
        self.table.select(selected);
        self.loaded = true;
    }

    fn complete(&mut self, response: Response) -> Option<ActionIntent> {
        self.pending = None;
        self.last_refresh = Instant::now();
        let refresh_error = match response.devices {
            Ok(devices) => {
                self.reconcile(devices);
                None
            }
            Err(error) => {
                // Keep old rows for context, but require explicit reselection
                // before any action after a failed refresh.
                self.table.select(None);
                Some(error)
            }
        };
        let refreshed = refresh_error.is_none();
        let notice = match (response.action, refresh_error) {
            (Some(Err(action)), Some(refresh)) => Some(Notice::error(format!(
                "{action}；同时无法刷新设备：{refresh}"
            ))),
            (Some(Err(action)), None) => Some(Notice::error(action)),
            (Some(Ok(action)), Some(refresh)) => Some(Notice::error(format!(
                "{action}；刷新失败，请按 R 重试：{refresh}"
            ))),
            (Some(Ok(action)), None) => Some(Notice::success(action)),
            (None, Some(refresh)) => Some(Notice::error(format!("刷新失败：{refresh}"))),
            (None, None) => None,
        };
        if let Some(notice) = notice {
            self.notice = notice;
        }
        let intent = self.queued.take()?;
        if self.quitting {
            return None;
        }
        if !refreshed {
            self.notice.text.push_str(&format!(
                "；已取消待执行的{}，请按 R 刷新后重试。",
                intent.action.label()
            ));
            return None;
        }
        if !self.devices.iter().any(|device| {
            device.bus_id == intent.device.bus_id && device.instance_id == intent.device.instance_id
        }) {
            self.notice = Notice::error(format!(
                "待操作的设备 {} 已移除或被替换，已取消{}；请重新选择设备。",
                intent.device.bus_id,
                intent.action.label()
            ));
            return None;
        }
        Some(intent)
    }
}

enum Request {
    Refresh,
    Attach {
        device: Device,
        distribution: String,
    },
    Detach(Device),
    Stop,
}

struct Response {
    devices: std::result::Result<Vec<Device>, String>,
    action: Option<std::result::Result<String, String>>,
}

struct Worker {
    requests: mpsc::Sender<Request>,
    responses: mpsc::Receiver<Response>,
    handle: Option<JoinHandle<()>>,
}

impl Worker {
    fn start<B: Backend + 'static>(mut backend: B) -> Result<Self> {
        let (requests, request_rx) = mpsc::channel();
        let (response_tx, responses) = mpsc::channel();
        let handle = thread::Builder::new()
            .name("usb-operations".into())
            .spawn(move || {
                while let Ok(request) = request_rx.recv() {
                    let action = match request {
                        Request::Refresh => None,
                        Request::Attach {
                            device,
                            distribution,
                        } => Some(backend.attach(&device, &distribution)),
                        Request::Detach(device) => Some(backend.detach(&device)),
                        Request::Stop => break,
                    };
                    let response = Response {
                        action: action.map(|result| result.map_err(|error| format!("{error:#}"))),
                        devices: backend.list_devices().map_err(|error| format!("{error:#}")),
                    };
                    if response_tx.send(response).is_err() {
                        break;
                    }
                }
            })
            .context("无法启动设备操作线程")?;
        Ok(Self {
            requests,
            responses,
            handle: Some(handle),
        })
    }

    fn send(&self, request: Request) -> Result<()> {
        self.requests.send(request).context("无法提交设备操作")
    }

    fn stop(&mut self) -> Result<()> {
        let _ = self.requests.send(Request::Stop);
        if let Some(handle) = self.handle.take() {
            handle
                .join()
                .map_err(|_| anyhow::anyhow!("设备操作线程异常退出"))?;
        }
        Ok(())
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

/// Remove terminal control characters from OS / USB-provided text.
fn printable(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect()
}

fn render(frame: &mut Frame<'_>, state: &mut UiState, env: &Environment, distro: &str) {
    let area = frame.area();
    if area.width < MIN_WIDTH || area.height < MIN_HEIGHT {
        frame.render_widget(
            Paragraph::new(format!(
                "USB → WSL2\n请放大终端至 {MIN_WIDTH} 列 × {MIN_HEIGHT} 行。\nQ / Esc 退出{}",
                if state.quitting {
                    "；正在等待当前操作完成"
                } else {
                    ""
                }
            ))
            .wrap(Wrap { trim: false }),
            area,
        );
        return;
    }

    let areas = Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(4),
        Constraint::Length(4),
        Constraint::Length(4),
        Constraint::Length(2),
    ])
    .split(area);
    let version = env
        .usbipd_version
        .split('+')
        .next()
        .unwrap_or(&env.usbipd_version);
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("usbipd ", Style::default().fg(Color::DarkGray)),
            Span::raw(printable(version)),
            Span::raw("    WSL2: "),
            Span::styled(printable(distro), Style::default().fg(Color::Cyan)),
        ]))
        .block(Block::default().borders(Borders::ALL).title(" USB → WSL2 ")),
        areas[0],
    );

    let rows = state.devices.iter().map(|device| {
        let status_color = if device.client_ip.is_some() {
            Color::LightGreen
        } else if device.shared {
            Color::Cyan
        } else {
            Color::DarkGray
        };
        Row::new(vec![
            Cell::from(device.bus_id.to_string()),
            Cell::from(device.vid_pid()),
            Cell::from(printable(&device.description)),
            Cell::from(device.status_label()).style(Style::default().fg(status_color)),
        ])
    });
    let table = Table::new(
        rows,
        [
            Constraint::Length(9),
            Constraint::Length(9),
            Constraint::Min(10),
            Constraint::Length(8),
        ],
    )
    .header(
        Row::new(["BUSID", "VID:PID", "USB 设备", "状态"]).style(
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
    )
    .block(
        Block::default()
            .borders(Borders::ALL)
            .title(format!(" 已连接设备 ({}) ", state.devices.len())),
    )
    .row_highlight_style(
        Style::default()
            .bg(Color::DarkGray)
            .add_modifier(Modifier::BOLD),
    )
    .highlight_symbol("› ")
    .column_spacing(1);
    frame.render_stateful_widget(table, areas[1], &mut state.table);

    let detail = match state.selected() {
        Some(device) => format!(
            "{}\n{}{}",
            printable(&device.description),
            printable(&device.instance_id),
            device
                .client_ip
                .as_ref()
                .map_or_else(String::new, |ip| format!("  客户端: {}", printable(ip)))
        ),
        None if state.devices.is_empty() && state.loaded => {
            "未发现已连接的 USB 设备。插入设备后按 R，或等待自动刷新。".into()
        }
        None if !state.loaded => "正在查询 USB 设备…".into(),
        None => "当前未选择设备，请按 ↑ / ↓ 重新选择。".into(),
    };
    frame.render_widget(
        Paragraph::new(detail)
            .wrap(Wrap { trim: true })
            .block(Block::default().borders(Borders::ALL).title(" 设备详情 ")),
        areas[2],
    );

    let (message, color) = if state.quitting {
        ("正在等待当前操作结束，随后退出…".to_owned(), Color::Yellow)
    } else if let Some(operation) = state.pending {
        let spinner = ['|', '/', '-', '\\'][state.tick % 4];
        let message = state.queued.as_ref().map_or_else(
            || format!("{spinner} {}…", operation.label()),
            |intent| {
                format!(
                    "{spinner} 正在刷新设备；完成后{} {}（Q 可取消）…",
                    intent.action.label(),
                    intent.device.bus_id
                )
            },
        );
        (message, Color::Yellow)
    } else {
        (printable(&state.notice.text), state.notice.color)
    };
    frame.render_widget(
        Paragraph::new(message)
            .style(Style::default().fg(color))
            .wrap(Wrap { trim: true })
            .block(Block::default().borders(Borders::ALL).title(" 操作结果 ")),
        areas[3],
    );
    frame.render_widget(
        Paragraph::new(vec![
            Line::from("↑↓ / J K 选择  Enter / A 映射  D 断开  R 刷新  Q 退出"),
            Line::styled(
                "映射期间 Windows 无法使用该设备；设备对所有 WSL2 发行版可见。",
                Style::default().fg(Color::DarkGray),
            ),
        ]),
        areas[4],
    );
}

type PanicHook = dyn Fn(&PanicHookInfo<'_>) + Send + Sync + 'static;

struct TerminalSession {
    terminal: Terminal<CrosstermBackend<Stdout>>,
    previous_hook: Arc<PanicHook>,
}

impl TerminalSession {
    fn enter() -> Result<Self> {
        enable_raw_mode().context("无法启用终端键盘模式")?;
        let mut stdout = io::stdout();
        if let Err(error) = execute!(stdout, EnterAlternateScreen, Hide) {
            restore_terminal();
            return Err(error).context("无法进入终端界面");
        }
        let terminal = match Terminal::new(CrosstermBackend::new(stdout)) {
            Ok(terminal) => terminal,
            Err(error) => {
                restore_terminal();
                return Err(error).context("无法初始化终端界面");
            }
        };
        let previous_hook: Arc<PanicHook> = Arc::from(panic::take_hook());
        let hook = Arc::clone(&previous_hook);
        panic::set_hook(Box::new(move |info| {
            restore_terminal();
            hook(info);
        }));
        Ok(Self {
            terminal,
            previous_hook,
        })
    }
}

fn restore_terminal() {
    let _ = disable_raw_mode();
    let _ = execute!(io::stdout(), Show, LeaveAlternateScreen);
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        restore_terminal();
        if !thread::panicking() {
            let previous = Arc::clone(&self.previous_hook);
            panic::set_hook(Box::new(move |info| previous(info)));
        }
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;

    fn device(bus_id: &str, instance_id: &str) -> Device {
        Device {
            bus_id: bus_id.parse().expect("valid test BUSID"),
            description: "测试 USB 设备".into(),
            instance_id: instance_id.into(),
            shared: false,
            client_ip: None,
        }
    }

    #[test]
    fn selection_tracks_identity_when_rows_move() {
        let mut state = UiState::new();
        state.reconcile(vec![device("2-1", "a"), device("2-2", "b")]);
        state.move_selection(true);
        state.reconcile(vec![device("2-2", "b"), device("2-3", "c")]);
        assert_eq!(state.table.selected(), Some(0));
        assert_eq!(
            state.selected().expect("selection retained").instance_id,
            "b"
        );
    }

    #[test]
    fn replaced_device_at_same_bus_does_not_inherit_selection() {
        let mut state = UiState::new();
        state.reconcile(vec![device("2-1", "original")]);
        state.reconcile(vec![device("2-1", "replacement")]);
        assert_eq!(state.table.selected(), None);
        state.reconcile(vec![device("2-1", "replacement")]);
        assert_eq!(state.table.selected(), None);
        state.move_selection(true);
        assert_eq!(state.table.selected(), Some(0));
    }

    #[test]
    fn empty_lists_and_navigation_remain_in_bounds() {
        let mut state = UiState::new();
        state.move_selection(true);
        state.move_selection(false);
        state.select_last();
        assert_eq!(state.table.selected(), None);
        state.reconcile(vec![device("2-1", "a")]);
        state.move_selection(true);
        state.move_selection(false);
        assert_eq!(state.table.selected(), Some(0));
        state.reconcile(Vec::new());
        assert_eq!(state.table.selected(), None);
    }

    #[test]
    fn failed_refresh_does_not_hide_action_failure() {
        let mut state = UiState::new();
        state.reconcile(vec![device("2-1", "a")]);
        let _ = state.complete(Response {
            action: Some(Err("UAC 已取消".into())),
            devices: Err("USB 服务不可用".into()),
        });
        assert!(state.notice.text.contains("UAC 已取消"));
        assert!(state.notice.text.contains("USB 服务不可用"));
        assert_eq!(state.table.selected(), None);
        assert_eq!(state.devices.len(), 1);
    }

    #[test]
    fn routine_refresh_preserves_last_operation_message() {
        let mut state = UiState::new();
        let _ = state.complete(Response {
            action: Some(Ok("映射成功".into())),
            devices: Ok(vec![device("2-1", "a")]),
        });
        let _ = state.complete(Response {
            action: None,
            devices: Ok(vec![device("2-1", "a")]),
        });
        assert_eq!(state.notice.text, "映射成功");
    }

    #[test]
    fn renders_unicode_and_tiny_terminals_without_panicking() -> Result<()> {
        let env = Environment {
            usbipd_version: "5.3.0+test".into(),
            distributions: Vec::new(),
        };
        for (width, height) in [(100, 24), (60, 17), (20, 5), (1, 1)] {
            let mut state = UiState::new();
            let mut usb = device("2-1", "USB\\VID_1234&PID_5678\\测试");
            usb.description = "很长的中文 USB 设备名称🦀".repeat(20);
            state.reconcile(vec![usb]);
            let mut terminal = Terminal::new(TestBackend::new(width, height))?;
            terminal.draw(|frame| render(frame, &mut state, &env, "Ubuntu 测试"))?;
            let output = format!("{:?}", terminal.backend().buffer());
            assert!(output.contains("USB") || width == 1);
        }
        Ok(())
    }

    #[test]
    fn device_names_cannot_emit_terminal_control_sequences() {
        assert_eq!(printable("USB\u{1b}[2J\r\n\t设备"), "USB [2J   设备");
    }

    #[test]
    fn one_press_during_refresh_keeps_original_target_and_dispatches_once() {
        let mut state = UiState::new();
        state.reconcile(vec![device("2-1", "a"), device("2-2", "b")]);
        state.pending = Some(Operation::Refresh);
        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        assert!(state.action_key(enter).is_none());
        state.move_selection(true);
        assert!(
            state
                .action_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE))
                .is_none()
        );

        let intent = state
            .complete(Response {
                action: None,
                devices: Ok(vec![device("2-2", "b"), device("2-1", "a")]),
            })
            .expect("the queued Enter must execute after refresh");
        assert_eq!(intent.action, DeviceAction::Attach);
        assert_eq!(intent.device.instance_id, "a");
        assert!(state.queued.is_none());
        assert!(
            state
                .complete(Response {
                    action: None,
                    devices: Ok(vec![device("2-1", "a")]),
                })
                .is_none()
        );
    }

    #[test]
    fn failed_refresh_cancels_queued_mutation_with_retry_guidance() {
        let mut state = UiState::new();
        state.reconcile(vec![device("2-1", "a")]);
        state.pending = Some(Operation::Refresh);
        assert!(
            state
                .action_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE))
                .is_none()
        );
        assert!(
            state
                .complete(Response {
                    action: None,
                    devices: Err("USB 服务不可用".into()),
                })
                .is_none()
        );
        assert!(state.queued.is_none());
        assert!(state.notice.text.contains("USB 服务不可用"));
        assert!(state.notice.text.contains("已取消待执行的断开"));
        assert!(state.notice.text.contains("按 R"));
    }

    #[test]
    fn removed_or_replaced_target_cancels_queued_mutation() {
        for replacement in [Vec::new(), vec![device("2-1", "replacement")]] {
            let mut state = UiState::new();
            state.reconcile(vec![device("2-1", "original")]);
            state.pending = Some(Operation::Refresh);
            assert!(
                state
                    .action_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
                    .is_none()
            );
            assert!(
                state
                    .complete(Response {
                        action: None,
                        devices: Ok(replacement),
                    })
                    .is_none()
            );
            assert!(state.queued.is_none());
            assert!(state.notice.text.contains("已移除或被替换"));
            assert!(state.notice.text.contains("已取消映射"));
        }
    }

    #[test]
    fn quit_cancels_queued_intent_without_starting_mutation() {
        let mut state = UiState::new();
        state.reconcile(vec![device("2-1", "a")]);
        state.pending = Some(Operation::Refresh);
        assert!(
            state
                .action_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
                .is_none()
        );
        state.request_quit();
        assert!(state.queued.is_none());
        assert!(state.pending == Some(Operation::Refresh));
        assert!(
            state
                .complete(Response {
                    action: None,
                    devices: Ok(vec![device("2-1", "a")]),
                })
                .is_none()
        );
        assert!(state.quitting);
    }

    #[test]
    fn repeats_and_busy_mutations_cannot_queue_actions() {
        let mut state = UiState::new();
        state.reconcile(vec![device("2-1", "a")]);
        state.pending = Some(Operation::Refresh);
        let mut enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        enter.kind = KeyEventKind::Repeat;
        assert!(state.action_key(enter).is_none());
        assert!(state.queued.is_none());
        for operation in [Operation::Attach, Operation::Detach] {
            state.pending = Some(operation);
            assert!(
                state
                    .action_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
                    .is_none()
            );
            assert!(state.queued.is_none());
        }
    }

    struct FailingBackend;

    impl Backend for FailingBackend {
        fn inspect(&mut self) -> Result<Environment> {
            bail!("unused")
        }

        fn list_devices(&mut self) -> Result<Vec<Device>> {
            bail!("refresh error")
        }

        fn attach(&mut self, _device: &Device, _distribution: &str) -> Result<String> {
            bail!("attach error")
        }

        fn detach(&mut self, _device: &Device) -> Result<String> {
            bail!("detach error")
        }
    }

    #[test]
    fn worker_returns_action_and_refresh_errors_independently() -> Result<()> {
        let mut worker = Worker::start(FailingBackend)?;
        worker.send(Request::Attach {
            device: device("2-1", "a"),
            distribution: "Ubuntu".into(),
        })?;
        let response = worker.responses.recv_timeout(Duration::from_secs(2))?;
        assert_eq!(response.action, Some(Err("attach error".into())));
        assert_eq!(response.devices.err().as_deref(), Some("refresh error"));
        worker.stop()?;
        Ok(())
    }
}
