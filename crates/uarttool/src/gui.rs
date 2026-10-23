//! 托盘常驻界面（windui 常驻模式）：托盘菜单 + 按需打开的主窗口。
//!
//! 服务运行在后台 tokio 运行时里，`Service` 本身线程安全，UI 线程用定时器读取快照写进信号，
//! 不需要跨线程消息。

use std::cell::{Cell, RefCell};
use std::path::Path;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use uart_core::config::{Config, Encoding, PortConfig, TerminalMode};
use uart_core::devices;
use uart_core::port::{PortSnapshot, PortState};
use uart_core::service::Service;
use windui::prelude::*;

use crate::{autostart, icon};

/// 单实例标识，`main` 里的提前仲裁与 `App::single_instance` 必须一致。
pub const APP_ID: &str = "uarttool";
const DANGER: u32 = 0xDC2626;
/// 表格行里放了 `.small()` 按钮，行高按「按钮高 + 上下内边距」算。
const ROW_H: i32 = 49;
const MODES: [&str; 2] = ["telnet", "raw"];
const ENCODINGS: [&str; 2] = ["utf-8", "gbk"];
/// 对话框（宽 520）里表单控件的宽度：长字段与短字段。
const FIELD_W: i32 = 340;
const SHORT_W: i32 = 160;

#[derive(Clone)]
struct Gui {
    service: Arc<Service>,
    runtime: tokio::runtime::Handle,
    /// 主窗口表格的数据
    rows: Signal<Vec<Vec<String>>>,
    /// 主窗口顶部的一行概要
    summary: Signal<String>,
    autostart: Signal<bool>,
    /// 配置文件解析失败的错误信息；非空时禁止在界面里保存配置
    load_error: Signal<String>,
}

pub fn run(
    service: Arc<Service>,
    runtime: tokio::runtime::Handle,
    load_error: Option<String>,
    open_window: bool,
) {
    let gui = Gui {
        service,
        runtime,
        rows: signal(Vec::new()),
        summary: signal(String::new()),
        autostart: signal(autostart::is_enabled()),
        load_error: signal(load_error.unwrap_or_default()),
    };
    let (tooltip, online) = gui.refresh();

    let tray = Tray::new()
        .tooltip(tooltip.clone())
        .icon_rgba(32, 32, icon::render(32, Some(online)).rgba())
        .on_left_click({
            let gui = gui.clone();
            move |ctx| ctx.open_window(gui.main_window())
        })
        .menu(tray_menu(&gui));

    let mut app = App::resident("uarttool")
        .icon(IconSource::sized(|size| icon::render(size, None)))
        .tray(tray);
    let tray_handle = app.tray_handle();

    // 第二次启动时打开主窗口。单实例回调拿不到 ctx，记一个标志交给定时器处理。
    let pending_open = Rc::new(Cell::new(false));
    app = app.single_instance(APP_ID, {
        let pending_open = pending_open.clone();
        move |_argv| pending_open.set(true)
    });

    let last_tray = RefCell::new((tooltip, online));
    let ticks = Cell::new(0u32);
    app = app.on_interval(Duration::from_millis(500), {
        let gui = gui.clone();
        move |ctx| {
            if pending_open.replace(false) {
                ctx.open_window(gui.main_window());
            }
            ticks.set(ticks.get().wrapping_add(1));
            if !ticks.get().is_multiple_of(2) {
                return;
            }
            let (tooltip, online) = gui.refresh();
            let mut last = last_tray.borrow_mut();
            if last.0 != tooltip {
                tray_handle.set_tooltip(tooltip.clone());
            }
            if last.1 != online {
                tray_handle.set_icon_rgba(32, 32, icon::render(32, Some(online)).rgba());
            }
            *last = (tooltip, online);
        }
    });

    if open_window {
        app = app.start_window(gui.main_window());
    }
    app.screenshot_window(gui.main_window())
        .screenshot_from_args()
        .run_resident();
}

