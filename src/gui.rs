//! 原生 Win32 GUI: 窗口列表 + 刷新/绑定/状态查询/单次发送/持续发送/停止发送 + 日志区。

use std::cell::RefCell;
use std::ffi::c_void;
use std::ptr;

use windows::core::{w, PCWSTR, PWSTR};
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, SIZE, SYSTEMTIME, WPARAM};
use windows::Win32::Graphics::Gdi::{
    CreateFontIndirectW, GetDC, GetTextExtentPoint32W, HGDIOBJ, ReleaseDC, SelectObject,
    COLOR_WINDOW, HBRUSH, HFONT,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Controls::{EM_REPLACESEL, EM_SCROLLCARET, EM_SETSEL};
use windows::Win32::UI::Input::KeyboardAndMouse::EnableWindow;
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::enum_windows::{CodexWindow, enumerate_console_sessions};
use crate::inject::KEEPALIVE_TEXT;

const ID_LISTBOX: i32 = 101;
const ID_LOG: i32 = 102;
const ID_EDIT_INTERVAL: i32 = 103;
const ID_EDIT_TEXT: i32 = 104;
const ID_BTN_REFRESH: i32 = 201;
const ID_BTN_BIND: i32 = 202;
const ID_BTN_STATUS: i32 = 203;
const ID_BTN_SEND: i32 = 204;
const ID_BTN_CONT: i32 = 205;
const ID_BTN_STOP: i32 = 206;

/// 持续发送定时器 ID。（轮询周期由「轮询周期(分)」输入框决定，默认 1 分钟。）
const ID_TIMER_SEND: usize = 1;

struct AppState {
    windows: Vec<CodexWindow>,
    bound_pid: Option<u32>,
}

thread_local! {
    static STATE: RefCell<AppState> = RefCell::new(AppState {
        windows: Vec::new(),
        bound_pid: None,
    });
}

/// UI 字体（系统 lfMessageFont，Segoe UI 9pt），创建后全程复用。
static mut UI_FONT: HFONT = HFONT(std::ptr::null_mut());

pub fn run() -> Result<(), String> {
    unsafe {
        let hinst = GetModuleHandleW(None).map_err(|e| e.to_string())?;

        let class_name = w!("CodexKeeperWindowClass");
        let wc = WNDCLASSW {
            lpfnWndProc: Some(wnd_proc),
            hInstance: hinst.into(),
            lpszClassName: class_name,
            hCursor: LoadCursorW(None, IDC_ARROW).map_or(HCURSOR(ptr::null_mut()), |c| c),
            hbrBackground: HBRUSH((COLOR_WINDOW.0 + 1) as usize as *mut c_void),
            ..Default::default()
        };
        // 重复进入时窗口类可能已存在，忽略失败。
        let _ = RegisterClassW(&wc);

        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            class_name,
            w!("codex-keeper"),
            WS_OVERLAPPEDWINDOW,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            780,
            580,
            None,
            None,
            Some(hinst.into()),
            None,
        )
        .map_err(|e| format!("CreateWindowExW 失败: {e}"))?;

        let _ = ShowWindow(hwnd, SW_SHOW);
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
    Ok(())
}

unsafe extern "system" fn wnd_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_CREATE => {
                create_children(hwnd);
                apply_ui_font(hwnd);
                LRESULT(0)
            }
            WM_COMMAND => {
                let id = (wparam.0 & 0xFFFF) as i32;
                if ((wparam.0 >> 16) & 0xFFFF) as u32 == BN_CLICKED {
                    match id {
                        ID_BTN_REFRESH => on_refresh(hwnd),
                        ID_BTN_BIND => on_bind(hwnd),
                        ID_BTN_STATUS => on_status(hwnd),
                        ID_BTN_SEND => on_send(hwnd),
                        ID_BTN_CONT => on_continuous(hwnd),
                        ID_BTN_STOP => on_stop(hwnd),
                        _ => {}
                    }
                    LRESULT(0)
                } else {
                    DefWindowProcW(hwnd, msg, wparam, lparam)
                }
            }
            WM_TIMER => {
                if wparam.0 == ID_TIMER_SEND {
                    on_timer_send(hwnd);
                }
                LRESULT(0)
            }
            WM_CLOSE => {
                let _ = DestroyWindow(hwnd);
                LRESULT(0)
            }
            WM_DESTROY => {
                let _ = KillTimer(Some(hwnd), ID_TIMER_SEND);
                PostQuitMessage(0);
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}

/// 创建 UI 字体并把 hWnd 之后的全部子控件换成该字体。
unsafe fn apply_ui_font(parent: HWND) {
    unsafe {
        let mut ncm = NONCLIENTMETRICSW {
            cbSize: std::mem::size_of::<NONCLIENTMETRICSW>() as u32,
            ..Default::default()
        };
        if SystemParametersInfoW(
            SPI_GETNONCLIENTMETRICS,
            ncm.cbSize,
            Some(&mut ncm as *mut _ as *mut core::ffi::c_void),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        )
        .is_ok()
        {
            *std::ptr::addr_of_mut!(UI_FONT) = CreateFontIndirectW(&ncm.lfMessageFont);
        }

        let mut hwnd = GetWindow(parent, GW_CHILD).ok();
        while let Some(h) = hwnd {
            SendMessageW(h, WM_SETFONT, Some(WPARAM((*std::ptr::addr_of!(UI_FONT)).0 as usize)), Some(LPARAM(1)));
            hwnd = GetWindow(h, GW_HWNDNEXT).ok();
        }
    }
}

/// 控件通用行高（Segoe UI 9pt 下的舒展高度）。
fn ui_row_height() -> i32 {
    26
}

/// 创建子控件：参数设置行 + 六个按钮 + 列表 + 日志编辑框。
unsafe fn create_children(parent: HWND) {
    unsafe {
        // 先取 UI 字体，测量标签时保证与实际渲染一致
        let mut ncm = NONCLIENTMETRICSW {
            cbSize: std::mem::size_of::<NONCLIENTMETRICSW>() as u32,
            ..Default::default()
        };
        let _ = SystemParametersInfoW(
            SPI_GETNONCLIENTMETRICS,
            ncm.cbSize,
            Some(&mut ncm as *mut _ as *mut core::ffi::c_void),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        );
        if (*std::ptr::addr_of!(UI_FONT)).is_invalid() {
            *std::ptr::addr_of_mut!(UI_FONT) = CreateFontIndirectW(&ncm.lfMessageFont);
        }

        // —— 参数设置行（靠左对齐） ——
        // 控件宽度按文字实测，从左边缘依次级联排列；
        // 标签→输入框 6px，两组之间 24px。
        // 发送内容输入框宽度为内容实测的 3 倍，便于编辑长文本。
        let row_h = ui_row_height();
        let label_y = 8 + (row_h - 20) / 2; // 标签高 20，垂直居中于行
        let edit_y = 8;

        // 控件固有宽度：标签实测+6px 内边距；输入框带下限。
        let label1_w = measure_text_w(parent, w!("轮询周期(分)")) + 6;
        let label2_w = measure_text_w(parent, w!("发送内容")) + 6;
        let edit1_w = (measure_text_w(parent, w!("1")) + 24).max(44);
        let edit2_w = (measure_text_w(parent, w!("继续当前任务")) * 3 + 24).max(100);

        let label_to_edit = 6i32;
        let group_gap = 24i32;

        let mut x = 12;
        create_static(parent, w!("轮询周期(分)"), x, label_y, label1_w);
        x += label1_w + label_to_edit;
        create_edit(parent, w!("1"), x, edit_y, edit1_w, ID_EDIT_INTERVAL, true);
        x += edit1_w + group_gap;
        create_static(parent, w!("发送内容"), x, label_y, label2_w);
        x += label2_w + label_to_edit;
        create_edit(parent, w!("继续当前任务"), x, edit_y, edit2_w, ID_EDIT_TEXT, false);

        // —— 按钮行 ——
        let btn_y = edit_y + row_h + 10;
        make_button_at(parent, w!("刷新"), 12, btn_y, ID_BTN_REFRESH);
        make_button_at(parent, w!("绑定"), 132, btn_y, ID_BTN_BIND);
        make_button_at(parent, w!("状态查询"), 252, btn_y, ID_BTN_STATUS);
        make_button_at(parent, w!("单次发送"), 372, btn_y, ID_BTN_SEND);
        make_button_at(parent, w!("持续发送"), 492, btn_y, ID_BTN_CONT);
        make_button_at(parent, w!("停止发送"), 612, btn_y, ID_BTN_STOP);
        let btn_bottom = btn_y + 32;

        // 窗口列表
        let list_y = btn_bottom + 10;
        let list_h = 344 - list_y;
        let _ = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("LISTBOX"),
            PCWSTR::null(),
            WINDOW_STYLE(
                (WS_CHILD.0 | WS_VISIBLE.0 | WS_VSCROLL.0 | WS_BORDER.0 | LBS_NOTIFY as u32) as u32,
            ),
            12,
            list_y,
            740,
            list_h,
            Some(parent),
            Some(HMENU(ID_LISTBOX as usize as *mut c_void)),
            Some(window_instance(parent)),
            None,
        );

        // 日志区
        let _ = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("EDIT"),
            PCWSTR::null(),
            WINDOW_STYLE(
                (WS_CHILD.0
                    | WS_VISIBLE.0
                    | WS_VSCROLL.0
                    | ES_MULTILINE as u32
                    | ES_READONLY as u32
                    | ES_AUTOVSCROLL as u32) as u32,
            ),
            12,
            344,
            740,
            190,
            Some(parent),
            Some(HMENU(ID_LOG as usize as *mut c_void)),
            Some(window_instance(parent)),
            None,
        );
    }
}

