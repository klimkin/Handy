//! Windows reliable paste.
//!
//! Publishes the transcript as a *delayed-render* clipboard format
//! (`SetClipboardData(CF_UNICODETEXT, NULL)`) owned by a hidden message-only
//! window. Windows sends the owner `WM_RENDERFORMAT` when a consumer actually
//! requests the data — that message is the read receipt. The previous
//! clipboard contents (snapshotted with full format fidelity) are restored
//! once receipts go quiet (see `paste_tx::evaluate`), guarded by the clipboard
//! sequence number so we never clobber a newer user copy.
//!
//! Threading: clipboard ownership and delayed rendering are per-thread and
//! need a message pump, so the whole transaction lives on a dedicated worker
//! thread. The calling thread only sends the paste chord once the worker
//! signals the transcript is published, then returns; the wait, guarded
//! restore and auto-submit all finish on the worker.

use std::sync::{mpsc::Sender, Arc, Mutex, Once};
use std::thread;
use std::time::Instant;

use log::{error, info};
use tauri::Manager;
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{
    SetLastError, ERROR_SUCCESS, HANDLE, HGLOBAL, HINSTANCE, HWND, LPARAM, LRESULT, WPARAM,
};

use super::{
    evaluate, send_chord, settlement_result, Completion, PasteOptions, TxState, WaitDecision,
};
use crate::clipboard::send_return_key;
use crate::input::EnigoState;
use crate::managers::stats::trace::{
    Event, OutputFailure, OutputKind, OutputReceipt, OutputResult,
};
use crate::settings::{AutoSubmitKey, ClipboardHandling, PasteMethod};
use windows::Win32::Foundation::GlobalFree;
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, EnumClipboardFormats, GetClipboardData, GetClipboardOwner,
    GetClipboardSequenceNumber, OpenClipboard, RegisterClipboardFormatW, SetClipboardData,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Memory::{
    GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock, GMEM_MOVEABLE,
};
use windows::Win32::System::Ole::{
    CF_BITMAP, CF_DSPBITMAP, CF_DSPENHMETAFILE, CF_DSPMETAFILEPICT, CF_DSPTEXT, CF_ENHMETAFILE,
    CF_OWNERDISPLAY, CF_PALETTE, CF_UNICODETEXT,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CopyImage, CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetMessageW,
    GetWindowLongPtrW, KillTimer, PostQuitMessage, RegisterClassW, SetTimer, SetWindowLongPtrW,
    GDI_IMAGE_TYPE, GWLP_USERDATA, HWND_MESSAGE, IMAGE_FLAGS, MSG, WINDOW_EX_STYLE, WINDOW_STYLE,
    WM_DESTROYCLIPBOARD, WM_RENDERALLFORMATS, WM_RENDERFORMAT, WM_TIMER, WNDCLASSW,
};

const CLASS_NAME: PCWSTR = w!("HandyPasteTxWindow");
const TIMER_ID: usize = 1;
const TIMER_INTERVAL_MS: u32 = 25;
/// Skip clipboard formats larger than this when snapshotting.
const MAX_FORMAT_BYTES: usize = 64 * 1024 * 1024;

const IMAGE_BITMAP_TYPE: GDI_IMAGE_TYPE = GDI_IMAGE_TYPE(0);
const LR_CREATEDIBSECTION_FLAG: IMAGE_FLAGS = IMAGE_FLAGS(0x2000);

struct SavedFormat {
    format: u32,
    data: Vec<u8>,
}

pub(super) struct WinTxShared {
    state: Mutex<TxState>,
    completion: Mutex<Completion>,
    text: String,
    snapshot: Mutex<Vec<SavedFormat>>,
    /// Copied HBITMAP (as raw usize), restored via SetClipboardData.
    saved_bitmap: Mutex<Option<usize>>,
    sequence: Mutex<u32>,
    hwnd: std::sync::atomic::AtomicUsize,
    app_handle: tauri::AppHandle,
    auto_submit: bool,
    auto_submit_key: AutoSubmitKey,
    /// ClipboardHandling::CopyToClipboard — settle by leaving the transcript
    /// on the clipboard as plain text instead of restoring the snapshot.
    preserve_transcript: bool,
}

