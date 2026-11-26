//! Narrow Windows operations whose targets are tied to a selected League install.

use crate::client_discovery::{path_eq, LocalClient};
use std::{
    ffi::OsString,
    os::windows::{
        ffi::OsStringExt,
        io::{AsRawHandle, FromRawHandle, OwnedHandle},
    },
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
};
use windows_sys::{
    core::BOOL,
    Win32::{
        Foundation::{GetLastError, HWND, LPARAM, RECT, WAIT_TIMEOUT, WPARAM},
        System::Threading::{
            GetCurrentThreadId, OpenProcess, QueryFullProcessImageNameW, TerminateProcess,
            PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE, PROCESS_TERMINATE,
        },
        UI::{
            Input::KeyboardAndMouse::{
                RegisterHotKey, UnregisterHotKey, MOD_ALT, MOD_CONTROL, MOD_NOREPEAT, MOD_SHIFT,
            },
            Shell::{DefSubclassProc, RemoveWindowSubclass, SetWindowSubclass},
            WindowsAndMessaging::{
                EnumWindows, GetClassNameW, GetClientRect, GetForegroundWindow, GetWindowRect,
                GetWindowThreadProcessId, IsIconic, IsZoomed, SetWindowPos, SWP_NOACTIVATE,
                SWP_NOOWNERZORDER, SWP_NOZORDER, WM_HOTKEY, WM_NCDESTROY,
            },
        },
    },
};

const HOTKEY_ID: i32 = 0x4d47;
const SUBCLASS_ID: usize = 0x4d4753;
const MAIN_WINDOW_CLASS: &str = "RCLIENT";
const CEF_WINDOW_CLASS: &str = "CefBrowserWindow";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct HotkeySpec {
    modifiers: u32,
    virtual_key: u32,
}

/// Parse the pinned renderer's shortcut IDs supported by RegisterHotKey.
/// Only explicit Ctrl/Alt/Shift combinations are accepted; bare keys are never consumed.
pub(crate) fn parse_shortcut(shortcut: &str) -> Result<HotkeySpec, String> {
    let mut modifiers = 0;
    let mut virtual_key = None;
    for part in shortcut.split('+') {
        if part.is_empty() {
            return Err("快捷键格式无效，请使用 Ctrl、Alt 或 Shift 加一个按键。".into());
        }
        let modifier = match part {
            "Control" => Some(MOD_CONTROL),
            "Alt" => Some(MOD_ALT),
            "Shift" => Some(MOD_SHIFT),
            _ => None,
        };
        if let Some(modifier) = modifier {
            if modifiers & modifier != 0 {
                return Err("快捷键不能重复包含同一个修饰键。".into());
            }
            modifiers |= modifier;
            continue;
        }
        if virtual_key.is_some() {
            return Err("快捷键只能包含一个主按键。".into());
        }
        let key = if part.len() == 1
            && (part.as_bytes()[0].is_ascii_uppercase() || part.as_bytes()[0].is_ascii_digit())
        {
            Some(part.as_bytes()[0] as u32)
        } else if part.starts_with('F') {
            part[1..]
                .parse::<u32>()
                .ok()
                .filter(|number| (1..=11).contains(number))
                .map(|number| 0x70 + number - 1)
        } else {
            None
        };
        let Some(key) = key else {
            return Err("此快捷键按键暂不支持；请使用字母、数字或 F1 至 F11。".into());
        };
        virtual_key = Some(key);
    }
    if modifiers == 0 || virtual_key.is_none() {
        return Err("快捷键必须包含 Ctrl、Alt 或 Shift，并带一个主按键。".into());
    }
    let main_key = virtual_key.expect("checked above");
    let letter_or_digit = (b'0' as u32..=b'9' as u32).contains(&main_key)
        || (b'A' as u32..=b'Z' as u32).contains(&main_key);
    if letter_or_digit && modifiers.count_ones() < 2 {
        return Err("字母和数字快捷键必须同时使用两个修饰键，以免拦截常用输入。".into());
    }
    if main_key == 0x73 && modifiers == MOD_ALT {
        return Err("Alt+F4 是 Windows 关闭窗口快捷键，不能分配给游戏操作。".into());
    }
    Ok(HotkeySpec {
        modifiers: modifiers | MOD_NOREPEAT,
        virtual_key: main_key,
    })
}