fn tray_menu(gui: &Gui) -> Vec<TrayMenuItem> {
    let g = gui.clone();
    let open = move |ctx: &mut TrayCtx| ctx.open_window(g.main_window());
    let g = gui.clone();
    let reload = move |ctx: &mut TrayCtx| match g.reload_from_file() {
        Ok(()) => ctx.notify("uarttool", "已重新加载配置"),
        Err(e) => ctx.notify("重新加载配置失败", &format!("{e:#}")),
    };
    let g = gui.clone();
    let toggle_autostart = move |ctx: &mut TrayCtx| {
        let want = !g.autostart.get();
        match autostart::set_enabled(want) {
            Ok(()) => g.autostart.set(want),
            Err(e) => ctx.notify("设置开机自启失败", &format!("{e:#}")),
        }
    };
    let config_path = gui.service.config_path().to_path_buf();
    let g = gui.clone();
    vec![
        TrayMenuItem::item("打开主窗口", open),
        TrayMenuItem::separator(),
        TrayMenuItem::item("打开日志目录", move |_| g.open_log_dir()),
        TrayMenuItem::item("打开配置文件", move |_| open_config_file(&config_path)),
        TrayMenuItem::item("重新加载配置", reload),
        TrayMenuItem::separator(),
        TrayMenuItem::check("开机自启", gui.autostart, toggle_autostart),
        TrayMenuItem::separator(),
        TrayMenuItem::item("退出", |ctx| ctx.quit()),
    ]
}

impl Gui {
    /// 读取服务快照刷新表格和概要；返回（托盘提示文字，是否有设备在线）。
    fn refresh(&self) -> (String, bool) {
        let config = self.service.config();
        let snapshots = self.service.snapshots();
        let rows: Vec<Vec<String>> = config
            .ports
            .iter()
            .map(|p| match snapshots.iter().find(|s| s.name == p.name) {
                Some(s) => snapshot_row(s),
                None => disabled_row(p),
            })
            .collect();
        if self.rows.get() != rows {
            self.rows.set(rows);
        }

        let online = snapshots
            .iter()
            .filter(|s| matches!(s.state, PortState::Connected { .. }))
            .count();
        let clients: usize = snapshots.iter().map(|s| s.clients).sum();
        let summary = format!(
            "HTTP 接口 {}    ·    {online}/{} 个串口在线    ·    {clients} 个终端连接",
            self.http_url(),
            config.ports.len()
        );
        if self.summary.get() != summary {
            self.summary.set(summary);
        }
        let tooltip = format!("uarttool — {online}/{} 个串口在线", config.ports.len());
        (tooltip, online > 0)
    }

    fn http_url(&self) -> String {
        let server = self.service.config().server;
        let host = match server.bind.as_str() {
            "0.0.0.0" | "::" => local_ip().unwrap_or_else(|| "127.0.0.1".into()),
            other => other.to_string(),
        };
        format!("http://{host}:{}/", server.http_port)
    }

    /// 界面修改配置的基准：重新读取配置文件，而不是用内存里的配置，
    /// 这样用户在文件里的手工修改（还没点重新加载）不会被界面保存冲掉。
    ///
    /// 配置文件解析失败时拒绝：此时内存里是空配置，保存会覆盖用户的原文件。
    fn editable_config(&self) -> Result<Config, String> {
        // 详细错误显示在主窗口顶部，这里只给一句提示，避免撑破对话框。
        const HINT: &str = "配置文件有错误，请先修正后点「重新加载配置」（详情见主窗口顶部）";
        if !self.load_error.with(|e| e.is_empty()) {
            return Err(HINT.into());
        }
        Config::load(self.service.config_path()).map_err(|e| {
            self.load_error.set(format!("{e:#}"));
            HINT.to_string()
        })
    }

    /// 保存配置文件并让服务应用新配置。
    fn apply_config(&self, config: Config) -> Result<()> {
        config.save(self.service.config_path())?;
        let _guard = self.runtime.enter();
        self.service.reload(config)?;
        self.refresh();
        Ok(())
    }

