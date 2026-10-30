//! Receipt-sequenced clipboard paste ("reliable paste", debug-gated).
//!
//! The legacy clipboard paste (`clipboard::paste_via_clipboard`) restores the
//! previous clipboard after a fixed delay. The paste keystroke is only
//! *enqueued* at that point — the target application reads the clipboard
//! whenever its event loop gets to it, so any fixed delay can lose the race
//! and the user gets their old clipboard pasted back (#502).
//!
//! This module instead publishes the transcript as a *lazy promise* and waits
//! for the operating system to tell us that a consumer actually read the
//! clipboard — a "receipt" — before restoring:
//!
//! - Windows: delayed rendering (`SetClipboardData(CF_UNICODETEXT, NULL)`),
//!   the owner window receives `WM_RENDERFORMAT` on read.
//! - macOS: `declareTypes:owner:` with an owner object, the pasteboard calls
//!   `pasteboard:provideDataForType:` on read.
//!
//! Two rules make the receipt trustworthy:
//!
//! 1. Only receipts observed *after* the paste chord was injected count. A
//!    read before that is an eager third party (clipboard manager, antivirus)
//!    reacting to the clipboard change itself.
//! 2. Restoration only happens while we still own the clipboard
//!    (sequence number / changeCount unchanged, no ownership-lost event). If
//!    the user copied something else in the meantime, their action wins.
//!
//! The restore is additionally gated on a short quiet period after the *last*
//! receipt, because some applications read the clipboard several times per
//! paste (Chromium probes, then reads). A bounded timeout caps how long the
//! transcript may occupy the clipboard; the failure mode is always "the
//! transcript stays on the clipboard a bit longer", never "stale content gets
//! pasted".

// The shared transaction state is compiled on all platforms (for the unit
// tests), but only the macOS/Windows platform modules consume all of it.
#![cfg_attr(not(any(target_os = "macos", target_os = "windows")), allow(dead_code))]

use std::sync::mpsc::{Receiver, Sender};
use std::time::{Duration, Instant};

use crate::managers::stats::trace::{
    CompletionBasis, OutputFailure, OutputKind, OutputResult, OutputSettlement, UnconfirmedReason,
};

#[cfg(any(target_os = "macos", target_os = "windows"))]
use crate::managers::stats::trace::{OutputReceipt, TraceReporter};

#[cfg(any(target_os = "macos", target_os = "windows"))]
pub(crate) struct PasteOptions {
    pub auto_submit: bool,
    pub auto_submit_key: crate::settings::AutoSubmitKey,
    pub clipboard_handling: crate::settings::ClipboardHandling,
    pub delay_after: Duration,
    pub trace: Option<TraceReporter>,
}

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "windows")]
mod windows;

#[cfg(target_os = "macos")]
use macos as platform;
#[cfg(target_os = "windows")]
use windows as platform;

/// How long after the *last* observed read the transcript stays on the
/// clipboard before restoring. Covers applications that read the clipboard
/// several times per paste (e.g. Chromium probe-then-read).
pub(crate) const QUIET_PERIOD: Duration = Duration::from_millis(200);

/// Upper bound on how long the transcript may occupy the clipboard before we
/// restore regardless of receipts. Long enough that a realistically loaded
/// target always gets to read first; short enough that a lost keystroke does
/// not strand the transcript on the clipboard for long.
pub(crate) const RESTORE_TIMEOUT: Duration = Duration::from_secs(8);

/// When the chord could not be injected at all, no legitimate receipt can
/// arrive, so restore quickly instead of waiting out the full timeout.
pub(crate) const FAILED_INJECTION_TIMEOUT: Duration = Duration::from_millis(500);

/// Shared, cross-thread record of one paste transaction.
#[derive(Debug)]
pub(crate) struct TxState {
    /// When the transcript was published to the clipboard.
    pub published_at: Instant,
    /// When the paste chord was injected. Only receipts *after* this count as
    /// evidence the target read the transcript — earlier reads are eager third
    /// parties reacting to the clipboard change itself.
    pub injected_at: Option<Instant>,
    /// The chord could not be sent; short-circuit the wait.
    pub injection_failed: bool,
    pub delivery_failed: bool,
    /// Times at which a consumer requested the clipboard data.
    pub last_receipt: Option<Instant>,
    /// Prevent settlement while chord injection is still running.
    pub injection_finished: bool,
    pub settle_after: Option<Instant>,
    /// Someone else took clipboard ownership (user copied elsewhere, ...).
    pub ownership_lost: bool,
    /// A newer paste transaction settled this one early (see flush logic in
    /// the platform modules).
    pub cancelled: bool,
    /// First post-injection receipt has been logged.
    pub logged_receipt: bool,
}