type Dispatch = Arc<dyn Fn() + Send + Sync + 'static>;

struct HotkeyBinding {
    alive: AtomicBool,
    registration: Mutex<Option<HotkeySpec>>,
    dispatch: Dispatch,
}

/// A RegisterHotKey binding attached to the Tauri main window's UI thread.
/// The subclass callback is freed by WM_NCDESTROY, so Tauri state may drop on any thread.
pub(crate) struct GameClientHotkey {
    hwnd: usize,
    owner_thread: u32,
    binding: Arc<HotkeyBinding>,
}

impl GameClientHotkey {
    pub(crate) fn attach<F>(hwnd: HWND, on_hotkey: F) -> Result<Self, String>
    where
        F: Fn() + Send + Sync + 'static,
    {
        if hwnd.is_null() {
            return Err("找不到主窗口，无法注册游戏快捷键。".into());
        }
        let mut process_id = 0;
        let owner_thread = unsafe { GetWindowThreadProcessId(hwnd, &mut process_id) };
        if owner_thread == 0 || process_id != std::process::id() {
            return Err("主窗口不属于当前工具箱进程。".into());
        }
        if owner_thread != unsafe { GetCurrentThreadId() } {
            return Err("快捷键控制器必须在主窗口 UI 线程初始化。".into());
        }

        let binding = Arc::new(HotkeyBinding {
            alive: AtomicBool::new(true),
            registration: Mutex::new(None),
            dispatch: Arc::new(on_hotkey),
        });
        let ref_data = Arc::into_raw(binding.clone()) as usize;
        let installed =
            unsafe { SetWindowSubclass(hwnd, Some(subclass_proc), SUBCLASS_ID, ref_data) };
        if installed == 0 {
            unsafe { drop(Arc::from_raw(ref_data as *const HotkeyBinding)) };
            return Err(format!(
                "无法安装游戏快捷键处理器（Windows 错误码 {}）。",
                unsafe { GetLastError() }
            ));
        }
        Ok(Self {
            hwnd: hwnd as usize,
            owner_thread,
            binding,
        })
    }

    /// Validate independently before the caller persists a renderer setting.
    pub(crate) fn validate(shortcut: &str) -> Result<(), String> {
        parse_shortcut(shortcut).map(|_| ())
    }

    /// Replace or clear the single application-owned global shortcut.
    pub(crate) fn replace(&self, shortcut: Option<&str>, enabled: bool) -> Result<(), String> {
        if unsafe { GetCurrentThreadId() } != self.owner_thread {
            return Err("快捷键只能在主窗口 UI 线程更新。".into());
        }
        if !self.binding.alive.load(Ordering::Acquire) {
            return Err("主窗口已关闭，无法更新游戏快捷键。".into());
        }
        let next = if enabled {
            Some(parse_shortcut(
                shortcut.ok_or("启用游戏快捷键前必须先选择组合键。")?,
            )?)
        } else {
            None
        };
        let mut current = self
            .binding
            .registration
            .lock()
            .map_err(|_| "快捷键状态不可用。")?;
        if *current == next {
            return Ok(());
        }
        let hwnd = self.hwnd as HWND;
        let previous = *current;
        if previous.is_some() {
            if unsafe { UnregisterHotKey(hwnd, HOTKEY_ID) } == 0 {
                return Err(last_error("Windows 无法撤销旧的游戏快捷键"));
            }
            *current = None;
        }
        if let Some(spec) = next {
            if unsafe { RegisterHotKey(hwnd, HOTKEY_ID, spec.modifiers, spec.virtual_key) } == 0 {
                let error = unsafe { GetLastError() };
                let restored = if let Some(previous) = previous {
                    if unsafe {
                        RegisterHotKey(hwnd, HOTKEY_ID, previous.modifiers, previous.virtual_key)
                    } != 0
                    {
                        *current = Some(previous);
                        true
                    } else {
                        false
                    }
                } else {
                    true
                };
                return Err(if !restored {
                    "新快捷键注册失败，且旧快捷键无法恢复；请重新设置。".into()
                } else if error == 1409 {
                    "此快捷键已被其他程序占用。".into()
                } else {
                    format!("Windows 无法注册快捷键（错误码 {error}）。")
                });
            }
            *current = Some(spec);
        }
        Ok(())
    }
}