    fn reload_from_file(&self) -> Result<()> {
        let config = Config::load(self.service.config_path()).inspect_err(|e| {
            self.load_error.set(format!("{e:#}"));
        })?;
        let _guard = self.runtime.enter();
        self.service.reload(config)?;
        self.load_error.set(String::new());
        self.refresh();
        Ok(())
    }

    fn open_log_dir(&self) {
        let dir = self.service.config().log_dir(self.service.config_path());
        let _ = std::fs::create_dir_all(&dir);
        let _ = std::process::Command::new("explorer").arg(&dir).spawn();
    }

    fn main_window(&self) -> WindowRequest {
        let gui = self.clone();
        Window::new("uarttool", 900, 520)
            .single("main")
            .centered(true)
            .min_size(720, 360)
            .icon(IconSource::sized(|size| icon::render(size, None)))
            .content(move || gui.main_view())
    }

    fn main_view(&self) -> Element {
        let editor = Editor::new(self.clone());
        let confirm = Confirm::new(self.clone());
        let load_error = self.load_error;
        let load_error_text = load_error.map(|e| format!("配置文件解析失败，修正后点「重新加载配置」：{e}"));

        let header = Element::row()
            .width_match()
            .spacing(8)
            .child(Element::label("串口共享").font_size(20.0).height(32))
            .child(Element::flex_spacer())
            .child(Element::button("添加串口").small().on_click({
                let editor = editor.clone();
                move |_| editor.open(None)
            }))
            .child(Element::button("重新加载配置").small().outline().neutral().on_click({
                let gui = self.clone();
                move |ctx| match gui.reload_from_file() {
                    Ok(()) => ctx.toast_ok("已重新加载配置"),
                    Err(e) => ctx.toast_err(format!("{e:#}")),
                }
            }));

        let rows = self.rows;
        let table = Element::table_virtual(
            vec![
                ("名称", 1.2),
                ("状态", 1.7),
                ("波特率", 0.8),
                ("终端端口", 1.1),
                ("连接数", 0.7),
                ("接收", 0.8),
                ("发送", 0.8),
            ],
            rows,
            ROW_H,
        )
        .actions("操作", 1.3, {
            let editor = editor.clone();
            let confirm = confirm.clone();
            move |row| {
                let name = rows.get().get(row).map(|r| r[0].clone()).unwrap_or_default();
                let editor = editor.clone();
                let confirm = confirm.clone();
                let edit_name = name.clone();
                Element::row()
                    .spacing(6)
                    .child(
                        Element::button("编辑")
                            .small()
                            .outline()
                            .on_click(move |_| editor.open(Some(edit_name.clone()))),
                    )
                    .child(
                        Element::button("删除")
                            .small()
                            .outline()
                            .danger()
                            .on_click(move |_| confirm.open(name.clone())),
                    )
            }
        })
        .width_match()
        .weight(1.0);

        let empty_hint = Element::label("还没有共享的串口，点「添加串口」开始。")
            .fg_role(Role::TextMuted)
            .width_match()
            .visible_when(move || rows.with(|r| r.is_empty()));

        let footer = Element::row()
            .width_match()
            .spacing(8)
            .child(Element::button("复制 HTTP 地址").small().outline().neutral().on_click({
                let gui = self.clone();
                move |ctx| {
                    ctx.clipboard_set(&gui.http_url());
                    ctx.toast_ok("已复制 HTTP 地址");
                }
            }))
            .child(Element::button("打开日志目录").small().outline().neutral().on_click({
                let gui = self.clone();
                move |_| gui.open_log_dir()
            }))
            .child(Element::button("打开配置文件").small().outline().neutral().on_click({
                let path = self.service.config_path().to_path_buf();
                move |_| open_config_file(&path)
            }))
            .child(Element::flex_spacer())
            .child(Element::checkbox("开机自启", self.autostart).on_toggle({
                let autostart = self.autostart;
                move |ctx| {
                    let want = !autostart.get();
                    match autostart::set_enabled(want) {
                        Ok(()) => autostart.set(want),
                        Err(e) => ctx.toast_err(format!("设置开机自启失败: {e:#}")),
                    }
                }
            }));

        Element::stack()
            .fill()
            .child(
                Element::col()
                    .fill()
                    .padding(16)
                    .spacing(10)
                    .child(header)
                    .child(Element::label_signal(self.summary).fg_role(Role::TextMuted).width_match())
                    .child(
                        Element::label_signal(load_error_text)
                            .fg(Color::hex(DANGER))
                            .width_match()
                            .visible_when(move || load_error.with(|e| !e.is_empty())),
                    )
                    .child(empty_hint)
                    .child(table)
                    .child(footer),
            )
            .child(editor.view())
            .child(confirm.view())
    }
}

