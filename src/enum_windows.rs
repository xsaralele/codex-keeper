//! 枚举本机所有控制台会话（conhost 经典控制台 + Windows Terminal/ConPTY 标签）。
//!
//! 原理：枚举进程快照，对每个 PID 尝试 `AttachConsole`；成功即代表该进程
//! 拥有控制台会话。附着后用 `GetConsoleProcessList` 取同会话全部成员、
//! `GetConsoleTitleW` 取会话标题、`GetConsoleWindow` 取可见窗口句柄。
//! 以成员 PID 集合去重会话。判定 codex：成员映像路径或会话标题含
//! `codex`（大小写不敏感），无硬编码 PID。

use std::collections::{HashMap, HashSet};

use windows::Win32::Foundation::{CloseHandle, FILETIME, HANDLE};
use windows::Win32::System::Console::{
    AttachConsole, FreeConsole, GetConsoleProcessList, GetConsoleScreenBufferInfo,
    GetConsoleTitleW, GetConsoleWindow, GetStdHandle, ReadConsoleOutputCharacterW,
    CONSOLE_SCREEN_BUFFER_INFO, COORD, STD_OUTPUT_HANDLE,
};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::Threading::{
    GetProcessTimes, OpenProcess, QueryFullProcessImageNameW, Sleep, PROCESS_NAME_FORMAT,
    PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::UI::WindowsAndMessaging::IsWindowVisible;

/// 一个控制台会话（一个 conhost 窗口或一个 WT 标签页）。
#[derive(Clone, Debug)]
pub struct CodexWindow {
    /// 会话的控制台窗口句柄；ConPTY(WT) 会话可能为 0（服务端隐藏窗口）
    pub hwnd: isize,
    /// 绑定/发送用的 PID：成功附着过的成员 PID（AttachConsole 可用）
    pub pid: u32,
    /// 会话中命中 codex 的成员 PID；非 codex 会话为 0
    pub codex_pid: u32,
    /// 会话标题（console title，WT 标签也有）
    pub title: String,
    /// 说明文本，如 `codex.exe (pid 1234)` 或 `cmd.exe (pid 567)`
    pub origin: String,
    /// 是否为 codex 会话
    pub is_codex: bool,
}

/// 枚举所有控制台会话（含 codex 与普通 shell 窗口）。
pub fn enumerate_console_sessions() -> Vec<CodexWindow> {
    let pids = snapshot_pids();

    let mut sessions: Vec<CodexWindow> = Vec::new();
    let mut seen: HashSet<Vec<u32>> = HashSet::new();
    let mut images: HashMap<u32, String> = HashMap::new();

    // 本程序为 GUI 子系统，正常无控制台；先保证干净基态。
    // 本程序为 GUI 子系统，正常无控制台；先保证干净基态。
    // 注意：AttachConsole 后 GetConsoleProcessList 会把本进程也列入成员，
    // 必须排除自身，否则会自我误判。
    let own_pid = std::process::id();
    let pids: Vec<u32> = pids.into_iter().filter(|p| *p != own_pid).collect();

    unsafe {
        let _ = FreeConsole();

        for pid in pids {
            if AttachConsole(pid).is_err() {
                continue; // 无控制台（GUI/服务/权限不足），跳过
            }

            let mut member_buf = [0u32; 128];
            let n = GetConsoleProcessList(&mut member_buf);
            if n == 0 {
                let _ = FreeConsole();
                continue;
            }
            let members: Vec<u32> = member_buf[..n as usize]
                .iter()
                .copied()
                .filter(|p| *p != 0 && *p != own_pid)
                .collect();

            // 以成员集合作为会话指纹去重（同一会话多个成员都会附着成功）
            let mut key = members.clone();
            key.sort_unstable();
            let key = key;
            if seen.contains(&key) {
                let _ = FreeConsole();
                continue;
            }

            let title = console_title();
            let hwnd = GetConsoleWindow();

            // 解析成员映像路径并判定 codex
            let mut is_codex = false;
            let mut codex_pid = 0u32;
            let mut origin = String::new();
            for &mpid in &members {
                let path = images
                    .entry(mpid)
                    .or_insert_with(|| query_image_path(mpid).unwrap_or_default())
                    .clone();
                if is_codex_image(&path) && codex_pid == 0 {
                    is_codex = true;
                    codex_pid = mpid;
                    origin = format!("{} (pid {})", short_name(&path), mpid);
                }
            }
            if title.to_lowercase().contains("codex") {
                is_codex = true;
            }
            if origin.is_empty() {
                for &mpid in &members {
                    if let Some(p) = images.get(&mpid) {
                        if !p.is_empty() {
                            origin = format!("{} (pid {})", short_name(p), mpid);
                            break;
                        }
                    }
                }
            }
            if origin.is_empty() {
                origin = format!("pid {pid} (未知映像)");
            }

            // 绑定目标优先 codex 成员，否则取首个有映像的成员
            let attach_pid = if codex_pid != 0 {
                codex_pid
            } else {
                members
                    .iter()
                    .find(|p| images.get(*p).map_or(false, |s| !s.is_empty()))
                    .copied()
                    .unwrap_or(pid)
            };

            // 只列出有可见窗口的会话；后台隐藏控制台(hwnd=0 或不可见)不展示。
            // 唯一例外：codex 会话始终保留（codex 若跑在 ConPTY 下 hwnd=0，仍需可见）。
            let visible = hwnd.0 as isize != 0 && IsWindowVisible(hwnd).as_bool();
            if !visible && !is_codex {
                seen.insert(key);
                let _ = FreeConsole();
                continue;
            }

            sessions.push(CodexWindow {
                hwnd: hwnd.0 as isize,
                pid: attach_pid,
                codex_pid,
                title,
                origin,
                is_codex,
            });

            seen.insert(key);
            let _ = FreeConsole();
        }
    }

    // codex 会话优先，其次可见窗口（hwnd!=0），再按 PID 排序
    sessions.sort_by(|a, b| {
        b.is_codex
            .cmp(&a.is_codex)
            .then((b.hwnd != 0).cmp(&(a.hwnd != 0)))
            .then(a.pid.cmp(&b.pid))
    });
    sessions
}

/// 绑定会话的工作状态。
#[derive(Clone, Debug)]
pub enum SessionStatus {
    /// 进程已退出或控制台已消失；携带原因
    Stopped(String),
    /// 运行中；signals 说明触发原因（界面刷新 / CPU 活动）
    Working {
        cpu_delta_ms: u64,
        members: usize,
        signals: String,
    },
    /// 空闲：进程存活、控制台在，界面静止且无 CPU 活动（等待输入）
    Idle { cpu_delta_ms: u64, members: usize },
}

/// 采样窗口内判定为"运行中"的 CPU 时间阈值（毫秒）。
const WORKING_CPU_THRESHOLD_MS: u64 = 15;

/// 一帧会话快照：CPU 时间 + 控制台标题 + 视口内容。
struct Frame {
    cpu: u64,
    members: Vec<u32>,
    title: String,
    screen: Vec<u16>,
}

/// 查询绑定会话的状态：运行中 / 空闲 / 停止。
///
/// 判定链：
/// 1. 进程打不开或已记录退出时间 → 停止；
/// 2. AttachConsole 失败（控制台已消失）→ 停止；
/// 3. 采样两帧快照，任一信号命中 → 运行中，否则 → 空闲：
///    a. 会话标题变化（codex 工作时标题 spinner 在转，如 "⠋⠹⠸"）；
///    b. 视口内容变化（TUI 持续重绘：spinner/进度/流式输出）；
///    c. 成员 CPU 时间增量 ≥ 阈值（本地计算型工作）。
///    仅有 CPU 信号不够：codex 等网络流式响应时 CPU 几乎为 0，
///    但界面在刷新，a/b 能捕捉到。
pub fn query_session_status(pid: u32, sample_ms: u32) -> SessionStatus {
    // 帧 0
    let f0 = match sample_frame(pid) {
        Some(f) => f,
        None => return SessionStatus::Stopped(format!("进程 pid {pid} 已退出或无法访问")),
    };
    if f0.members.is_empty() {
        return SessionStatus::Stopped(format!("pid {pid} 的控制台已关闭"));
    }

    // 采样窗口
    unsafe {
        Sleep(sample_ms);
    }

    // 帧 1
    let f1 = match sample_frame(pid) {
        Some(f) => f,
        None => return SessionStatus::Stopped(format!("进程 pid {pid} 已退出或无法访问")),
    };
    if f1.members.is_empty() {
        return SessionStatus::Stopped(format!("pid {pid} 的控制台已关闭"));
    }

    let members = f1.members.len();
    let cpu_delta_ms = f1.cpu.saturating_sub(f0.cpu) / 10_000;

    // 三路信号
    let mut signals: Vec<&str> = Vec::new();
    if f0.title != f1.title {
        signals.push("界面刷新(标题变化)");
    }
    if f0.screen != f1.screen {
        signals.push("界面刷新(屏幕内容变化)");
    }
    if cpu_delta_ms >= WORKING_CPU_THRESHOLD_MS {
        signals.push("CPU 活动");
    }

    if signals.is_empty() {
        SessionStatus::Idle {
            cpu_delta_ms,
            members,
        }
    } else {
        SessionStatus::Working {
            cpu_delta_ms,
            members,
            signals: signals.join(" + "),
        }
    }
}

/// 附着到 pid 所在控制台，捕获一帧快照（CPU / 标题 / 视口 / 成员）。
/// 进程不存在或已退出时返回 None；进程活着但控制台没了时 members 为空。
fn sample_frame(pid: u32) -> Option<Frame> {
    unsafe {
        // 进程必须能打开且未退出
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let handle: HANDLE = h.into();
        let mut creation = FILETIME::default();
        let mut exit = FILETIME::default();
        let mut kernel = FILETIME::default();
        let mut user = FILETIME::default();
        let ok =
            GetProcessTimes(handle, &mut creation, &mut exit, &mut kernel, &mut user).is_ok();
        let _ = CloseHandle(handle);
        if !ok || ft_u64(exit) != 0 {
            return None; // 已退出或无权限
        }

        // 附着目标控制台
        let _ = FreeConsole();
        if AttachConsole(pid).is_err() {
            return Some(Frame {
                cpu: 0,
                members: Vec::new(),
                title: String::new(),
                screen: Vec::new(),
            });
        }

        // 标题
        let title = console_title();

        // 视口内容（读不到就留空，比较时视为无信号）
        let screen = read_viewport();

        // 会话成员
        let mut buf = [0u32; 128];
        let n = GetConsoleProcessList(&mut buf);
        let own = std::process::id();
        let members: Vec<u32> = buf[..n as usize]
            .iter()
            .copied()
            .filter(|p| *p != 0 && *p != own)
            .collect();

        let _ = FreeConsole();

        // 汇总成员 CPU 时间
        let mut total = 0u64;
        for &mpid in &members {
            if let Some(h) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, mpid).ok() {
                let handle: HANDLE = h.into();
                let mut c = FILETIME::default();
                let mut e = FILETIME::default();
                let mut k = FILETIME::default();
                let mut u = FILETIME::default();
                if GetProcessTimes(handle, &mut c, &mut e, &mut k, &mut u).is_ok() && ft_u64(e) == 0
                {
                    total = total.saturating_add(ft_u64(k)).saturating_add(ft_u64(u));
                }
                let _ = CloseHandle(handle);
            }
        }

        Some(Frame {
            cpu: total,
            members,
            title,
            screen,
        })
    }
}