unsafe extern "system" fn subclass_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    subclass_id: usize,
    ref_data: usize,
) -> isize {
    if message == WM_HOTKEY && wparam == HOTKEY_ID as usize {
        let binding = &*(ref_data as *const HotkeyBinding);
        let spec = binding
            .registration
            .lock()
            .ok()
            .and_then(|binding| *binding);
        let modifiers = (lparam as u32 & 0xffff) & !MOD_NOREPEAT;
        let virtual_key = (lparam as u32 >> 16) & 0xffff;
        if spec.is_some_and(|spec| {
            spec.modifiers & !MOD_NOREPEAT == modifiers && spec.virtual_key == virtual_key
        }) {
            let dispatch = binding.dispatch.clone();
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| dispatch()));
        }
    } else if message == WM_NCDESTROY {
        UnregisterHotKey(hwnd, HOTKEY_ID);
        let binding = &*(ref_data as *const HotkeyBinding);
        binding.alive.store(false, Ordering::Release);
        if let Ok(mut registration) = binding.registration.lock() {
            *registration = None;
        }
        RemoveWindowSubclass(hwnd, Some(subclass_proc), subclass_id);
        drop(Arc::from_raw(ref_data as *const HotkeyBinding));
    }
    DefSubclassProc(hwnd, message, wparam, lparam)
}

/// Resize the one League UX window verified against the selected client's retained identity.
pub(crate) fn resize_league_client_window(
    client: &LocalClient,
    base_width: u32,
    base_height: u32,
    zoom: f64,
) -> Result<(), String> {
    let width = scaled_dimension(base_width, zoom)?;
    let height = scaled_dimension(base_height, zoom)?;
    let ux = client
        .verified_ux_process()
        .map_err(|error| format!("无法验证当前 League 客户端窗口：{error}"))?;
    if !ux.alive() {
        return Err("League 客户端窗口进程已退出。".into());
    }
    if ux.created_at < client.instance.created_at {
        return Err("League 客户端窗口早于所选客户端实例。".into());
    }
    let parent = exact_ux_window(ux.pid)?;
    if !ux.alive() || !window_owned_by(parent, ux.pid) {
        return Err("League 客户端窗口身份已变化，请重试。".into());
    }
    if unsafe { IsIconic(parent) } != 0 || unsafe { IsZoomed(parent) } != 0 {
        return Err("请先还原 League 客户端窗口，再调整大小。".into());
    }
    if let Some(child) = exact_cef_child(parent, ux.pid)? {
        resize_owned_windows(parent, Some(child), ux.pid, width, height)?;
    } else {
        resize_owned_windows(parent, None, ux.pid, width, height)?;
    }
    if !ux.alive() || !window_owned_by(parent, ux.pid) {
        return Err("League 客户端窗口在调整期间已关闭或变化。".into());
    }
    Ok(())
}