/// The transaction currently holding the clipboard, if any. A new
/// transaction settles it before snapshotting (see `flush_pending`).
static PENDING: Mutex<Option<Arc<WinTxShared>>> = Mutex::new(None);

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

unsafe fn shared_ptr(hwnd: HWND) -> *const WinTxShared {
    GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const WinTxShared
}

/// Sends the auto-submit Enter. Uses `try_lock` because the paste caller may
/// currently hold the enigo lock while waiting for this worker.
fn send_auto_submit(
    shared: &WinTxShared,
    enigo: Option<&mut enigo::Enigo>,
) -> Result<(), OutputFailure> {
    if let Some(enigo) = enigo {
        return send_return_key(enigo, shared.auto_submit_key)
            .map_err(|_| OutputFailure::AutoSubmit);
    }
    let state = shared
        .app_handle
        .try_state::<EnigoState>()
        .ok_or(OutputFailure::AutoSubmit)?;
    let mut enigo = state.0.try_lock().map_err(|_| OutputFailure::AutoSubmit)?;
    send_return_key(&mut enigo, shared.auto_submit_key).map_err(|_| OutputFailure::AutoSubmit)
}

/// Renders the promised transcript into the clipboard, which must already be
/// open: the system opens it on our behalf for WM_RENDERFORMAT; every other
/// caller has to wrap this in OpenClipboard/CloseClipboard itself.
unsafe fn render_text(shared: &WinTxShared) -> Result<(), OutputFailure> {
    let wide_text: Vec<u16> = shared
        .text
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let hg = GlobalAlloc(GMEM_MOVEABLE, wide_text.len() * 2).map_err(|_| OutputFailure::Other)?;
    let ptr = GlobalLock(hg) as *mut u16;
    if ptr.is_null() {
        let _ = GlobalFree(Some(hg));
        return Err(OutputFailure::Other);
    }
    std::ptr::copy_nonoverlapping(wide_text.as_ptr(), ptr, wide_text.len());
    let _ = GlobalUnlock(hg);
    if SetClipboardData(CF_UNICODETEXT.0 as u32, Some(HANDLE(hg.0))).is_err() {
        let _ = GlobalFree(Some(hg));
        return Err(OutputFailure::Other);
    }
    Ok(())
}

unsafe extern "system" fn paste_wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    let shared = shared_ptr(hwnd);
    match msg {
        WM_RENDERFORMAT => {
            if !shared.is_null() {
                let shared = &*shared;
                if wparam.0 as u32 == CF_UNICODETEXT.0 as u32 {
                    let at = Instant::now();
                    let rendered = render_text(shared);
                    if rendered.is_ok() && GetClipboardOwner().is_ok_and(|owner| owner == hwnd) {
                        *shared
                            .sequence
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner) =
                            GetClipboardSequenceNumber();
                    }
                    if let Ok(mut st) = shared.state.lock() {
                        st.record_receipt(at);
                        st.delivery_failed |= rendered.is_err();
                    }
                }
            }
            LRESULT(0)
        }
        WM_RENDERALLFORMATS => {
            // Sent when the window is destroyed while an unrendered promise is
            // still on the clipboard — not a consumer read, so no receipt.
            // Unlike WM_RENDERFORMAT the system does not open the clipboard on
            // our behalf here: open it and confirm we still own it first.
            if !shared.is_null() {
                let shared = &*shared;
                if OpenClipboard(Some(hwnd)).is_ok() {
                    if GetClipboardOwner()
                        .map(|owner| owner == hwnd)
                        .unwrap_or(false)
                    {
                        let _ = render_text(shared);
                    }
                    let _ = CloseClipboard();
                }
            }
            LRESULT(0)
        }
        WM_DESTROYCLIPBOARD => {
            if !shared.is_null() {
                if let Ok(mut st) = (&*shared).state.lock() {
                    st.ownership_lost = true;
                }
            }
            LRESULT(0)
        }
        WM_TIMER => {
            if !shared.is_null() {
                on_timer(hwnd, &*shared);
            }
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

fn ensure_window_class(hinstance: HINSTANCE) {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let wc = WNDCLASSW {
            lpfnWndProc: Some(paste_wnd_proc),
            hInstance: hinstance,
            lpszClassName: CLASS_NAME,
            ..Default::default()
        };
        unsafe {
            RegisterClassW(&wc);
        }
    });
}

