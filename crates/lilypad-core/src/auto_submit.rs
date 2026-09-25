//! Decides whether a finished live/session game is auto-submitted or held back so the user can
//! add notes.
//!
//! The rule that matters: **LilyPad owns the clock, not the notification.** Earlier versions
//! waited for the Windows toast to report dismissal, which made the notification system's health
//! decide whether a real session was submitted — a machine with the notification component
//! missing, or Focus Assist enabled, filed perfectly good sessions onto the pending queue as
//! failures. ("Unattended mode" existed only to route around that, and is gone with it.)
//!
//! So the toast is presentation. It can bring the answer in early, by being clicked or
//! dismissed, but it can never withhold one: when the window elapses, the session submits.

use std::future::Future;
use std::time::Duration;
use tokio::sync::oneshot;

/// How long a finished live/session game waits before auto-submitting, giving the user a chance
/// to intercept it and add notes.
pub const INTERCEPT_WINDOW: Duration = Duration::from_secs(25);

#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    Submit,
    AddNotes,
}

/// Resolves when the user asks to add notes, when the toast is dismissed, or when `window`
/// elapses — whichever comes first.
///
/// Both receivers are matched on `Ok(())` rather than bare completion. A oneshot whose sender is
/// dropped completes immediately with an error, and `tokio::select!` would treat that as the arm
/// firing: a toast that failed to be created drops both senders, so a bare `_ =` pattern would
/// resolve instantly and silently. Matching `Ok(())` disables those arms instead, leaving the
/// timer to answer — which is exactly the intended behaviour when there is no working toast.
pub async fn wait_for_outcome(
    add_notes: oneshot::Receiver<()>,
    dismissed: oneshot::Receiver<()>,
    window: Duration,
) -> Outcome {
    tokio::select! {
        biased;
        // Checked first so a toast that is dismissed *by* clicking Add Notes cannot race its
        // own dismissal signal and submit the session the user was trying to annotate.
        Ok(()) = add_notes => Outcome::AddNotes,
        Ok(()) = dismissed => Outcome::Submit,
        _ = tokio::time::sleep(window) => Outcome::Submit,
    }
}

/// Drives a frontend notification adapter only while an interception is pending. Even a
/// presentation future that never returns (including notification creation) cannot stop the
/// timer. Completing/dropping presentation without an explicit action is not a dismissal.
pub async fn wait_with_presentation(
    add_notes: oneshot::Receiver<()>,
    dismissed: oneshot::Receiver<()>,
    window: Duration,
    presentation: impl Future<Output = ()>,
) -> Outcome {
    let decision = wait_for_outcome(add_notes, dismissed, window);
    tokio::pin!(decision);
    tokio::select! {
        biased;
        // Poll actions first so an action ready at the deadline can reach the decision.
        () = presentation => decision.await,
        outcome = &mut decision => outcome,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const WINDOW: Duration = Duration::from_millis(50);

    /// The regression this module exists for: no toast at all must still submit.
    #[tokio::test(start_paused = true)]
    async fn a_notification_that_never_appears_still_submits_when_the_window_elapses() {
        let (add_notes_tx, add_notes) = oneshot::channel();
        let (dismissed_tx, dismissed) = oneshot::channel();
        // Both senders dropped: how a machine with no working notification component looks.
        drop(add_notes_tx);
        drop(dismissed_tx);
        assert_eq!(
            wait_for_outcome(add_notes, dismissed, WINDOW).await,
            Outcome::Submit
        );
    }

    /// ...and must take the whole window to do it, rather than resolving instantly off the
    /// dropped senders. Submitting early is harmless here, but it would mean the timer was not
    /// actually in control, which is the property this design depends on.
    #[tokio::test(start_paused = true)]
    async fn a_failed_notification_does_not_short_circuit_the_window() {
        let (add_notes_tx, add_notes) = oneshot::channel();
        let (dismissed_tx, dismissed) = oneshot::channel();
        drop(add_notes_tx);
        drop(dismissed_tx);
        let started = tokio::time::Instant::now();
        wait_for_outcome(add_notes, dismissed, WINDOW).await;
        assert!(started.elapsed() >= WINDOW);
    }

    #[tokio::test(start_paused = true)]
    async fn add_notes_holds_the_session_back() {
        let (add_notes_tx, add_notes) = oneshot::channel();
        let (_dismissed_tx, dismissed) = oneshot::channel();
        add_notes_tx.send(()).unwrap();
        assert_eq!(
            wait_for_outcome(add_notes, dismissed, WINDOW).await,
            Outcome::AddNotes
        );
    }

    /// Clicking Add Notes also dismisses the toast, so both signals can arrive together.
    /// Add Notes must win, or the session the user wanted to annotate is submitted without notes.
    #[tokio::test(start_paused = true)]
    async fn add_notes_wins_when_it_arrives_alongside_dismissal() {
        let (add_notes_tx, add_notes) = oneshot::channel();
        let (dismissed_tx, dismissed) = oneshot::channel();
        add_notes_tx.send(()).unwrap();
        dismissed_tx.send(()).unwrap();
        assert_eq!(
            wait_for_outcome(add_notes, dismissed, WINDOW).await,
            Outcome::AddNotes
        );
    }

    /// Dismissal is an early answer, not a required one -- the user should not wait out the
    /// window after swiping the toast away.
    #[tokio::test(start_paused = true)]
    async fn dismissal_submits_without_waiting_out_the_window() {
        let (_add_notes_tx, add_notes) = oneshot::channel();
        let (dismissed_tx, dismissed) = oneshot::channel();
        dismissed_tx.send(()).unwrap();
        let started = tokio::time::Instant::now();
        assert_eq!(
            wait_for_outcome(add_notes, dismissed, WINDOW).await,
            Outcome::Submit
        );
        assert!(started.elapsed() < WINDOW);
    }

    #[tokio::test(start_paused = true)]
    async fn a_stalled_presentation_is_dropped_at_the_deadline() {
        let (add_tx, add) = oneshot::channel();
        let (dismiss_tx, dismiss) = oneshot::channel();
        let presentation = async move {
            let _senders = (add_tx, dismiss_tx);
            std::future::pending::<()>().await;
        };
        let start = tokio::time::Instant::now();
        assert_eq!(
            wait_with_presentation(add, dismiss, WINDOW, presentation).await,
            Outcome::Submit
        );
        assert_eq!(start.elapsed(), WINDOW);
    }

    #[tokio::test(start_paused = true)]
    async fn presentation_failure_still_leaves_the_full_interception_window() {
        let (add_tx, add) = oneshot::channel();
        let (dismiss_tx, dismiss) = oneshot::channel();
        let start = tokio::time::Instant::now();
        let failed = async move {
            drop((add_tx, dismiss_tx));
        };
        assert_eq!(
            wait_with_presentation(add, dismiss, WINDOW, failed).await,
            Outcome::Submit
        );
        assert_eq!(start.elapsed(), WINDOW);
    }

    #[tokio::test(start_paused = true)]
    async fn presentation_can_intercept_without_waiting_for_timeout() {
        let (add_tx, add) = oneshot::channel();
        let (dismiss_tx, dismiss) = oneshot::channel();
        let action = async move {
            add_tx.send(()).unwrap();
            dismiss_tx.send(()).unwrap();
        };
        assert_eq!(
            wait_with_presentation(add, dismiss, WINDOW, action).await,
            Outcome::AddNotes
        );
    }
}
