use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crossterm::ExecutableCommand;
use crossterm::cursor::Hide;
use crossterm::event::{EnableBracketedPaste, EnableMouseCapture};
use crossterm::terminal::{self, Clear, ClearType, EnterAlternateScreen};

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
        // Signal the background event-reader thread to exit. It picks
        // this up at the next `event::poll` tick (50ms) and releases
        // crossterm's internal mutex. Wait briefly for the reader to
        // actually quiesce — otherwise it races with our CPR sync
        // for stdin bytes (crossterm's parser silently consumes the
        // reply, our libc::read times out waiting for it).
        EVENT_READER_SHUTDOWN.store(true, Ordering::Relaxed);
        std::thread::sleep(Duration::from_millis(60));
        let mut stdout = std::io::stdout();

        // === Phase 1: tell the terminal to stop reporting things ===
        // Explicit DECRST for every mode we might have touched.
        // Order matters less here than completeness — any mode left
        // on can trigger unsolicited reports later (focus events,
        // mouse motion, paste sentinels, modify-other-keys).
        //   ?1000  — X10 mouse
        //   ?1002  — cell motion mouse
        //   ?1003  — all-motion mouse
        //   ?1004  — focus in/out events
        //   ?1006  — SGR-encoded mouse
        //   ?1015  — urxvt mouse
        //   ?2004  — bracketed paste
        //   ?1049  — alternate screen (LeaveAlternateScreen)
        // Plus SGR reset (`\x1b[0m`) and cursor-show (`\x1b[?25h`).
        let _ = stdout.write_all(
            b"\x1b[0m\
              \x1b[?25h\
              \x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1004l\x1b[?1006l\x1b[?1015l\
              \x1b[?2004l\
              \x1b[?1049l",
        );
        let _ = stdout.flush();

        // === Phase 2: synchronization sentinel ===
        // Some terminals (iTerm2 in particular) reply to alt-screen
        // exit with a flurry of unsolicited reports: OSC 11 bg-color
        // (`\x1b]11;rgb:…`), primary DA (`\x1b[?64;…c`), cursor
        // position (`\x1b[…R`). Drain-by-time is fragile because the
        // round-trip is unbounded (SSH, tmux nesting, slow VT) and
        // anything that arrives AFTER raw mode is disabled will be
        // re-interpreted by the shell's line discipline / readline
        // and become visible garbage at the prompt.
        //
        // Solution: SEND OUR OWN cursor-position query (DSR-CPR,
        // `\x1b[6n`). Terminals process queries in FIFO order, so
        // when we see our own CPR reply (`\x1b[<row>;<col>R`) on
        // stdin, every earlier reply (including the unsolicited
        // alt-screen-exit chatter) has also been delivered. Read
        // stdin until we see ANY `R`-terminated CSI; discard
        // everything along the way. Bounded timeout as a fallback
        // for very-slow / non-responsive terminals (raw write to
        // /dev/null or similar).
        #[cfg(unix)]
        sync_and_drain_via_cpr(&mut stdout, Duration::from_millis(500));

        // === Phase 3: tear down raw mode ===
        // By here the synchronization sentinel has fired and the
        // stdin buffer is empty. Disable raw mode and exit.
        let _ = terminal::disable_raw_mode();
        // Final cursor-show in cooked mode in case the shell's prompt
        // theme depended on it being visible.
        let _ = stdout.write_all(b"\x1b[?25h");
        let _ = stdout.flush();
    }
}

/// Send a CPR query (`\x1b[6n`) and read stdin until the terminal's
/// reply (`\x1b[<row>;<col>R`) appears, discarding every byte along
/// the way. Terminals process queries in FIFO order, so seeing our
/// CPR reply guarantees every PRIOR unsolicited reply
/// (alt-screen-exit chatter from iTerm2 / kitty / foot, OSC 11
/// bg-color, primary DA) has already been drained. This is the
/// xterm "sync" trick — used by vim, neovim, and most modern TUIs.
///
/// Bounded by `budget` as a fallback for terminals that don't reply
/// at all (rare; mostly headless / pipe contexts where the guard
/// shouldn't be active anyway).
#[cfg(unix)]
fn sync_and_drain_via_cpr(stdout: &mut std::io::Stdout, budget: Duration) {
    let fd_in: libc::c_int = 0; // stdin

    // Save the current stdin flags so we can restore blocking
    // semantics for the shell when we're done.
    let original_flags = unsafe { libc::fcntl(fd_in, libc::F_GETFL) };
    if original_flags < 0 {
        return;
    }
    let nb_flags = original_flags | libc::O_NONBLOCK;
    if unsafe { libc::fcntl(fd_in, libc::F_SETFL, nb_flags) } < 0 {
        return;
    }

    // Emit the sentinel query. If write fails (broken pipe, e.g.
    // stdout redirected to a file), bail — we can't sync.
    if stdout.write_all(b"\x1b[6n").is_err() {
        let _ = unsafe { libc::fcntl(fd_in, libc::F_SETFL, original_flags) };
        return;
    }
    let _ = stdout.flush();

    let deadline = std::time::Instant::now() + budget;
    let mut buf = [0u8; 1024];
    // State machine: scan accumulated bytes for any `\x1b[…R`
    // sequence. CPR is the only `R`-terminated CSI that comes back
    // unsolicited at this stage; we're not strict about row/col
    // values, just looking for the terminator.
    let mut in_csi = false;
    let mut got_cpr = false;
    while !got_cpr && std::time::Instant::now() < deadline {
        let n = unsafe { libc::read(fd_in, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
        if n > 0 {
            // Scan the freshly-read chunk for `\x1b[…R`. Carries
            // state across reads via `in_csi` so a CSI split
            // across read boundaries still triggers correctly.
            for &b in &buf[..n as usize] {
                if !in_csi {
                    if b == 0x1b {
                        // Start of an escape; the next byte should
                        // be `[` for CSI, but we don't gate on
                        // it — the scanner is tolerant to OSC
                        // (`\x1b]…`) and SS3 (`\x1bO…`) by simply
                        // not matching `R` until they're consumed.
                        in_csi = true;
                    }
                } else if b == b'R' {
                    got_cpr = true;
                    break;
                } else if b == 0x1b {
                    // New escape started without the previous one
                    // closing — could happen on garbage input.
                    // Reset and continue.
                    in_csi = true;
                }
            }
            continue;
        }
        if n == 0 {
            // EOF — stdin closed. Nothing more to drain.
            break;
        }
        let err = std::io::Error::last_os_error().raw_os_error();
        match err {
            Some(e) if e == libc::EAGAIN || e == libc::EWOULDBLOCK => {
                // Nothing pending right now; sleep briefly and
                // poll again. 4ms is small enough that even fast
                // terminals exit within ~8-12ms total (CPR
                // round-trip + slack); a slow SSH link gets the
                // full 500ms budget.
                std::thread::sleep(Duration::from_millis(4));
            }
            Some(libc::EINTR) => continue,
            _ => break,
        }
    }

    // Restore blocking semantics for the shell.
    let _ = unsafe { libc::fcntl(fd_in, libc::F_SETFL, original_flags) };
}