fn resize_owned_windows(
    parent: HWND,
    child: Option<HWND>,
    expected_pid: u32,
    width: u32,
    height: u32,
) -> Result<(), String> {
    if !window_owned_by(parent, expected_pid)
        || child.is_some_and(|window| !window_owned_by(window, expected_pid))
    {
        return Err("League 客户端窗口所属进程与已验证身份不匹配。".into());
    }
    let mut original = RECT::default();
    if unsafe { GetWindowRect(parent, &mut original) } == 0 {
        return Err(last_error("无法读取 League 客户端窗口大小"));
    }
    let flags = SWP_NOZORDER | SWP_NOACTIVATE | SWP_NOOWNERZORDER;
    if unsafe {
        SetWindowPos(
            parent,
            std::ptr::null_mut(),
            original.left,
            original.top,
            width as i32,
            height as i32,
            flags,
        )
    } == 0
    {
        return Err(last_error("Windows 拒绝调整 League 客户端窗口"));
    }
    let mut resized = RECT::default();
    if unsafe { GetWindowRect(parent, &mut resized) } == 0
        || resized.right - resized.left != width as i32
        || resized.bottom - resized.top != height as i32
    {
        return Err("League 客户端窗口大小未达到请求值。".into());
    }
    if let Some(child) = child {
        if !window_owned_by(parent, expected_pid) || !window_owned_by(child, expected_pid) {
            return Err("League 客户端内容窗口所属进程已变化。".into());
        }
        let mut client_rect = RECT::default();
        if unsafe { GetClientRect(parent, &mut client_rect) } == 0 {
            return Err(last_error("无法读取 League 客户端内容区域"));
        }
        if unsafe {
            SetWindowPos(
                child,
                std::ptr::null_mut(),
                0,
                0,
                client_rect.right - client_rect.left,
                client_rect.bottom - client_rect.top,
                flags,
            )
        } == 0
        {
            let error = unsafe { GetLastError() };
            unsafe {
                SetWindowPos(
                    parent,
                    std::ptr::null_mut(),
                    original.left,
                    original.top,
                    original.right - original.left,
                    original.bottom - original.top,
                    flags,
                );
            }
            return Err(format!(
                "无法调整 League 客户端内容窗口（Windows 错误码 {error}）。"
            ));
        }
        let mut resized_child = RECT::default();
        if unsafe { GetWindowRect(child, &mut resized_child) } == 0
            || resized_child.right - resized_child.left != client_rect.right - client_rect.left
            || resized_child.bottom - resized_child.top != client_rect.bottom - client_rect.top
        {
            return Err("League 客户端内容窗口大小未达到请求值。".into());
        }
        if !window_owned_by(parent, expected_pid) || !window_owned_by(child, expected_pid) {
            return Err("League 客户端内容窗口在调整期间已变化。".into());
        }
    }
    Ok(())
}

fn scaled_dimension(base: u32, zoom: f64) -> Result<u32, String> {
    if !(320..=4096).contains(&base) || !zoom.is_finite() || !(0.25..=4.0).contains(&zoom) {
        return Err("窗口尺寸或缩放比例超出安全范围。".into());
    }
    let scaled = (f64::from(base) * zoom).round();
    if !(320.0..=8192.0).contains(&scaled) {
        return Err("缩放后的窗口尺寸超出安全范围。".into());
    }
    Ok(scaled as u32)
}

struct WindowSearch {
    pid: u32,
    class: &'static str,
    matches: Vec<HWND>,
}

unsafe extern "system" fn enumerate_target_window(hwnd: HWND, data: LPARAM) -> BOOL {
    let search = &mut *(data as *mut WindowSearch);
    if window_owned_by(hwnd, search.pid) && window_class(hwnd).as_deref() == Some(search.class) {
        search.matches.push(hwnd);
    }
    1
}

fn exact_ux_window(pid: u32) -> Result<HWND, String> {
    let mut search = WindowSearch {
        pid,
        class: MAIN_WINDOW_CLASS,
        matches: Vec::new(),
    };
    if unsafe {
        EnumWindows(
            Some(enumerate_target_window),
            &mut search as *mut _ as LPARAM,
        )
    } == 0
        && search.matches.is_empty()
    {
        return Err(last_error("无法枚举 League 客户端窗口"));
    }
    match search.matches.as_slice() {
        [window] => Ok(*window),
        [] => Err("未找到属于已验证 LeagueClientUx 进程的主窗口。".into()),
        _ => Err("已验证的 LeagueClientUx 进程包含多个主窗口，拒绝猜测目标。".into()),
    }
}