/// 单行文本编辑框（参数输入），宽度由调用方给定。
unsafe fn create_edit(parent: HWND, text: PCWSTR, x: i32, y: i32, w: i32, id: i32, centered: bool) {
    unsafe {
        let h = ui_row_height() - 2;
        let mut style = WS_CHILD.0 | WS_VISIBLE.0 | WS_BORDER.0 | ES_AUTOHSCROLL as u32;
        if centered {
            style |= ES_CENTER as u32;
        }
        let _ = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("EDIT"),
            text,
            WINDOW_STYLE(style),
            x,
            y,
            w,
            h,
            Some(parent),
            Some(HMENU(id as usize as *mut c_void)),
            Some(window_instance(parent)),
            None,
        );
    }
}

/// 静态标签，宽度由调用方给定（调用方先用 measure_text_w 实测）。
unsafe fn create_static(parent: HWND, text: PCWSTR, x: i32, y: i32, w: i32) {
    unsafe {
        let _ = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("STATIC"),
            text,
            WINDOW_STYLE((WS_CHILD.0 | WS_VISIBLE.0) as u32),
            x,
            y,
            w,
            20,
            Some(parent),
            None,
            Some(window_instance(parent)),
            None,
        );
    }
}

/// 测量文字在 UI 字体下的像素宽度（末尾不含 NUL）。
/// 测量时把 UI 字体选入 DC，保证与控件实际渲染一致。
unsafe fn measure_text_w(parent: HWND, text: PCWSTR) -> i32 {
    unsafe {
        let hdc = GetDC(Some(parent));
        if hdc.is_invalid() {
            return 80; // 测量失败的保守回退宽度
        }
        let old = if (*std::ptr::addr_of!(UI_FONT)).is_invalid() {
            None
        } else {
            let prev = SelectObject(hdc, HGDIOBJ((*std::ptr::addr_of!(UI_FONT)).0));
            if prev.is_invalid() { None } else { Some(prev) }
        };
        // PCWSTR 指向的字符串以 NUL 结尾；w! 宏保证
        let mut len = 0usize;
        while *text.0.add(len) != 0 {
            len += 1;
            if len > 256 {
                break;
            }
        }
        let mut size = SIZE::default();
        let ok = GetTextExtentPoint32W(hdc, std::slice::from_raw_parts(text.0, len), &mut size);
        if let Some(prev) = old {
            let _ = SelectObject(hdc, prev);
        }
        let _ = ReleaseDC(Some(parent), hdc);
        if ok.as_bool() {
            size.cx
        } else {
            80
        }
    }
}

