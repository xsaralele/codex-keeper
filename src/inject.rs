//! 向目标控制台窗口注入键盘输入（继续 + 回车）。
//!
//! 流程：AttachConsole(pid) -> GetStdHandle(STD_INPUT_HANDLE) ->
//! WriteConsoleInputW 写入 KEY_EVENT 序列 -> FreeConsole()。
//! 成功返回 Ok，写入条数不足或 API 失败返回可读错误文本，不 panic。

use std::fmt;
use windows::core::BOOL;
use windows::Win32::Foundation::GetLastError;
use windows::Win32::System::Console::{
    AttachConsole, FreeConsole, GetStdHandle, WriteConsoleInputW, INPUT_RECORD, INPUT_RECORD_0,
    KEY_EVENT, KEY_EVENT_RECORD, KEY_EVENT_RECORD_0, STD_INPUT_HANDLE,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{VIRTUAL_KEY, VK_RETURN};

/// 注入失败时携带的可读原因。
#[derive(Debug)]
pub struct InjectError(pub String);

impl fmt::Display for InjectError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// 发送内容的默认值（输入框为空时的回退）。
pub const KEEPALIVE_TEXT: &str = "继续当前任务";

/// 构造 text + 回车 的 KEY_EVENT 序列。
///
/// 每个字符产生两条记录：按下（bKeyDown=1，带 UnicodeChar）与抬起
/// （bKeyDown=0）。空文本仍会发送一个回车。
fn build_key_events(text: &str) -> Vec<INPUT_RECORD> {
    let mut chars: Vec<u16> = text.encode_utf16().collect();
    chars.push(VK_RETURN.0);

    let mut events = Vec::with_capacity(chars.len() * 2);
    for &code in &chars {
        let vk = VIRTUAL_KEY(code);
        for key_up in [false, true] {
            events.push(INPUT_RECORD {
                EventType: KEY_EVENT as u16,
                Event: INPUT_RECORD_0 {
                    KeyEvent: KEY_EVENT_RECORD {
                        bKeyDown: BOOL(if key_up { 0 } else { 1 }),
                        wRepeatCount: 1,
                        wVirtualKeyCode: vk.0,
                        wVirtualScanCode: 0,
                        uChar: KEY_EVENT_RECORD_0 {
                            UnicodeChar: if key_up { 0 } else { code },
                        },
                        dwControlKeyState: 0,
                    },
                },
            });
        }
    }
    events
}

/// 向 pid 所属的控制台写入 text + 回车。
/// text 为空时按默认「继续」发送。
pub fn send_continue(pid: u32, text: &str) -> Result<usize, InjectError> {
    if pid == 0 {
        return Err(InjectError("无效的 PID（0）".into()));
    }
    if text.is_empty() {
        return Err(InjectError("发送内容为空".into()));
    }

    unsafe {
        // 本程序是 GUI 子系统，正常无控制台；若已有则先释放，
        // 避免 AttachConsole 返回 ERROR_ACCESS_DENIED。
        let _ = FreeConsole();

        if let Err(e) = AttachConsole(pid) {
            return Err(InjectError(format!(
                "AttachConsole({pid}) 失败: {}（错误码 {}）",
                e.message(),
                GetLastError().0
            )));
        }

        let result = write_continue(text);

        // 与 AttachConsole 成对释放。
        let _ = FreeConsole();
        result
    }
}

fn write_continue(text: &str) -> Result<usize, InjectError> {
    unsafe {
        let handle = match GetStdHandle(STD_INPUT_HANDLE) {
            Ok(h) => h,
            Err(e) => return Err(InjectError(format!("GetStdHandle 失败: {}", e.message()))),
        };
        if handle.is_invalid() {
            return Err(InjectError(
                "目标没有可用的标准输入句柄（不是控制台窗口？）".into(),
            ));
        }

        let events = build_key_events(text);
        let total = events.len() as u32;
        let mut written = 0u32;

        // 逐条写入，确保顺序（字符 -> 回车）。
        for rec in &events {
            let mut one = 0u32;
            WriteConsoleInputW(handle, std::slice::from_ref(rec), &mut one).map_err(|e| {
                InjectError(format!("WriteConsoleInputW 失败: {}", e.message()))
            })?;
            if one == 0 {
                return Err(InjectError(
                    "WriteConsoleInputW 写入 0 条，目标可能已退出或无控制台".into(),
                ));
            }
            written += one;
        }

        if written < total {
            return Err(InjectError(format!(
                "只写入 {written}/{total} 条按键事件，注入不完整"
            )));
        }
        Ok(written as usize)
    }
}