fn snapshot_row(s: &PortSnapshot) -> Vec<String> {
    let state = match &s.state {
        PortState::Connected { com } => format!("● 已连接 {com}"),
        PortState::Waiting => "○ 等待设备接入".into(),
        PortState::Error(e) => format!("✕ {e}"),
    };
    vec![
        s.name.clone(),
        state,
        s.baud.to_string(),
        format!("{} {}", s.tcp_port, mode_name(s.mode)),
        s.clients.to_string(),
        human_bytes(s.rx_bytes),
        human_bytes(s.tx_bytes),
    ]
}

fn disabled_row(p: &PortConfig) -> Vec<String> {
    vec![
        p.name.clone(),
        "已停用".into(),
        p.baud.to_string(),
        format!("{} {}", p.tcp_port, mode_name(p.mode)),
        "-".into(),
        "-".into(),
    ]
}

fn mode_name(mode: TerminalMode) -> &'static str {
    match mode {
        TerminalMode::Telnet => "telnet",
        TerminalMode::Raw => "raw",
    }
}

fn human_bytes(n: u64) -> String {
    match n {
        0..1024 => format!("{n} B"),
        1024..1_048_576 => format!("{:.1} KB", n as f64 / 1024.0),
        _ => format!("{:.1} MB", n as f64 / 1_048_576.0),
    }
}

/// 本机的局域网 IP：向外部地址"连接"一个 UDP 套接字（不发包），读它选中的本地地址。
fn local_ip() -> Option<String> {
    let socket = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("10.255.255.255:1").ok()?;
    Some(socket.local_addr().ok()?.ip().to_string())
}

/// 用默认程序打开配置文件；文件还不存在时先写一份默认配置。
fn open_config_file(path: &Path) {
    if !path.exists() {
        let _ = Config::default().save(path);
    }
    let _ = std::process::Command::new("explorer").arg(path).spawn();
}

/// 设备下拉框的一项。
#[derive(Clone)]
struct DeviceChoice {
    label: String,
    serial_number: Option<String>,
    com: Option<String>,
}

/// 添加 / 编辑串口的对话框。
#[derive(Clone)]
struct Editor {
    gui: Gui,
    show: Signal<bool>,
    title: Signal<String>,
    /// 正在编辑的串口原名；`None` 表示新增
    original: Rc<RefCell<Option<String>>>,
    choices: Rc<RefCell<Vec<DeviceChoice>>>,
    device_labels: Signal<Vec<String>>,
    device: Signal<usize>,
    name: Signal<String>,
    baud: Signal<String>,
    tcp_port: Signal<String>,
    mode: Signal<usize>,
    encoding: Signal<usize>,
    enabled: Signal<bool>,
    error: Signal<String>,
}

impl Editor {
    fn new(gui: Gui) -> Self {
        Self {
            gui,
            show: signal(false),
            title: signal(String::new()),
            original: Rc::new(RefCell::new(None)),
            choices: Rc::new(RefCell::new(Vec::new())),
            device_labels: signal(Vec::new()),
            device: signal(0),
            name: signal(String::new()),
            baud: signal(String::new()),
            tcp_port: signal(String::new()),
            mode: signal(0),
            encoding: signal(0),
            enabled: signal(true),
            error: signal(String::new()),
        }
    }