unsafe fn make_button_at(parent: HWND, text: PCWSTR, x: i32, y: i32, id: i32) {
    unsafe {
        let _ = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("BUTTON"),
            text,
            WINDOW_STYLE((WS_CHILD.0 | WS_VISIBLE.0 | BS_PUSHBUTTON as u32) as u32),
            x,
            y,
            110,
            30,
            Some(parent),
            Some(HMENU(id as usize as *mut c_void)),
            Some(window_instance(parent)),
            None,
        );
    }
}

unsafe fn control(parent: HWND, id: i32) -> HWND {
    unsafe { GetDlgItem(Some(parent), id).unwrap_or(HWND(ptr::null_mut())) }
}

unsafe fn window_instance(hwnd: HWND) -> HINSTANCE {
    unsafe { HINSTANCE(GetWindowLongPtrW(hwnd, GWLP_HINSTANCE) as *mut c_void) }
}

/// 向列表框追加一行（Unicode）。
unsafe fn list_add(hwnd: HWND, text: &str) {
    unsafe {
        let list = control(hwnd, ID_LISTBOX);
        let mut buf: Vec<u16> = text.encode_utf16().collect();
        buf.push(0);
        SendMessageW(
            list,
            LB_ADDSTRING,
            Some(WPARAM(0)),
            Some(LPARAM(PWSTR(buf.as_mut_ptr()).0 as isize)),
        );
    }
}