impl TxState {
    pub fn new() -> Self {
        Self {
            published_at: Instant::now(),
            injected_at: None,
            injection_failed: false,
            delivery_failed: false,
            last_receipt: None,
            injection_finished: false,
            settle_after: None,
            ownership_lost: false,
            cancelled: false,
            logged_receipt: false,
        }
    }

    /// Records a read receipt, logging the first one that counts as evidence.
    pub fn record_receipt(&mut self, at: Instant) {
        self.last_receipt = Some(at);
        if !self.logged_receipt {
            if let Some(injected) = self.injected_at {
                if at >= injected {
                    self.logged_receipt = true;
                    log::info!(
                        "[reliable-paste] clipboard read {}ms after chord",
                        at.duration_since(injected).as_millis()
                    );
                }
            }
        }
    }

    pub fn last_receipt_after_injection(&self) -> Option<Instant> {
        let injected = self.injected_at?;
        self.last_receipt.filter(|t| *t >= injected)
    }

    pub fn any_receipt_after_injection(&self) -> bool {
        self.last_receipt_after_injection().is_some()
    }
}

pub(crate) enum WaitDecision {
    KeepWaiting,
    /// Stop waiting; settle the transaction (auto-submit + guarded restore).
    Finish,
}

/// Pure decision: given the current transaction state, keep waiting for the
/// target to read, or finish now. Both platform event loops call this.
pub(crate) fn evaluate(state: &TxState, now: Instant) -> WaitDecision {
    if !state.injection_finished {
        return WaitDecision::KeepWaiting;
    }
    if state.ownership_lost || state.cancelled {
        return WaitDecision::Finish;
    }
    if state.settle_after.is_some_and(|after| now < after) {
        return WaitDecision::KeepWaiting;
    }
    if let Some(last) = state.last_receipt_after_injection() {
        if now.duration_since(last) >= QUIET_PERIOD {
            return WaitDecision::Finish;
        }
    }
    let deadline = if state.injection_failed {
        FAILED_INJECTION_TIMEOUT
    } else {
        RESTORE_TIMEOUT
    };
    if now.duration_since(state.published_at) >= deadline {
        return WaitDecision::Finish;
    }
    WaitDecision::KeepWaiting
}

/// Owns the single notification shared by normal and superseding settlement.
#[derive(Debug, Default)]
pub(crate) struct Completion {
    sender: Option<Sender<OutputSettlement>>,
}

impl Completion {
    pub fn channel() -> (Self, Receiver<OutputSettlement>) {
        let (sender, receiver) = std::sync::mpsc::channel();
        (
            Self {
                sender: Some(sender),
            },
            receiver,
        )
    }

    pub fn is_pending(&self) -> bool {
        self.sender.is_some()
    }

    pub fn settle(
        &mut self,
        result: OutputResult,
        finish: impl FnOnce() -> Result<(), OutputFailure>,
    ) {
        self.settle_with(|| match finish() {
            Ok(()) => result,
            Err(failure) => OutputResult::Failed(failure),
        });
    }

    pub fn settle_with(&mut self, finish: impl FnOnce() -> OutputResult) {
        let Some(sender) = self.sender.take() else {
            return;
        };
        let result = finish();
        let _ = sender.send(OutputSettlement {
            at: Instant::now(),
            kind: OutputKind::ClipboardPaste,
            result,
        });
    }
}

pub(crate) fn settlement_result(
    state: &TxState,
    still_ours: bool,
    superseded: bool,
) -> OutputResult {
    if state.injection_failed {
        OutputResult::Failed(OutputFailure::Injection)
    } else if state.delivery_failed {
        OutputResult::Failed(OutputFailure::Other)
    } else if state.ownership_lost || !still_ours {
        OutputResult::Unconfirmed(UnconfirmedReason::OwnershipLost)
    } else if superseded {
        OutputResult::Unconfirmed(UnconfirmedReason::Superseded)
    } else if state.any_receipt_after_injection() {
        OutputResult::Completed(CompletionBasis::ReceiptTransactionSettled)
    } else {
        OutputResult::Unconfirmed(UnconfirmedReason::Timeout)
    }
}

/// Modifier hold for the receipt-sequenced chord, kept at parity with the
/// legacy path (100ms) for the beta: that hold was added in #165 because real
/// users' systems dropped chords released too quickly, and the beta should
/// validate the receipt mechanism without changing a second variable.
///
/// Once receipts are proven in the field this becomes a safe tuning knob — a
/// chord the target never recognizes produces no receipt and is logged ("no
/// read within timeout") rather than failing silently, so a shorter hold
/// (measured working at 10ms on a fast machine, cutting visible latency from
/// ~110ms to ~20ms) can be tried as its own experiment later.
const CHORD_HOLD_MS: u64 = 100;