/// If a previous transaction is still holding the clipboard, settle it now so
/// the snapshot below captures the user's original clipboard content. The
/// previous worker observes `cancelled` on its next timer tick and tears down
/// without restoring.
fn flush_pending(enigo: &mut enigo::Enigo) {
    let previous = PENDING
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take();
    if let Some(previous) = previous {
        settle(&previous, Some(enigo), true);
    }
}

fn settle(shared: &WinTxShared, enigo: Option<&mut enigo::Enigo>, superseded: bool) -> bool {
    // Main-thread restoration can synchronously dispatch messages to this
    // window. Its timer must stay available while another caller settles.
    let mut completion = if superseded {
        shared
            .completion
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    } else {
        match shared.completion.try_lock() {
            Ok(completion) => completion,
            Err(std::sync::TryLockError::WouldBlock) => return false,
            Err(std::sync::TryLockError::Poisoned(error)) => error.into_inner(),
        }
    };
    if !completion.is_pending() {
        return true;
    }
    let sequence = *shared
        .sequence
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let (receipt, still_ours) = {
        let mut state = shared
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let ours = !state.ownership_lost && unsafe { GetClipboardSequenceNumber() } == sequence;
        state.cancelled = true;
        (
            state.any_receipt_after_injection()
                && !state.injection_failed
                && !state.delivery_failed,
            ours,
        )
    };
    completion.settle_with(|| {
        let submit = if shared.auto_submit && receipt && still_ours {
            send_auto_submit(shared, enigo)
        } else {
            Ok(())
        };
        let (result, still_ours) = {
            let state = shared
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let ours = !state.ownership_lost && unsafe { GetClipboardSequenceNumber() } == sequence;
            (settlement_result(&state, ours, superseded), ours)
        };
        let restore = if still_ours {
            unsafe { settle_clipboard(shared) }
        } else {
            Ok(())
        };
        match submit.and(restore) {
            Ok(()) => result,
            Err(failure) => OutputResult::Failed(failure),
        }
    });
    true
}

unsafe fn settle_clipboard(shared: &WinTxShared) -> Result<(), OutputFailure> {
    if !shared.preserve_transcript {
        return restore_snapshot(shared);
    }
    let hwnd = HWND(shared.hwnd.load(std::sync::atomic::Ordering::Acquire) as *mut _);
    OpenClipboard(Some(hwnd)).map_err(|_| OutputFailure::Restore)?;
    let result = EmptyClipboard()
        .map_err(|_| OutputFailure::Restore)
        .and_then(|_| render_text(shared).map_err(|_| OutputFailure::Restore));
    let closed = CloseClipboard().map_err(|_| OutputFailure::Restore);
    result.and(closed)
}