/// 追加一行日志并滚动到底部。
unsafe fn log(hwnd: HWND, line: &str) {
    unsafe {
        let edit = control(hwnd, ID_LOG);
        if edit.is_invalid() {
            return;
        }
        let text = format!("{line}\r\n");
        let mut buf: Vec<u16> = text.encode_utf16().collect();
        buf.push(0);
        // 先选中全文再替换，实现追加。
        SendMessageW(edit, EM_SETSEL, Some(WPARAM(usize::MAX)), Some(LPARAM(isize::MAX)));
        SendMessageW(
            edit,
            EM_REPLACESEL,
            Some(WPARAM(1)),
            Some(LPARAM(PWSTR(buf.as_mut_ptr()).0 as isize)),
        );
        SendMessageW(edit, EM_SCROLLCARET, Some(WPARAM(0)), Some(LPARAM(0)));
    }
}

fn now() -> String {
    unsafe {
        let st: SYSTEMTIME = windows::Win32::System::SystemInformation::GetLocalTime();
        format!("{:02}:{:02}:{:02}", st.wHour, st.wMinute, st.wSecond)
    }
}

/// 刷新按钮：重新枚举 codex 控制台窗口并重建列表。
fn on_refresh(hwnd: HWND) {
    let found = enumerate_console_sessions();

    STATE.with(|s| s.borrow_mut().windows = found.clone());

    unsafe {
        let list = control(hwnd, ID_LISTBOX);
        SendMessageW(list, LB_RESETCONTENT, Some(WPARAM(0)), Some(LPARAM(0)));
        for win in &found {
            let bound = STATE.with(|s| s.borrow().bound_pid == Some(win.pid));
            let btag = if bound { "[已绑定] " } else { "" };
            let ctag = if win.is_codex { "[codex] " } else { "[shell] " };
            let htag = if win.hwnd == 0 { "[隐藏] " } else { "" };
            let title = if win.title.is_empty() {
                "(无标题)"
            } else {
                &win.title
            };
            list_add(
                hwnd,
                &format!(
                    "{btag}{ctag}{htag}[pid {}] {} - {title}",
                    win.pid, win.origin
                ),
            );
        }
        let codex_n = found.iter().filter(|w| w.is_codex).count();
        log(
            hwnd,
            &format!(
                "[{}] 刷新完成，发现 {} 个控制台会话（codex {} 个，其他 shell {} 个）",
                now(),
                found.len(),
                codex_n,
                found.len() - codex_n
            ),
        );
    }
}

fn selected_index(hwnd: HWND) -> Option<usize> {
    unsafe {
        let list = control(hwnd, ID_LISTBOX);
        let idx = SendMessageW(list, LB_GETCURSEL, Some(WPARAM(0)), Some(LPARAM(0))).0 as i32;
        if idx < 0 { None } else { Some(idx as usize) }
    }
}