/// 附着状态下读取当前控制台的可见视口内容（用于检测 TUI 是否在重绘）。
unsafe fn read_viewport() -> Vec<u16> {
    unsafe {
        let out = match GetStdHandle(STD_OUTPUT_HANDLE) {
            Ok(h) => h,
            Err(_) => return Vec::new(),
        };
        let mut info = CONSOLE_SCREEN_BUFFER_INFO::default();
        if GetConsoleScreenBufferInfo(out, &mut info).is_err() {
            return Vec::new();
        }
        let cols = (info.srWindow.Right - info.srWindow.Left + 1).max(0) as usize;
        let rows = (info.srWindow.Bottom - info.srWindow.Top + 1).max(0) as usize;
        if cols == 0 || rows == 0 || cols * rows > 1_000_000 {
            return Vec::new();
        }
        let mut buf = vec![0u16; cols * rows];
        let mut read = 0u32;
        let ok = ReadConsoleOutputCharacterW(
            out,
            &mut buf,
            COORD {
                X: info.srWindow.Left,
                Y: info.srWindow.Top,
            },
            &mut read,
        );
        if ok.is_err() || read == 0 {
            return Vec::new();
        }
        buf.truncate(read as usize);
        buf
    }
}

fn ft_u64(ft: FILETIME) -> u64 {
    (ft.dwHighDateTime as u64) << 32 | ft.dwLowDateTime as u64
}