fn exact_cef_child(parent: HWND, pid: u32) -> Result<Option<HWND>, String> {
    let mut search = WindowSearch {
        pid,
        class: CEF_WINDOW_CLASS,
        matches: Vec::new(),
    };
    // The content HWND must be an actual child of the already-verified client window and must
    // still report the exact same UX process ID.
    enumerate_children(parent, &mut search);
    match search.matches.as_slice() {
        [window] => Ok(Some(*window)),
        [] => Ok(None),
        _ => Err("League 客户端包含多个内容窗口，拒绝猜测目标。".into()),
    }
}

fn enumerate_children(parent: HWND, search: &mut WindowSearch) {
    let mut child = unsafe {
        windows_sys::Win32::UI::WindowsAndMessaging::GetWindow(
            parent,
            windows_sys::Win32::UI::WindowsAndMessaging::GW_CHILD,
        )
    };
    while !child.is_null() {
        if window_owned_by(child, search.pid)
            && window_class(child).as_deref() == Some(search.class)
        {
            search.matches.push(child);
        }
        child = unsafe {
            windows_sys::Win32::UI::WindowsAndMessaging::GetWindow(
                child,
                windows_sys::Win32::UI::WindowsAndMessaging::GW_HWNDNEXT,
            )
        };
    }
}

fn window_owned_by(hwnd: HWND, expected_pid: u32) -> bool {
    if hwnd.is_null() {
        return false;
    }
    let mut pid = 0;
    unsafe { GetWindowThreadProcessId(hwnd, &mut pid) != 0 && pid == expected_pid }
}

fn window_class(hwnd: HWND) -> Option<String> {
    let mut buffer = [0u16; 256];
    let len = unsafe { GetClassNameW(hwnd, buffer.as_mut_ptr(), buffer.len() as i32) };
    (len > 0).then(|| String::from_utf16_lossy(&buffer[..len as usize]))
}

/// Terminate only the single foreground game process if its image is inside the selected install.
pub(crate) fn terminate_foreground_game(client: &LocalClient) -> Result<(), String> {
    if !client.alive() {
        return Err("所选 League 客户端已退出，请重新连接。".into());
    }
    let candidates = client
        .game_executable_candidates()
        .map_err(|error| format!("无法检查 League 游戏安装目录：{error}"))?;
    if candidates.is_empty() {
        return Err("所选客户端安装目录中没有可验证的 Game\\League of Legends.exe。".into());
    }
    let foreground = unsafe { GetForegroundWindow() };
    let process = verified_game_window_process(foreground, &candidates)?;
    if !client.alive()
        || unsafe { GetForegroundWindow() } != foreground
        || !window_owned_by(foreground, process.pid)
    {
        return Err("前台窗口或 League 客户端状态已变化，请重试。".into());
    }
    terminate_verified_process(&process)
}

struct VerifiedGameProcess {
    handle: OwnedHandle,
    pid: u32,
}

fn verified_game_window_process(
    foreground: HWND,
    candidates: &[PathBuf],
) -> Result<VerifiedGameProcess, String> {
    if foreground.is_null() {
        return Err("当前没有可验证的前台游戏窗口。".into());
    }
    if unsafe { GetForegroundWindow() } != foreground {
        return Err("指定窗口当前不在前台。".into());
    }
    let mut pid = 0;
    if unsafe { GetWindowThreadProcessId(foreground, &mut pid) } == 0 || pid == 0 {
        return Err("无法识别前台窗口所属进程。".into());
    }
    let process = verified_game_process(pid, candidates)?;
    let mut current_pid = 0;
    if unsafe { GetForegroundWindow() } != foreground
        || unsafe { GetWindowThreadProcessId(foreground, &mut current_pid) } == 0
        || current_pid != pid
    {
        return Err("前台窗口或所属进程状态已变化，请重试。".into());
    }
    Ok(process)
}