/// 绑定按钮：把所选行的 codex PID 记为发送目标（重复绑定幂等）。
fn on_bind(hwnd: HWND) {
    let idx = match selected_index(hwnd) {
        Some(i) => i,
        None => {
            unsafe { log(hwnd, &format!("[{}] 绑定失败: 请先在列表中选择一个窗口", now())) };
            return;
        }
    };

    let picked = STATE.with(|s| s.borrow().windows.get(idx).cloned());
    match picked {
        Some(win) => {
            let already = STATE.with(|s| s.borrow().bound_pid == Some(win.pid));
            STATE.with(|s| s.borrow_mut().bound_pid = Some(win.pid));
            let note = if already { " (幂等: 已是当前绑定目标)" } else { "" };
            unsafe {
                log(
                    hwnd,
                    &format!(
                        "[{}] 绑定目标: {} (pid {}){note}",
                        now(),
                        win.origin,
                        win.pid
                    ),
                );
            }
            // 刷新列表以显示已绑定标记。
            on_refresh(hwnd);
        }
        None => unsafe {
            log(hwnd, &format!("[{}] 绑定失败: 列表索引已失效，请刷新", now()))
        },
    }
}

/// 状态查询按钮：判定绑定会话是 运行中 / 空闲 / 停止。
fn on_status(hwnd: HWND) {
    let bound = STATE.with(|s| s.borrow().bound_pid);
    let pid = match bound {
        Some(p) => p,
        None => {
            unsafe { log(hwnd, &format!("[{}] 状态查询失败: 尚未绑定任何窗口", now())) };
            return;
        }
    };

    match crate::enum_windows::query_session_status(pid, 500) {
        crate::enum_windows::SessionStatus::Stopped(reason) => unsafe {
            log(hwnd, &format!("[{}] 状态查询 (pid {}): 已停止 — {reason}", now(), pid))
        },
        crate::enum_windows::SessionStatus::Working { cpu_delta_ms, members, signals } => unsafe {
            log(
                hwnd,
                &format!(
                    "[{}] 状态查询 (pid {}): 运行中（信号: {signals}；CPU 活动 {cpu_delta_ms}ms/采样期，{members} 个进程）",
                    now(),
                    pid
                ),
            )
        },
        crate::enum_windows::SessionStatus::Idle { cpu_delta_ms, members } => unsafe {
            log(
                hwnd,
                &format!(
                    "[{}] 状态查询 (pid {}): 空闲（等待输入，采样 500ms 内 CPU 活动 {cpu_delta_ms}ms，{members} 个进程）",
                    now(),
                    pid
                ),
            )
        },
    }
}

/// 发送按钮: 向绑定窗口写入发送内容 + 回车。
fn on_send(hwnd: HWND) {
    let bound = STATE.with(|s| s.borrow().bound_pid);
    let pid = match bound {
        Some(p) => p,
        None => {
            unsafe { log(hwnd, &format!("[{}] 发送失败: 尚未绑定任何窗口", now())) };
            return;
        }
    };

    match crate::inject::send_continue(pid, &send_text(hwnd)) {
        Ok(n) => unsafe {
            let text = send_text(hwnd);
            log(
                hwnd,
                &format!(
                    "[{}] 已向 pid {} 发送「{text}」+ 回车 ({n} 条按键事件)",
                    now(),
                    pid
                ),
            )
        },
        Err(e) => unsafe {
            log(hwnd, &format!("[{}] 发送失败 (pid {}): {e}", now(), pid))
        },
    }
}

/// 持续发送进行中时，前 4 个按钮（刷新/绑定/状态查询/单次发送）禁用。
unsafe fn set_main_buttons(hwnd: HWND, enabled: bool) {
    unsafe {
        for id in [ID_BTN_REFRESH, ID_BTN_BIND, ID_BTN_STATUS, ID_BTN_SEND] {
            let btn = control(hwnd, id);
            if !btn.is_invalid() {
                let _ = EnableWindow(btn, enabled);
            }
        }
    }
}

/// 读取「发送内容」输入框；为空时回退默认「继续」。
fn send_text(hwnd: HWND) -> String {
    let raw = read_edit(hwnd, ID_EDIT_TEXT);
    if raw.trim().is_empty() {
        KEEPALIVE_TEXT.to_string()
    } else {
        raw.trim().to_string()
    }
}