    /// 打开对话框：`name` 为 `None` 时新增，否则编辑该串口。
    fn open(&self, name: Option<String>) {
        let config = self.gui.editable_config().unwrap_or_else(|_| self.gui.service.config());
        let existing = name
            .as_deref()
            .and_then(|n| config.ports.iter().find(|p| p.name == n))
            .cloned();

        let devices = devices::list_devices();
        let mut choices: Vec<DeviceChoice> = devices
            .iter()
            .map(|d| DeviceChoice {
                label: [
                    Some(d.com.clone()),
                    d.serial_number.as_ref().map(|sn| format!("序列号 {sn}")),
                    d.description.as_deref().map(|s| s.trim_end_matches(&format!(" ({})", d.com)).to_string()),
                ]
                .into_iter()
                .flatten()
                .collect::<Vec<_>>()
                .join("  ·  "),
                serial_number: d.serial_number.clone(),
                com: Some(d.com.clone()),
            })
            .collect();

        let mut selected = 0;
        if let Some(p) = &existing {
            match devices::find_device(p, &devices) {
                Some(d) => {
                    selected = devices.iter().position(|x| x.com == d.com).unwrap_or(0);
                    // 选中的就是当前匹配到的设备：保留原来的匹配规则，原样保存时配置不变、串口不重启
                    choices[selected].serial_number = p.serial_number.clone();
                    choices[selected].com = p.com.clone();
                }
                None => {
                    let rule = match (&p.serial_number, &p.com) {
                        (Some(sn), _) => format!("序列号 {sn}"),
                        (None, Some(com)) => com.clone(),
                        (None, None) => "未配置".into(),
                    };
                    choices.insert(
                        0,
                        DeviceChoice {
                            label: format!("{rule}（当前未接入）"),
                            serial_number: p.serial_number.clone(),
                            com: p.com.clone(),
                        },
                    );
                }
            }
        }

        let next_port = config.ports.iter().map(|p| p.tcp_port).max().map_or(7001, |p| p + 1);
        let p = existing.clone().unwrap_or(PortConfig {
            name: String::new(),
            serial_number: None,
            com: None,
            baud: 115_200,
            tcp_port: next_port,
            mode: TerminalMode::Telnet,
            encoding: Encoding::Utf8,
            enabled: true,
        });

        self.device_labels.set(choices.iter().map(|c| c.label.clone()).collect());
        *self.choices.borrow_mut() = choices;
        *self.original.borrow_mut() = name.clone();
        self.title.set(if name.is_some() { "编辑串口".into() } else { "添加串口".into() });
        self.device.set(selected);
        self.name.set(p.name);
        self.baud.set(p.baud.to_string());
        self.tcp_port.set(p.tcp_port.to_string());
        self.mode.set(matches!(p.mode, TerminalMode::Raw) as usize);
        self.encoding.set(matches!(p.encoding, Encoding::Gbk) as usize);
        self.enabled.set(p.enabled);
        self.error.set(String::new());
        self.show.set(true);
    }

    fn save(&self) -> Result<(), String> {
        let mut config = self.gui.editable_config()?;
        let name = self.name.get().trim().to_string();
        if name.is_empty() {
            return Err("请填写名称".into());
        }
        let choice = self
            .choices
            .borrow()
            .get(self.device.get())
            .cloned()
            .ok_or("请选择设备（没有可选设备时请先接入设备）")?;
        let baud: u32 = self.baud.get().trim().parse().map_err(|_| "波特率必须是正整数")?;
        let tcp_port: u16 = self
            .tcp_port
            .get()
            .trim()
            .parse()
            .map_err(|_| "TCP 端口必须是 1–65535 的整数")?;
        let port = PortConfig {
            name,
            serial_number: choice.serial_number,
            com: choice.com,
            baud,
            tcp_port,
            mode: if self.mode.get() == 1 { TerminalMode::Raw } else { TerminalMode::Telnet },
            encoding: if self.encoding.get() == 1 { Encoding::Gbk } else { Encoding::Utf8 },
            enabled: self.enabled.get(),
        };

        match self.original.borrow().as_deref() {
            Some(original) => match config.ports.iter_mut().find(|p| p.name == original) {
                Some(slot) => *slot = port,
                None => config.ports.push(port),
            },
            None => config.ports.push(port),
        }
        config.validate().map_err(|e| format!("{e:#}"))?;
        self.gui.apply_config(config).map_err(|e| format!("{e:#}"))
    }