/// Sends the platform paste chord for the configured method.
pub(crate) fn send_chord(
    enigo: &mut enigo::Enigo,
    paste_method: &crate::settings::PasteMethod,
) -> Result<(), String> {
    use crate::settings::PasteMethod;
    match paste_method {
        PasteMethod::CtrlV => crate::input::send_paste_ctrl_v(enigo, CHORD_HOLD_MS),
        PasteMethod::CtrlShiftV => crate::input::send_paste_ctrl_shift_v(enigo, CHORD_HOLD_MS),
        PasteMethod::ShiftInsert => crate::input::send_paste_shift_insert(enigo, CHORD_HOLD_MS),
        other => Err(format!(
            "Invalid paste method for clipboard paste: {:?}",
            other
        )),
    }
}

/// Attempts the receipt-sequenced paste. Returns `Err` before anything has
/// been published when the platform transaction cannot start, in which case
/// the caller should fall back to the legacy paste path. On `Ok`, publishing
/// and chord injection have completed and the guarded restore (plus
/// auto-submit) finishes asynchronously.
#[cfg(any(target_os = "macos", target_os = "windows"))]
pub(crate) fn try_reliable_paste(
    text: &str,
    app_handle: &tauri::AppHandle,
    paste_method: &crate::settings::PasteMethod,
    enigo: &mut enigo::Enigo,
    options: PasteOptions,
) -> Result<OutputReceipt, String> {
    platform::run(text, app_handle, paste_method, enigo, options)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state_after_publish(published_ago: Duration) -> TxState {
        let mut s = TxState::new();
        s.published_at = Instant::now() - published_ago;
        s.injection_finished = true;
        s
    }

    #[test]
    fn cannot_settle_before_chord_injection_returns() {
        let state = TxState::new();
        assert!(matches!(
            evaluate(&state, Instant::now() + RESTORE_TIMEOUT),
            WaitDecision::KeepWaiting
        ));
    }

    #[test]
    fn confirmed_settlement_honors_configured_after_delay() {
        let mut state = state_after_publish(Duration::from_millis(500));
        let now = Instant::now();
        state.injected_at = Some(state.published_at);
        state.record_receipt(now - QUIET_PERIOD);
        state.settle_after = Some(now + Duration::from_millis(100));
        assert!(matches!(evaluate(&state, now), WaitDecision::KeepWaiting));
        assert!(matches!(
            evaluate(&state, state.settle_after.unwrap()),
            WaitDecision::Finish
        ));
    }

    #[test]
    fn receipt_arrives_only_after_followup_work() {
        let (mut completion, receiver) = Completion::channel();
        assert!(matches!(
            receiver.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Empty)
        ));
        let finished = std::cell::Cell::new(None);
        completion.settle(
            OutputResult::Completed(CompletionBasis::ReceiptTransactionSettled),
            || {
                assert!(matches!(
                    receiver.try_recv(),
                    Err(std::sync::mpsc::TryRecvError::Empty)
                ));
                finished.set(Some(Instant::now()));
                Ok(())
            },
        );
        let settlement = receiver.recv().unwrap();
        assert!(settlement.at >= finished.get().unwrap());
        assert!(matches!(
            settlement.result,
            OutputResult::Completed(CompletionBasis::ReceiptTransactionSettled)
        ));
    }

    #[test]
    fn competing_settlement_runs_followup_exactly_once() {
        let (mut completion, receiver) = Completion::channel();
        let calls = std::cell::Cell::new(0);
        completion.settle(
            OutputResult::Unconfirmed(UnconfirmedReason::Superseded),
            || {
                calls.set(calls.get() + 1);
                Ok(())
            },
        );
        completion.settle(
            OutputResult::Completed(CompletionBasis::ReceiptTransactionSettled),
            || {
                calls.set(calls.get() + 1);
                Ok(())
            },
        );
        assert_eq!(calls.get(), 1);
        assert!(matches!(
            receiver.recv().unwrap().result,
            OutputResult::Unconfirmed(UnconfirmedReason::Superseded)
        ));
        assert!(receiver.try_recv().is_err());
    }

    #[test]
    fn independent_transactions_can_finish_out_of_order() {
        let (mut first, first_receiver) = Completion::channel();
        let (mut second, second_receiver) = Completion::channel();
        second.settle(
            OutputResult::Unconfirmed(UnconfirmedReason::Timeout),
            || Ok(()),
        );
        assert!(second_receiver.try_recv().is_ok());
        assert!(matches!(
            first_receiver.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Empty)
        ));
        first.settle(
            OutputResult::Unconfirmed(UnconfirmedReason::Superseded),
            || Ok(()),
        );
        assert!(matches!(
            first_receiver.recv().unwrap().result,
            OutputResult::Unconfirmed(UnconfirmedReason::Superseded)
        ));
    }

    #[test]
    fn followup_failures_prevent_successful_completion() {
        for failure in [OutputFailure::Restore, OutputFailure::AutoSubmit] {
            let (mut completion, receiver) = Completion::channel();
            completion.settle(
                OutputResult::Completed(CompletionBasis::ReceiptTransactionSettled),
                || Err(failure),
            );
            assert!(matches!(
                receiver.recv().unwrap().result,
                OutputResult::Failed(OutputFailure::Restore | OutputFailure::AutoSubmit)
            ));
        }
    }

    #[test]
    fn settlement_keeps_unconfirmed_reasons_distinct() {
        let mut state = state_after_publish(RESTORE_TIMEOUT);
        assert!(matches!(
            settlement_result(&state, true, false),
            OutputResult::Unconfirmed(UnconfirmedReason::Timeout)
        ));
        assert!(matches!(
            settlement_result(&state, true, true),
            OutputResult::Unconfirmed(UnconfirmedReason::Superseded)
        ));
        assert!(matches!(
            settlement_result(&state, false, false),
            OutputResult::Unconfirmed(UnconfirmedReason::OwnershipLost)
        ));
        state.injection_failed = true;
        assert!(matches!(
            settlement_result(&state, true, false),
            OutputResult::Failed(OutputFailure::Injection)
        ));
        state.injection_failed = false;
        state.delivery_failed = true;
        assert!(matches!(
            settlement_result(&state, true, false),
            OutputResult::Failed(OutputFailure::Other)
        ));
    }

    #[test]
    fn read_receipt_is_required_for_confirmed_settlement() {
        let mut state = state_after_publish(Duration::from_millis(500));
        state.injected_at = Some(state.published_at);
        state.record_receipt(Instant::now() - QUIET_PERIOD);
        assert!(matches!(
            settlement_result(&state, true, false),
            OutputResult::Completed(CompletionBasis::ReceiptTransactionSettled)
        ));
        state.ownership_lost = true;
        assert!(matches!(
            settlement_result(&state, true, false),
            OutputResult::Unconfirmed(UnconfirmedReason::OwnershipLost)
        ));
    }

    #[test]
    fn keeps_waiting_without_receipt_within_timeout() {
        let s = state_after_publish(Duration::from_millis(100));
        assert!(matches!(
            evaluate(&s, Instant::now()),
            WaitDecision::KeepWaiting
        ));
    }

    #[test]
    fn finishes_after_quiet_period_once_read() {
        let mut s = state_after_publish(Duration::from_millis(300));
        s.injected_at = Some(Instant::now() - Duration::from_millis(250));
        s.last_receipt = Some(Instant::now() - QUIET_PERIOD);
        assert!(matches!(evaluate(&s, Instant::now()), WaitDecision::Finish));
    }

    #[test]
    fn waits_through_quiet_period_after_recent_read() {
        let mut s = state_after_publish(Duration::from_millis(300));
        s.injected_at = Some(Instant::now() - Duration::from_millis(100));
        s.last_receipt = Some(Instant::now() - Duration::from_millis(50));
        assert!(matches!(
            evaluate(&s, Instant::now()),
            WaitDecision::KeepWaiting
        ));
    }

    #[test]
    fn pre_injection_receipt_does_not_count() {
        let mut s = state_after_publish(Duration::from_millis(300));
        s.last_receipt = Some(Instant::now() - Duration::from_millis(200));
        s.injected_at = Some(Instant::now() - Duration::from_millis(100));
        assert!(!s.any_receipt_after_injection());
        assert!(matches!(
            evaluate(&s, Instant::now()),
            WaitDecision::KeepWaiting
        ));
    }

    #[test]
    fn finishes_on_timeout_without_receipt() {
        let s = state_after_publish(RESTORE_TIMEOUT);
        assert!(matches!(evaluate(&s, Instant::now()), WaitDecision::Finish));
    }

    #[test]
    fn failed_injection_uses_short_timeout() {
        let mut s = state_after_publish(FAILED_INJECTION_TIMEOUT);
        s.injection_failed = true;
        assert!(matches!(evaluate(&s, Instant::now()), WaitDecision::Finish));
    }

    #[test]
    fn ownership_loss_finishes_immediately() {
        let mut s = state_after_publish(Duration::from_millis(10));
        s.ownership_lost = true;
        assert!(matches!(evaluate(&s, Instant::now()), WaitDecision::Finish));
    }
}