unsafe fn restore_snapshot(shared: &WinTxShared) -> Result<(), OutputFailure> {
    let hwnd = HWND(shared.hwnd.load(std::sync::atomic::Ordering::Acquire) as *mut _);
    OpenClipboard(Some(hwnd)).map_err(|_| OutputFailure::Restore)?;
    let result = (|| {
        EmptyClipboard().map_err(|_| OutputFailure::Restore)?;
        let formats = shared.snapshot.lock().map_err(|_| OutputFailure::Restore)?;
        for saved in formats.iter() {
            if saved.data.is_empty() {
                continue;
            }
            let hg =
                GlobalAlloc(GMEM_MOVEABLE, saved.data.len()).map_err(|_| OutputFailure::Restore)?;
            let ptr = GlobalLock(hg) as *mut u8;
            if ptr.is_null() {
                let _ = GlobalFree(Some(hg));
                return Err(OutputFailure::Restore);
            }
            std::ptr::copy_nonoverlapping(saved.data.as_ptr(), ptr, saved.data.len());
            let _ = GlobalUnlock(hg);
            if SetClipboardData(saved.format, Some(HANDLE(hg.0))).is_err() {
                let _ = GlobalFree(Some(hg));
                return Err(OutputFailure::Restore);
            }
        }
        if let Some(raw) = shared
            .saved_bitmap
            .lock()
            .map_err(|_| OutputFailure::Restore)?
            .take()
        {
            SetClipboardData(CF_BITMAP.0 as u32, Some(HANDLE(raw as *mut _)))
                .map_err(|_| OutputFailure::Restore)?;
        }
        Ok(())
    })();
    let closed = CloseClipboard().map_err(|_| OutputFailure::Restore);
    result.and(closed)
}

unsafe fn snapshot_clipboard(hwnd: HWND, shared: &WinTxShared) -> Result<(), String> {
    OpenClipboard(Some(hwnd)).map_err(|e| format!("OpenClipboard failed: {e}"))?;
    let mut formats = Vec::new();
    let mut format = 0u32;
    loop {
        format = EnumClipboardFormats(format);
        if format == 0 {
            break;
        }
        if format == CF_BITMAP.0 as u32 {
            // GDI object, not global memory: duplicate the handle instead.
            if let Ok(handle) = GetClipboardData(CF_BITMAP.0 as u32) {
                if let Ok(copy) =
                    CopyImage(handle, IMAGE_BITMAP_TYPE, 0, 0, LR_CREATEDIBSECTION_FLAG)
                {
                    if let Ok(mut slot) = shared.saved_bitmap.lock() {
                        *slot = Some(copy.0 as usize);
                    }
                }
            }
            continue;
        }
        // Formats whose handles are not plain global memory cannot be
        // byte-copied; skipping them matches what the legacy path restored.
        if format == CF_ENHMETAFILE.0 as u32
            || format == CF_DSPENHMETAFILE.0 as u32
            || format == CF_DSPBITMAP.0 as u32
            || format == CF_DSPMETAFILEPICT.0 as u32
            || format == CF_DSPTEXT.0 as u32
            || format == CF_OWNERDISPLAY.0 as u32
            || format == CF_PALETTE.0 as u32
        {
            continue;
        }
        if let Ok(handle) = GetClipboardData(format) {
            let hg = HGLOBAL(handle.0);
            let size = GlobalSize(hg);
            if size == 0 || size > MAX_FORMAT_BYTES {
                continue;
            }
            let ptr = GlobalLock(hg) as *const u8;
            if ptr.is_null() {
                continue;
            }
            let data = std::slice::from_raw_parts(ptr, size).to_vec();
            let _ = GlobalUnlock(hg);
            formats.push(SavedFormat { format, data });
        }
    }
    let _ = CloseClipboard();
    if let Ok(mut slot) = shared.snapshot.lock() {
        *slot = formats;
    }
    Ok(())
}

/// Publishes the transcript as a delayed-render promise plus clipboard
/// history / cloud / monitoring opt-out markers (the same formats Chrome uses
/// for Incognito copies). Returns the new clipboard sequence number.
unsafe fn publish(hwnd: HWND) -> Result<u32, String> {
    OpenClipboard(Some(hwnd)).map_err(|e| format!("OpenClipboard failed: {e}"))?;
    let published = publish_formats();
    let closed = CloseClipboard();
    published?;
    closed.map_err(|e| format!("CloseClipboard failed: {e}"))?;
    Ok(GetClipboardSequenceNumber())
}