fn verified_game_process(pid: u32, candidates: &[PathBuf]) -> Result<VerifiedGameProcess, String> {
    let process_session = crate::client_discovery::session_id(pid);
    if process_session.is_none()
        || process_session != crate::client_discovery::session_id(std::process::id())
    {
        return Err("前台窗口不属于当前 Windows 会话。".into());
    }
    let handle = unsafe {
        OpenProcess(
            PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE | PROCESS_TERMINATE,
            0,
            pid,
        )
    };
    if handle.is_null() {
        return Err(last_error("无法安全打开前台进程"));
    }
    let process = unsafe { OwnedHandle::from_raw_handle(handle) };
    let image = process_image(&process).ok_or("无法验证前台进程的完整路径。")?;
    let image = std::fs::canonicalize(image).map_err(|_| "无法验证前台进程的完整路径。")?;
    if !candidates
        .iter()
        .any(|candidate| path_eq(candidate, &image))
    {
        return Err("前台窗口不是所选 League 安装中的游戏进程。".into());
    }
    if unsafe {
        windows_sys::Win32::System::Threading::WaitForSingleObject(process.as_raw_handle(), 0)
    } != WAIT_TIMEOUT
    {
        return Err("前台游戏进程已经退出。".into());
    }
    Ok(VerifiedGameProcess {
        handle: process,
        pid,
    })
}

fn terminate_verified_process(process: &VerifiedGameProcess) -> Result<(), String> {
    if unsafe { TerminateProcess(process.handle.as_raw_handle(), 0) } == 0 {
        return Err(last_error("Windows 拒绝结束已验证的 League 游戏进程"));
    }
    Ok(())
}

fn process_image(process: &OwnedHandle) -> Option<PathBuf> {
    let mut name = vec![0u16; 32_768];
    let mut units = name.len() as u32;
    (unsafe {
        QueryFullProcessImageNameW(process.as_raw_handle(), 0, name.as_mut_ptr(), &mut units)
    } != 0)
        .then(|| PathBuf::from(OsString::from_wide(&name[..units as usize])))
}

fn last_error(action: &str) -> String {
    format!("{action}（Windows 错误码 {}）。", unsafe {
        GetLastError()
    })
}