/// 读取「轮询周期(分)」输入框并转成毫秒；非法/为空/超出范围时回退默认 1 分钟。
fn interval_ms(hwnd: HWND) -> u32 {
    const DEFAULT_MS: u32 = 60_000;
    let raw = read_edit(hwnd, ID_EDIT_INTERVAL);
    match raw.trim().parse::<u32>() {
        Ok(m) if m >= 1 && m <= 1440 => m.saturating_mul(60_000),
        _ => DEFAULT_MS,
    }
}

/// 读取对话框内一个单行编辑框的文本。
fn read_edit(hwnd: HWND, id: i32) -> String {
    unsafe {
        let mut buf = [0u16; 512];
        let n = GetDlgItemTextW(hwnd, id, &mut buf);
        if n == 0 {
            String::new()
        } else {
            String::from_utf16_lossy(&buf[..n as usize])
        }
    }
}

/// 持续发送按钮：启动定时器，周期取自「轮询周期(分)」输入框——
/// 先查状态，空闲才发送（避免 codex 工作时把发送内容堆进输入队列）。
/// 持续发送期间禁用前 4 个按钮，防止中途改绑定/手动发送造成混乱。
fn on_continuous(hwnd: HWND) {
    let bound = STATE.with(|s| s.borrow().bound_pid);
    if bound.is_none() {
        unsafe { log(hwnd, &format!("[{}] 持续发送失败: 尚未绑定任何窗口", now())) };
        return;
    }

    unsafe {
        let interval = interval_ms(hwnd);
        let ok = SetTimer(Some(hwnd), ID_TIMER_SEND, interval, None);
        if ok == 0 {
            log(hwnd, &format!("[{}] 持续发送失败: SetTimer 失败", now()));
        } else {
            set_main_buttons(hwnd, false);
            log(
                hwnd,
                &format!(
                    "[{}] 持续发送已启动: 每 {} 分钟检查一次，仅在会话空闲时发送「{}」",
                    now(),
                    interval / 60_000,
                    send_text(hwnd)
                ),
            );
        }
    }
}

/// 停止发送按钮：停掉定时器并恢复按钮。
fn on_stop(hwnd: HWND) {
    unsafe {
        if KillTimer(Some(hwnd), ID_TIMER_SEND).is_ok() {
            set_main_buttons(hwnd, true);
            log(hwnd, &format!("[{}] 持续发送已停止", now()));
        } else {
            log(hwnd, &format!("[{}] 当前没有正在进行的持续发送", now()));
        }
    }
}

/// 定时器回调：每轮先做状态查询，按状态决定是否发送。
fn on_timer_send(hwnd: HWND) {
    let pid = match STATE.with(|s| s.borrow().bound_pid) {
        Some(p) => p,
        None => {
            unsafe {
                let _ = KillTimer(Some(hwnd), ID_TIMER_SEND);
                set_main_buttons(hwnd, true);
                log(hwnd, &format!("[{}] 持续发送已停止: 绑定已失效，请重新绑定", now()));
            }
            return;
        }
    };

    match crate::enum_windows::query_session_status(pid, 500) {
        crate::enum_windows::SessionStatus::Stopped(reason) => unsafe {
            let _ = KillTimer(Some(hwnd), ID_TIMER_SEND);
            set_main_buttons(hwnd, true);
            log(
                hwnd,
                &format!(
                    "[{}] 持续发送已自动停止: 目标会话已停止 — {reason}",
                    now()
                ),
            );
        },
        crate::enum_windows::SessionStatus::Working { .. } => unsafe {
            log(
                hwnd,
                &format!("[{}] 持续发送: pid {} 正在工作，本轮跳过", now(), pid),
            );
        },
        crate::enum_windows::SessionStatus::Idle { .. } => {
            let text = send_text(hwnd);
            match crate::inject::send_continue(pid, &text) {
                Ok(n) => unsafe {
                    log(
                        hwnd,
                        &format!(
                            "[{}] 持续发送: pid {} 空闲，已发送「{text}」+回车 ({n} 条按键事件)",
                            now(),
                            pid
                        ),
                    );
                },
                Err(e) => unsafe {
                    let _ = KillTimer(Some(hwnd), ID_TIMER_SEND);
                    set_main_buttons(hwnd, true);
                    log(
                        hwnd,
                        &format!(
                            "[{}] 持续发送已自动停止: 发送失败 (pid {}) — {e}",
                            now(),
                            pid
                        ),
                    );
                },
            }
        }
    }
}