/// Everything `publish` does while the clipboard is open, split out so
/// `publish` closes the clipboard on every path — bailing out while holding it
/// open (and possibly already emptied) would strand the clipboard and leave
/// the legacy fallback snapshotting nothing.
unsafe fn publish_formats() -> Result<(), String> {
    EmptyClipboard().map_err(|e| format!("EmptyClipboard failed: {e}"))?;

    for (name, value) in [
        ("ExcludeClipboardContentFromMonitorProcessing", 1u32),
        ("CanIncludeInClipboardHistory", 0u32),
        ("CanUploadToCloudClipboard", 0u32),
    ] {
        let name_wide = wide(name);
        let format = RegisterClipboardFormatW(PCWSTR(name_wide.as_ptr()));
        if format == 0 {
            continue;
        }
        if let Ok(hg) = GlobalAlloc(GMEM_MOVEABLE, std::mem::size_of::<u32>()) {
            let ptr = GlobalLock(hg) as *mut u32;
            if !ptr.is_null() {
                *ptr = value;
                let _ = GlobalUnlock(hg);
                if SetClipboardData(format, Some(HANDLE(hg.0))).is_err() {
                    let _ = GlobalFree(Some(hg));
                }
            } else {
                let _ = GlobalFree(Some(hg));
            }
        }
    }

    // NULL handle = delayed rendering: we are only asked for the data (via
    // WM_RENDERFORMAT) when a consumer actually reads it. SetClipboardData
    // returns the handle it was given, so for delayed rendering success is
    // also NULL and the windows crate reports it as an Err carrying
    // GetLastError(). Only a nonzero thread error is a real failure, and the
    // thread error must be cleared first so a stale value from an earlier
    // call can't masquerade as one.
    SetLastError(ERROR_SUCCESS);
    if let Err(e) = SetClipboardData(CF_UNICODETEXT.0 as u32, None) {
        if e.code().is_err() {
            return Err(format!("SetClipboardData failed: {e}"));
        }
    }
    Ok(())
}

fn on_timer(_hwnd: HWND, shared: &WinTxShared) {
    let finish = {
        let state = shared
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.cancelled || matches!(evaluate(&state, Instant::now()), WaitDecision::Finish)
    };
    if !finish {
        return;
    }
    if !settle(shared, None, false) {
        return;
    }
    if let Ok(mut slot) = PENDING.lock() {
        let is_us = slot
            .as_ref()
            .map(|pending| Arc::as_ptr(pending) == shared as *const WinTxShared)
            .unwrap_or(false);
        if is_us {
            *slot = None;
        }
    }
    unsafe {
        PostQuitMessage(0);
    }
}

unsafe fn destroy_window_and_shared(hwnd: HWND) {
    let ptr = shared_ptr(hwnd);
    let _ = DestroyWindow(hwnd);
    if !ptr.is_null() {
        drop(Arc::from_raw(ptr));
    }
}