/// 判定：映像文件名以 codex 开头（codex.exe / codex.cmd 等，大小写不敏感）。
/// 只看文件名且要求前缀匹配，避免误伤 codex-keeper.exe 这类名称。
pub fn is_codex_image(path: &str) -> bool {
    short_name(path)
        .to_lowercase()
        .split('.')
        .next()
        .map_or(false, |stem| stem == "codex")
}

fn short_name(path: &str) -> &str {
    path.rsplit(['\\', '/']).next().unwrap_or(path)
}

/// 附着状态下读取当前控制台标题。
unsafe fn console_title() -> String {
    let mut buf = [0u16; 1024];
    let n = unsafe { GetConsoleTitleW(&mut buf) };
    if n > 0 {
        String::from_utf16_lossy(&buf[..n as usize])
    } else {
        String::new()
    }
}

/// 进程快照：所有 PID。
fn snapshot_pids() -> Vec<u32> {
    unsafe {
        let snap = match CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) {
            Ok(h) => h,
            Err(_) => return Vec::new(),
        };
        let mut out = Vec::new();
        let mut entry = PROCESSENTRY32W {
            dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        if Process32FirstW(snap, &mut entry).is_ok() {
            loop {
                out.push(entry.th32ProcessID);
                if Process32NextW(snap, &mut entry).is_err() {
                    break;
                }
            }
        }
        let _ = CloseHandle(snap);
        out
    }
}

fn query_image_path(pid: u32) -> Option<String> {
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let handle: HANDLE = h.into();
        let mut buf = [0u16; 1024];
        let mut size = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(
            handle,
            PROCESS_NAME_FORMAT(0),
            windows::core::PWSTR(buf.as_mut_ptr()),
            &mut size,
        );
        let _ = CloseHandle(handle);
        if ok.is_err() {
            return None;
        }
        Some(String::from_utf16_lossy(&buf[..size as usize]))
    }
}