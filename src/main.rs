//! codex-keeper: 列出控制台会话（codex CLI 及其他 shell 窗口），
//! 绑定后可向其发送 "继续" + 回车。
//!
//! GUI 子系统：`#![windows_subsystem = "windows"]`，不弹额外控制台。
//! `--list` 参数：附着父控制台打印枚举结果（便于调试与脚本验证）。

#![windows_subsystem = "windows"]

mod enum_windows;
mod gui;
mod inject;

use windows::Win32::System::Console::{
    AttachConsole, FreeConsole, GetStdHandle, WriteConsoleW, STD_OUTPUT_HANDLE,
};

fn main() {
    // --list：无头模式，向父控制台输出当前所有控制台会话
    if std::env::args().any(|a| a == "--list") {
        list_mode();
        return;
    }

    // --status <pid>：无头模式，查询指定会话状态（写到 %TEMP%\codex-keeper-list.txt）
    let args: Vec<String> = std::env::args().collect();
    if let Some(i) = args.iter().position(|a| a == "--status") {
        let pid: u32 = args
            .get(i + 1)
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        status_mode(pid);
        return;
    }

    if let Err(e) = gui::run() {
        // GUI 程序无控制台，失败时以弹窗告知。
        use windows::Win32::UI::WindowsAndMessaging::{MESSAGEBOX_STYLE, MB_ICONERROR, MB_OK};
        let text: Vec<u16> = format!("codex-keeper 启动失败: {e}")
            .encode_utf16()
            .chain([0])
            .collect();
        unsafe {
            let _ = windows::Win32::UI::WindowsAndMessaging::MessageBoxW(
                None,
                windows::core::PCWSTR(text.as_ptr()),
                windows::core::w!("codex-keeper"),
                MESSAGEBOX_STYLE(MB_OK.0 | MB_ICONERROR.0),
            );
        }
    }
}

/// ATTACH_PARENT_PROCESS：附着到启动本程序的父控制台。
const ATTACH_PARENT_PROCESS: u32 = u32::MAX;

fn list_mode() {
    let sessions = enum_windows::enumerate_console_sessions();

    // 构建文本（两种输出共用）
    let mut lines = String::new();
    if sessions.is_empty() {
        lines.push_str("未发现任何控制台会话\r\n");
    }
    for w in &sessions {
        let tag = if w.is_codex { "[codex]" } else { "[shell]" };
        lines.push_str(&format!(
            "{tag} pid={} hwnd={} {} | {}\r\n",
            w.pid,
            w.hwnd,
            w.origin,
            if w.title.is_empty() { "(无标题)" } else { &w.title }
        ));
    }

    // 优先写结果文件（%TEMP%\codex-keeper-list.txt），便于脚本/无人值守验证
    if let Ok(tmp) = std::env::var("TEMP") {
        let path = std::path::Path::new(&tmp).join("codex-keeper-list.txt");
        let _ = std::fs::write(&path, &lines);
    }

    unsafe {
        if AttachConsole(ATTACH_PARENT_PROCESS).is_ok() {
            if let Ok(out) = GetStdHandle(STD_OUTPUT_HANDLE) {
                let utf16: Vec<u16> = lines.encode_utf16().collect();
                let mut written = 0u32;
                let _ = WriteConsoleW(out, &utf16, Some(&mut written), None);
                let _ = FreeConsole();
            }
        }
    }

    // 无法附着父控制台（例如从资源管理器启动）且显式传 --msg 时弹窗展示
    if std::env::args().any(|a| a == "--msg") {
        use windows::Win32::UI::WindowsAndMessaging::{MESSAGEBOX_STYLE, MB_OK};
        let utf16: Vec<u16> = lines.encode_utf16().chain([0]).collect();
        unsafe {
            let _ = windows::Win32::UI::WindowsAndMessaging::MessageBoxW(
                None,
                windows::core::PCWSTR(utf16.as_ptr()),
                windows::core::w!("codex-keeper --list"),
                MESSAGEBOX_STYLE(MB_OK.0),
            );
        }
    }
}

/// --status <pid>：查询会话状态并写入 %TEMP%\codex-keeper-list.txt。
fn status_mode(pid: u32) {
    use enum_windows::{query_session_status, SessionStatus};

    let result = match query_session_status(pid, 500) {
        SessionStatus::Stopped(reason) => format!("已停止 — {reason}"),
        SessionStatus::Working { cpu_delta_ms, members, signals } => {
            format!("运行中（信号: {signals}；CPU 活动 {cpu_delta_ms}ms/采样期，{members} 个进程）")
        }
        SessionStatus::Idle { cpu_delta_ms, members } => {
            format!("空闲（CPU 活动 {cpu_delta_ms}ms/500ms，{members} 个进程）")
        }
    };
    let line = format!("status(pid={pid}): {result}\r\n");

    if let Ok(tmp) = std::env::var("TEMP") {
        let path = std::path::Path::new(&tmp).join("codex-keeper-list.txt");
        let _ = std::fs::write(&path, &line);
    }

    unsafe {
        if AttachConsole(ATTACH_PARENT_PROCESS).is_ok() {
            if let Ok(out) = GetStdHandle(STD_OUTPUT_HANDLE) {
                let utf16: Vec<u16> = line.encode_utf16().collect();
                let mut written = 0u32;
                let _ = WriteConsoleW(out, &utf16, Some(&mut written), None);
                let _ = FreeConsole();
            }
        }
    }
}