fn pump_thread(shared: Arc<WinTxShared>, ready: Sender<Result<(), String>>) {
    unsafe {
        let hinstance = match GetModuleHandleW(PCWSTR::null()) {
            Ok(hmodule) => HINSTANCE(hmodule.0),
            Err(e) => {
                let _ = ready.send(Err(format!("GetModuleHandle failed: {e}")));
                return;
            }
        };
        ensure_window_class(hinstance);

        let hwnd = match CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            CLASS_NAME,
            w!("HandyPasteTx"),
            WINDOW_STYLE::default(),
            0,
            0,
            0,
            0,
            Some(HWND_MESSAGE),
            None,
            Some(hinstance),
            None,
        ) {
            Ok(hwnd) => hwnd,
            Err(e) => {
                let _ = ready.send(Err(format!("CreateWindowEx failed: {e}")));
                return;
            }
        };
        shared
            .hwnd
            .store(hwnd.0 as usize, std::sync::atomic::Ordering::Release);
        SetWindowLongPtrW(
            hwnd,
            GWLP_USERDATA,
            Arc::into_raw(shared.clone()) as *const _ as isize,
        );

        if SetTimer(Some(hwnd), TIMER_ID, TIMER_INTERVAL_MS, None) == 0 {
            destroy_window_and_shared(hwnd);
            let _ = ready.send(Err("Reliable paste timer unavailable".into()));
            return;
        }

        let published = match snapshot_clipboard(hwnd, &shared) {
            Ok(()) => match publish(hwnd) {
                Ok(sequence) => Ok(sequence),
                Err(e) => {
                    // publish may have emptied the clipboard before failing;
                    // put the snapshot back so the legacy fallback's own
                    // snapshot captures the user's clipboard, not an empty one.
                    let _ = restore_snapshot(&shared);
                    Err(e)
                }
            },
            Err(e) => Err(e),
        };
        let sequence = match published {
            Ok(sequence) => sequence,
            Err(e) => {
                destroy_window_and_shared(hwnd);
                let _ = ready.send(Err(e));
                return;
            }
        };
        *shared.sequence.lock().unwrap() = sequence;
        shared.state.lock().unwrap().published_at = Instant::now();
        if let Ok(mut slot) = PENDING.lock() {
            *slot = Some(shared.clone());
        }
        let _ = ready.send(Ok(()));

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = DispatchMessageW(&msg);
        }

        shared
            .completion
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .settle(OutputResult::Failed(OutputFailure::Disconnected), || Ok(()));
        if let Ok(mut slot) = PENDING.lock() {
            if slot
                .as_ref()
                .is_some_and(|pending| Arc::ptr_eq(pending, &shared))
            {
                *slot = None;
            }
        }
        let _ = KillTimer(Some(hwnd), TIMER_ID);
        destroy_window_and_shared(hwnd);
    }
}

pub(super) fn run(
    text: &str,
    app_handle: &tauri::AppHandle,
    paste_method: &PasteMethod,
    enigo: &mut enigo::Enigo,
    options: PasteOptions,
) -> Result<OutputReceipt, String> {
    let PasteOptions {
        auto_submit,
        auto_submit_key,
        clipboard_handling,
        delay_after,
        trace,
    } = options;
    flush_pending(enigo);
    let (completion, receiver) = Completion::channel();
    let shared = Arc::new(WinTxShared {
        state: Mutex::new(TxState::new()),
        completion: Mutex::new(completion),
        text: text.to_string(),
        snapshot: Mutex::new(Vec::new()),
        saved_bitmap: Mutex::new(None),
        sequence: Mutex::new(0),
        hwnd: std::sync::atomic::AtomicUsize::new(0),
        app_handle: app_handle.clone(),
        auto_submit,
        auto_submit_key,
        preserve_transcript: clipboard_handling == ClipboardHandling::CopyToClipboard,
    });

    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    let shared_for_pump = shared.clone();
    thread::spawn(move || pump_thread(shared_for_pump, ready_tx));

    // Wait until the transcript is actually published (or the worker reports
    // why it could not) before injecting the chord.
    match ready_rx.recv() {
        Ok(Ok(())) => {}
        Ok(Err(e)) => return Err(e),
        Err(_) => return Err("reliable paste worker died before publishing".to_string()),
    }
    info!("[reliable-paste] published transcript (delayed render)");

    // Mark injection *before* sending: enigo holds the chord for ~100ms and a
    // fast target may legitimately read while the chord is still held.
    shared.state.lock().unwrap().injected_at = Some(Instant::now());
    match send_chord(enigo, paste_method) {
        Ok(()) => {
            if let Some(trace) = trace {
                trace.event(Instant::now(), Event::PasteInjected);
            }
            info!("[reliable-paste] paste chord sent ({paste_method:?})");
        }
        Err(e) => {
            // Keep the transaction alive: the worker restores the clipboard
            // after the short failed-injection timeout.
            shared.state.lock().unwrap().injection_failed = true;
            error!("[reliable-paste] failed to send paste chord: {e}");
        }
    }

    {
        let mut state = shared
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.settle_after = Some(Instant::now() + delay_after);
        state.injection_finished = true;
    }
    Ok(OutputReceipt::pending(receiver, OutputKind::ClipboardPaste))
}
