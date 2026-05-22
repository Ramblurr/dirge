use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crossterm::ExecutableCommand;
use crossterm::cursor::{Hide, Show};
use crossterm::event::{
    self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
};
use crossterm::terminal::{self, Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen};

/// Shared shutdown signal between the input-reader background thread
/// in `ui::mod` and `TerminalGuard::drop`. The reader polls this with
/// each `event::poll` tick; the guard sets it before tearing down so
/// the reader exits its loop cooperatively instead of dying mid-read
/// when the process unwinds. Without this flag the reader stays
/// blocked in `event::read()` while the guard's drain pass is also
/// holding crossterm's internal mutex — the two race for terminal-
/// response bytes (OSC 11, primary DA, CPR). Either path consumes
/// them, but the race is real and the outcome is timing-dependent.
pub(crate) static EVENT_READER_SHUTDOWN: AtomicBool = AtomicBool::new(false);

pub struct TerminalGuard;

impl TerminalGuard {
    pub fn new() -> std::io::Result<Self> {
        // Reset the shutdown flag in case the binary previously held a
        // guard in the same process (test harness, embedded use).
        EVENT_READER_SHUTDOWN.store(false, Ordering::Relaxed);
        let mut stdout = std::io::stdout();
        stdout.execute(EnterAlternateScreen)?;
        stdout.execute(Clear(ClearType::All))?;
        stdout.execute(EnableMouseCapture)?;
        // Bracketed paste lets the terminal deliver a multi-line paste as a
        // single Event::Paste, rather than a flood of keystroke events. The
        // input editor relies on this to compress long pastes into a
        // `[N lines pasted]` placeholder.
        stdout.execute(EnableBracketedPaste)?;
        // Hide the hardware cursor by default. While the agent streams output,
        // the renderer issues many MoveTo calls and the visible cursor would
        // flicker across the screen. draw_bottom re-shows it only after
        // positioning it at the input prompt.
        stdout.execute(Hide)?;
        terminal::enable_raw_mode()?;
        Ok(TerminalGuard)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        // Signal the background event-reader thread to exit its loop.
        // It picks this up at the next `event::poll` tick (up to ~50ms),
        // breaks out of its outer loop, and releases crossterm's
        // internal mutex so our drain below can run without contention.
        EVENT_READER_SHUTDOWN.store(true, Ordering::Relaxed);
        let mut stdout = std::io::stdout();
        // The shutdown order matters. Each escape-emitting transition
        // — DisableMouseCapture, DisableBracketedPaste, and especially
        // LeaveAlternateScreen — provokes some terminals (iTerm2,
        // tmux state machines, foot, kitty) to reply with synchronous
        // status bytes: OSC 11 bg-color (`\x1b]11;rgb:…\x1b\\`),
        // primary DA (`\x1b[?64;…c`), and cursor-position reports
        // (`\x1b[…R`). If raw mode is already off when those bytes
        // arrive on stdin, the TTY line discipline echoes them
        // straight to the user's shell prompt as visible garbage.
        //
        // The fix is to keep raw mode on past every escape-emitting
        // transition AND drain after each, then finally disable raw
        // mode last. Previous ordering disabled raw mode BEFORE
        // leaving the alt screen, so the alt-screen-exit's responses
        // always leaked.
        let _ = stdout.execute(Show);
        let _ = stdout.execute(DisableBracketedPaste);
        let _ = stdout.execute(DisableMouseCapture);
        let _ = stdout.flush();
        // Drain pass 1: catches responses to the three mode-resets
        // above. Start with a long first poll (80ms) to cover the
        // background reader's worst-case 50ms poll latency, then
        // short polls until deadline. Total budget here is ~150ms
        // — slow links (SSH-over-VPN, tmux-in-tmux) need more than
        // the previous 80ms window.
        drain_events(Duration::from_millis(150));
        // NOW leave the alt screen while still in raw mode. Some
        // terminals only emit the bg-color OSC 11 response on this
        // specific transition; leaving alt screen after `disable_raw`
        // was the original leak.
        let _ = stdout.execute(LeaveAlternateScreen);
        let _ = stdout.flush();
        // Drain pass 2: catches responses to LeaveAlternateScreen.
        drain_events(Duration::from_millis(100));
        // Raw mode last — by now everything the terminal would
        // unsolicit has been parsed and discarded.
        let _ = terminal::disable_raw_mode();
        let _ = stdout.flush();
    }
}

/// Drain pending terminal events from crossterm's queue until either
/// nothing is pending or the budget expires. Uses an initial longer
/// poll (covers the background reader's poll latency) followed by
/// short polls. Errors are swallowed — drain is best-effort and the
/// process is exiting either way.
fn drain_events(budget: Duration) {
    let deadline = std::time::Instant::now() + budget;
    let mut first = true;
    loop {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        let wait = if first {
            // First poll absorbs the background reader's worst-case
            // 50ms poll tick + a margin for the terminal round-trip.
            remaining.min(Duration::from_millis(80))
        } else {
            remaining.min(Duration::from_millis(5))
        };
        first = false;
        match event::poll(wait) {
            Ok(true) => {
                if event::read().is_err() {
                    break;
                }
            }
            // Quiet for one poll cycle — assume the terminal is done
            // talking. Break instead of spinning to deadline so a
            // fast-responding terminal exits promptly.
            Ok(false) => break,
            Err(_) => break,
        }
    }
}