#[cfg(test)]
mod tests {
    use super::{
        parse_shortcut, resize_owned_windows, scaled_dimension, GameClientHotkey, HOTKEY_ID,
    };
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };
    use windows_sys::Win32::{
        Foundation::{GetLastError, HWND, LPARAM, RECT, WPARAM},
        System::LibraryLoader::GetModuleHandleW,
        System::Threading::WaitForSingleObject,
        UI::WindowsAndMessaging::{
            CreateWindowExW, DefWindowProcW, DestroyWindow, GetForegroundWindow, GetWindowRect,
            RegisterClassW, SendMessageW, WM_HOTKEY, WNDCLASSW, WS_CHILD, WS_EX_NOACTIVATE,
            WS_POPUP,
        },
    };

    struct WindowFixture {
        parent: HWND,
        child: HWND,
    }

    impl WindowFixture {
        fn create() -> Self {
            let class_name: Vec<u16> = "MgsHksLgtSyntheticWindow"
                .encode_utf16()
                .chain([0])
                .collect();
            let instance = unsafe { GetModuleHandleW(std::ptr::null()) };
            let class = WNDCLASSW {
                lpfnWndProc: Some(test_window_proc),
                hInstance: instance,
                lpszClassName: class_name.as_ptr(),
                ..Default::default()
            };
            if unsafe { RegisterClassW(&class) } == 0 {
                assert_eq!(
                    unsafe { GetLastError() },
                    1410,
                    "synthetic class registration failed"
                );
            }
            let parent = unsafe {
                CreateWindowExW(
                    WS_EX_NOACTIVATE,
                    class_name.as_ptr(),
                    std::ptr::null(),
                    WS_POPUP,
                    200,
                    200,
                    400,
                    300,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    instance,
                    std::ptr::null(),
                )
            };
            assert!(!parent.is_null(), "hidden synthetic parent creation failed");
            let child = unsafe {
                CreateWindowExW(
                    0,
                    class_name.as_ptr(),
                    std::ptr::null(),
                    WS_CHILD,
                    0,
                    0,
                    200,
                    150,
                    parent,
                    std::ptr::null_mut(),
                    instance,
                    std::ptr::null(),
                )
            };
            assert!(!child.is_null(), "synthetic child creation failed");
            Self { parent, child }
        }
    }

    impl Drop for WindowFixture {
        fn drop(&mut self) {
            unsafe {
                DestroyWindow(self.child);
                DestroyWindow(self.parent);
            }
        }
    }

    unsafe extern "system" fn test_window_proc(
        hwnd: HWND,
        message: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> isize {
        DefWindowProcW(hwnd, message, wparam, lparam)
    }

    #[test]
    fn register_hotkey_accepts_supported_modified_keys() {
        assert_eq!(
            parse_shortcut("Control+Alt+Q").unwrap(),
            super::HotkeySpec {
                modifiers: 0x0002 | 0x0001 | 0x4000,
                virtual_key: b'Q' as u32,
            }
        );
        assert_eq!(parse_shortcut("Shift+F8").unwrap().virtual_key, 0x77);
    }

    #[test]
    fn register_hotkey_rejects_unmodified_duplicate_and_unsupported_keys() {
        for shortcut in [
            "Q",
            "Control+Control+Q",
            "Meta+Q",
            "Control+Numpad1",
            "Control+Q",
            "Shift+Q",
            "Alt+F4",
            "",
        ] {
            assert!(parse_shortcut(shortcut).is_err(), "accepted {:?}", shortcut);
        }
    }

    #[test]
    fn register_hotkey_rejects_unmodified_and_noncanonical_keys() {
        for shortcut in [
            "control+Q",
            "Control+F12",
            "Control+F25",
            "Control+Q+A",
            "Control+Alt",
        ] {
            assert!(parse_shortcut(shortcut).is_err(), "accepted {:?}", shortcut);
        }
    }

    #[test]
    fn resize_dimensions_are_scaled_and_bounded() {
        assert_eq!(scaled_dimension(1280, 1.25).unwrap(), 1600);
        assert!(scaled_dimension(319, 1.0).is_err());
        assert!(scaled_dimension(1280, f64::NAN).is_err());
        assert!(scaled_dimension(4096, 4.0).is_err());
    }

    #[test]
    fn resize_changes_only_owned_hidden_windows_without_activating_them() {
        let windows = WindowFixture::create();
        let foreground_before = unsafe { GetForegroundWindow() };
        resize_owned_windows(
            windows.parent,
            Some(windows.child),
            std::process::id(),
            640,
            480,
        )
        .unwrap();
        let mut parent = RECT::default();
        let mut child = RECT::default();
        assert_ne!(unsafe { GetWindowRect(windows.parent, &mut parent) }, 0);
        assert_ne!(unsafe { GetWindowRect(windows.child, &mut child) }, 0);
        assert_eq!(
            (parent.right - parent.left, parent.bottom - parent.top),
            (640, 480)
        );
        assert_eq!(
            (child.right - child.left, child.bottom - child.top),
            (640, 480)
        );
        assert_eq!(unsafe { GetForegroundWindow() }, foreground_before);
    }

    #[test]
    fn resize_rejects_a_window_owned_by_another_pid_before_mutation() {
        let windows = WindowFixture::create();
        let mut before = RECT::default();
        assert_ne!(unsafe { GetWindowRect(windows.parent, &mut before) }, 0);
        let error = resize_owned_windows(
            windows.parent,
            Some(windows.child),
            std::process::id().wrapping_add(1),
            640,
            480,
        )
        .unwrap_err();
        let mut after = RECT::default();
        assert_ne!(unsafe { GetWindowRect(windows.parent, &mut after) }, 0);
        assert_eq!(
            (before.left, before.top, before.right, before.bottom),
            (after.left, after.top, after.right, after.bottom)
        );
        assert!(error.contains("所属进程"));
    }

    #[test]
    fn game_termination_refuses_a_task_owned_window_without_foreground_focus() {
        let windows = WindowFixture::create();
        let executable = std::fs::canonicalize(std::env::current_exe().unwrap()).unwrap();
        let result = super::verified_game_window_process(windows.parent, &[executable]);
        let Err(error) = result else {
            panic!("an unfocused synthetic window was accepted as the game process");
        };
        assert!(error.contains("前台"));
        assert_ne!(unsafe { GetForegroundWindow() }, windows.parent);
    }

    #[test]
    fn synthetic_game_process_child() {
        if std::env::var_os("MGS_HKSLGT_SYNTHETIC_GAME_CHILD").is_some() {
            std::thread::sleep(std::time::Duration::from_secs(60));
        }
    }

    #[test]
    fn verified_process_handle_terminates_only_the_test_owned_child() {
        use std::{
            os::windows::{io::AsRawHandle, process::CommandExt},
            process::Command,
        };
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;

        let executable = std::fs::canonicalize(std::env::current_exe().unwrap()).unwrap();
        let mut child = Command::new(&executable)
            .args([
                "--exact",
                "windows_tools::tests::synthetic_game_process_child",
                "--nocapture",
            ])
            .env("MGS_HKSLGT_SYNTHETIC_GAME_CHILD", "1")
            .creation_flags(CREATE_NO_WINDOW)
            .spawn()
            .unwrap();
        let result = (|| {
            let process =
                super::verified_game_process(child.id(), std::slice::from_ref(&executable))?;
            super::terminate_verified_process(&process)?;
            assert_eq!(
                unsafe { WaitForSingleObject(process.handle.as_raw_handle(), 5_000) },
                0,
                "TerminateProcess did not stop the test-owned process"
            );
            Ok::<(), String>(())
        })();
        if result.is_err() {
            let _ = child.kill();
        }
        let status = child.wait().unwrap();
        result.unwrap();
        assert_eq!(status.code(), Some(0));
    }

    #[test]
    fn registered_hotkey_dispatches_only_matching_enabled_messages() {
        let windows = WindowFixture::create();
        let count = Arc::new(AtomicUsize::new(0));
        let callback_count = count.clone();
        let controller = GameClientHotkey::attach(windows.parent, move || {
            callback_count.fetch_add(1, Ordering::SeqCst);
        })
        .unwrap();
        let shortcut = "Control+Alt+Shift+F11";
        controller.replace(Some(shortcut), true).unwrap();
        let spec = parse_shortcut(shortcut).unwrap();
        let modifiers = (spec.modifiers & !super::MOD_NOREPEAT) as u16;
        let expected = ((spec.virtual_key as u16 as u32) << 16) | modifiers as u32;
        unsafe {
            SendMessageW(
                windows.parent,
                WM_HOTKEY,
                HOTKEY_ID as WPARAM,
                expected as LPARAM,
            )
        };
        assert_eq!(count.load(Ordering::SeqCst), 1);

        unsafe { SendMessageW(windows.parent, WM_HOTKEY, HOTKEY_ID as WPARAM, 0) };
        assert_eq!(count.load(Ordering::SeqCst), 1);
        controller.replace(None, false).unwrap();
        unsafe {
            SendMessageW(
                windows.parent,
                WM_HOTKEY,
                HOTKEY_ID as WPARAM,
                expected as LPARAM,
            )
        };
        assert_eq!(count.load(Ordering::SeqCst), 1);
    }
}