    fn view(&self) -> Element {
        let show = self.show;
        let error = self.error;

        let body = Element::col()
            .width_match()
            .spacing(8)
            .child(Element::field("名称", Element::text_input(self.name, "例如 esp32").width(FIELD_W)))
            .child(Element::field(
                "设备",
                Element::dropdown_signal(self.device_labels, self.device).width(FIELD_W),
            ))
            .child(Element::field("波特率", Element::text_input(self.baud, "115200").width(SHORT_W)))
            .child(Element::field("TCP 端口", Element::text_input(self.tcp_port, "7001").width(SHORT_W)))
            .child(Element::field("终端模式", Element::dropdown(MODES.to_vec(), self.mode).width(SHORT_W)))
            .child(Element::field("编码", Element::dropdown(ENCODINGS.to_vec(), self.encoding).width(SHORT_W)))
            .child(Element::field("启用", Element::switch(self.enabled)))
            .child(
                Element::label_signal(error)
                    .fg(Color::hex(DANGER))
                    .width_match()
                    .visible_when(move || error.with(|e| !e.is_empty())),
            );

        let editor = self.clone();
        let footer = Element::row()
            .width_match()
            .spacing(8)
            .child(Element::flex_spacer())
            .child(
                Element::button("取消")
                    .small()
                    .outline()
                    .neutral()
                    .on_click(move |_| show.set(false)),
            )
            .child(Element::button("保存").small().on_click(move |ctx| match editor.save() {
                Ok(()) => {
                    editor.show.set(false);
                    ctx.toast_ok("已保存");
                }
                Err(e) => editor.error.set(e),
            }));

        Element::dialog_panel(show, self.title, 520, move |_| show.set(false), body, footer)
    }
}

/// 删除确认对话框。
#[derive(Clone)]
struct Confirm {
    gui: Gui,
    show: Signal<bool>,
    target: Signal<String>,
    message: Signal<String>,
}

impl Confirm {
    fn new(gui: Gui) -> Self {
        Self {
            gui,
            show: signal(false),
            target: signal(String::new()),
            message: signal(String::new()),
        }
    }

    fn open(&self, name: String) {
        self.message
            .set(format!("删除串口「{name}」？已连接的终端会断开，日志文件会保留。"));
        self.target.set(name);
        self.show.set(true);
    }

    fn view(&self) -> Element {
        let show = self.show;
        let this = self.clone();
        let footer = Element::row()
            .width_match()
            .spacing(8)
            .child(Element::flex_spacer())
            .child(
                Element::button("取消")
                    .small()
                    .outline()
                    .neutral()
                    .on_click(move |_| show.set(false)),
            )
            .child(Element::button("删除").small().danger().on_click(move |ctx| {
                let target = this.target.get();
                let mut config = match this.gui.editable_config() {
                    Ok(config) => config,
                    Err(e) => {
                        ctx.toast_err(e);
                        return;
                    }
                };
                config.ports.retain(|p| p.name != target);
                match this.gui.apply_config(config) {
                    Ok(()) => ctx.toast_ok(format!("已删除 {target}")),
                    Err(e) => ctx.toast_err(format!("{e:#}")),
                }
                this.show.set(false);
            }));
        Element::dialog_panel(
            show,
            "删除串口",
            420,
            move |_| show.set(false),
            Element::label_signal(self.message).width_match(),
            footer,
        )
    }
}
