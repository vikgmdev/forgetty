//! P7.1: a pane that runs a real shell reports prompt start, command start and
//! command end with the exit code, the directory and the tty mode (echo off
//! during a password-style read). One test per shell; a shell that is not
//! installed is skipped (fish: `FORGETTY_TEST_FISH` names a binary called
//! `fish` when it is not in PATH).

#![cfg(target_os = "linux")]

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use forgetty_core::PaneId;
use forgetty_pty::PtySize;
use forgetty_session::{Phase, SessionEvent, SessionManager, ShellIntegration, ShellSnapshot};
use tokio::sync::broadcast::Receiver;

/// The tests set HOME and friends for the shells they start: one at a time.
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn find_shell(name: &str) -> Option<PathBuf> {
    if name == "fish" {
        if let Some(p) = std::env::var_os("FORGETTY_TEST_FISH") {
            return Some(PathBuf::from(p));
        }
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).map(|d| d.join(name)).find(|p| p.is_file())
}

/// Every `ShellState` of one pane so far, in order.
struct Seen {
    rx: Receiver<SessionEvent>,
    pane: PaneId,
    states: Vec<ShellSnapshot>,
}

impl Seen {
    fn drain(&mut self) {
        while let Ok(ev) = self.rx.try_recv() {
            if let SessionEvent::ShellState { pane_id, state } = ev {
                if pane_id == self.pane {
                    self.states.push(state);
                }
            }
        }
    }

    /// Wait until `pred` is true for some state that came after `from`.
    /// Returns the index of that state.
    async fn wait(
        &mut self,
        what: &str,
        from: usize,
        mut pred: impl FnMut(&ShellSnapshot) -> bool,
    ) -> usize {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            self.drain();
            if let Some(i) = (from..self.states.len()).find(|&i| pred(&self.states[i])) {
                return i;
            }
            if Instant::now() > deadline {
                let log: Vec<String> = self
                    .states
                    .iter()
                    .map(|s| {
                        format!(
                            "rev {} {:?} mark {:?} code {:?} cwd {:?} tty {:?}",
                            s.rev, s.phase, s.mark, s.exit_code, s.cwd, s.tty
                        )
                    })
                    .collect();
                panic!("timed out waiting for {what}; states:\n{}", log.join("\n"));
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
}

struct Env {
    home: tempfile::TempDir,
    work: PathBuf,
}

/// A home with empty rc files, so the user's own never load.
fn quiet_home() -> Env {
    let home = tempfile::tempdir().unwrap();
    for f in [".zshrc", ".zshenv", ".zprofile", ".bashrc", ".bash_profile", ".profile"] {
        std::fs::write(home.path().join(f), "").unwrap();
    }
    // bash and zsh both show "$ " style prompts of their own.
    let work = home.path().join("work dir").canonicalize().unwrap_or_else(|_| {
        std::fs::create_dir_all(home.path().join("work dir")).unwrap();
        home.path().join("work dir").canonicalize().unwrap()
    });
    Env { home, work }
}

fn set_env(home: &Path) {
    // SAFETY: tests of this file run one after the other (a single #[test]
    // runs every shell), so nothing else reads the environment meanwhile.
    unsafe {
        std::env::set_var("HOME", home);
        std::env::remove_var("ZDOTDIR");
        std::env::remove_var("PROMPT_COMMAND");
        std::env::remove_var("XDG_CONFIG_HOME");
        std::env::set_var("XDG_CONFIG_HOME", home.join(".config"));
        std::env::set_var("TERM", "xterm-256color");
    }
}

fn spawn_pane(sm: &SessionManager, shell: &Path, cwd: &Path) -> PaneId {
    let size = PtySize { rows: 24, cols: 100, pixel_width: 0, pixel_height: 0 };
    sm.create_pane(size, Some(cwd.to_path_buf()), None, Some(shell.display().to_string()), true)
        .expect("create_pane")
}

async fn run_shell(name: &str) {
    let Some(shell) = find_shell(name) else {
        println!("SKIPPED: {name} is not installed");
        return;
    };
    let env = quiet_home();
    set_env(env.home.path());
    let rcs: Vec<(PathBuf, String)> = [".zshrc", ".bashrc", ".bash_profile", ".zshenv"]
        .iter()
        .map(|f| (env.home.path().join(f), String::new()))
        .collect();

    let sm = SessionManager::new();
    let si_dir = env.home.path().join("si");
    sm.set_shell_integration(Some(ShellIntegration::install(&si_dir).unwrap()));
    let rx = sm.subscribe_meta();
    let pane = spawn_pane(&sm, &shell, &env.work);
    let mut seen = Seen { rx, pane, states: Vec::new() };

    // The first prompt: A, then B, with the directory.
    let i = seen.wait("the first prompt (B)", 0, |s| s.phase == Phase::Input).await;
    seen.wait("A before B", 0, |s| s.phase == Phase::Prompt).await;
    let cwd_ok = seen.states.iter().any(|s| s.cwd.as_deref() == env.work.to_str());
    assert!(cwd_ok, "{name}: no state carries the directory {:?}", env.work);
    // The line editor edits the line itself: the kernel does not echo.
    let tty = seen
        .wait("the line editor tty mode", i, |s| s.tty.is_some_and(|t| !t.canonical && !t.echo))
        .await;

    // A command that fails: C, then D with its exit code, then the next prompt.
    sm.write_pty(pane, b"sh -c 'exit 3'\n").unwrap();
    let c = seen.wait("command start (C)", tty, |s| s.phase == Phase::Running).await;
    let d = seen
        .wait("command end (D) with code 3", c, |s| {
            s.phase == Phase::Done && s.exit_code == Some(3)
        })
        .await;
    let a2 = seen.wait("the next prompt (A)", d, |s| s.phase == Phase::Prompt).await;
    let b2 = seen.wait("the next prompt ends (B)", a2, |s| s.phase == Phase::Input).await;

    // A command that works: the code is 0.
    sm.write_pty(pane, b"true\n").unwrap();
    seen.wait("exit code 0", b2, |s| s.phase == Phase::Done && s.exit_code == Some(0)).await;
    let b3 = seen.wait("prompt again", b2, |s| s.phase == Phase::Input && s.rev > 0).await;
    let b3 =
        seen.states[b3..].iter().position(|s| s.phase == Phase::Input).map(|p| b3 + p).unwrap();

    // A password-style read: canonical, echo off, while the command runs.
    sm.write_pty(pane, b"sh -c 'stty -echo; read -r x; stty echo'\n").unwrap();
    let masked = seen
        .wait("echo off during the read", b3, |s| {
            s.phase == Phase::Running && s.tty.is_some_and(|t| t.canonical && !t.echo)
        })
        .await;
    sm.write_pty(pane, b"hunter2\n").unwrap();
    // The read ends, the line editor has the terminal again.
    seen.wait("the line editor again after the read", masked, |s| {
        s.phase == Phase::Input && s.tty.is_some_and(|t| !t.canonical && !t.echo)
    })
    .await;

    // The on-demand read agrees with the events.
    let now = sm.shell_state(pane).unwrap();
    assert_eq!(now.phase, Phase::Input);
    assert!(now.rev >= seen.states.last().unwrap().rev);
    assert_eq!(now.exit_code, Some(0));

    // The rc files are untouched.
    for (path, content) in rcs {
        assert_eq!(std::fs::read_to_string(&path).unwrap(), content, "{name}: {path:?} was edited");
    }
    sm.close_pane(pane).unwrap();
}

// One test body for the three shells: they share the process environment.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn each_installed_shell_reports_marks_exit_code_cwd_and_echo_off() {
    let _one = SERIAL.lock().await;
    for name in ["zsh", "bash", "fish"] {
        println!("== {name}");
        run_shell(name).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn without_the_integration_a_shell_reports_no_marks_but_the_tty_mode() {
    let _one = SERIAL.lock().await;
    let Some(shell) = find_shell("bash") else {
        println!("SKIPPED: bash is not installed");
        return;
    };
    let env = quiet_home();
    set_env(env.home.path());
    let sm = SessionManager::new();
    let rx = sm.subscribe_meta();
    let pane = spawn_pane(&sm, &shell, &env.work);
    let mut seen = Seen { rx, pane, states: Vec::new() };
    seen.wait("the line editor tty mode", 0, |s| s.tty.is_some_and(|t| !t.canonical && !t.echo))
        .await;
    sm.write_pty(pane, b"true\n").unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;
    seen.drain();
    assert!(seen.states.iter().all(|s| s.phase == Phase::Unknown && s.mark.is_none()));
    sm.close_pane(pane).unwrap();
}

/// The termios is read after the output settles, not per chunk.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_tty_is_not_read_per_chunk_of_output() {
    let _one = SERIAL.lock().await;
    let Some(shell) = find_shell("bash") else {
        println!("SKIPPED: bash is not installed");
        return;
    };
    let env = quiet_home();
    set_env(env.home.path());
    let sm = SessionManager::new();
    let pane = spawn_pane(&sm, &shell, &env.work);
    tokio::time::sleep(Duration::from_millis(500)).await;
    let before = sm.shell_tty_reads(pane).unwrap();
    let t0 = Instant::now();
    sm.write_pty(pane, b"head -c 64000000 /dev/zero | tr '\\0' x; echo FLOOD-$((1+1))\n").unwrap();
    // Wait for the end of the flood through the output stream.
    let mut out = sm.subscribe_output();
    let mut chunks = 0u64;
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        assert!(Instant::now() < deadline, "flood did not finish");
        if let Ok(Ok(SessionEvent::PtyOutput { pane_id, data })) =
            tokio::time::timeout(Duration::from_millis(100), out.recv()).await
        {
            if pane_id == pane {
                chunks += 1;
                if data.windows(7).any(|w| w == b"FLOOD-2") {
                    break;
                }
            }
        }
    }
    let reads = sm.shell_tty_reads(pane).unwrap() - before;
    let secs = t0.elapsed().as_secs_f64();
    println!("{chunks} chunks in {secs:.2} s, {reads} termios reads");
    assert!(chunks > 100, "the flood made only {chunks} chunks");
    // One read per settle interval at most, plus the ones around the edges.
    let budget = (secs / 0.060) as u64 + 4;
    assert!(reads <= budget, "{reads} reads for {chunks} chunks in {secs:.2} s (budget {budget})");
    assert!(reads * 4 < chunks, "reads follow chunks: {reads} reads, {chunks} chunks");
    sm.close_pane(pane).unwrap();
}

/// New tabs and splits take the user's shell (`$SHELL`) through the same
/// integration, and they build while the session lock is held.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tabs_and_splits_start_their_shell_with_the_integration() {
    let _one = SERIAL.lock().await;
    let Some(shell) = find_shell("bash") else {
        println!("SKIPPED: bash is not installed");
        return;
    };
    let env = quiet_home();
    set_env(env.home.path());
    // SAFETY: serialized by SERIAL.
    unsafe { std::env::set_var("SHELL", &shell) };
    let sm = SessionManager::new();
    sm.set_shell_integration(Some(ShellIntegration::install(&env.home.path().join("si")).unwrap()));
    let size = PtySize { rows: 24, cols: 100, pixel_width: 0, pixel_height: 0 };
    let rx = sm.subscribe_meta();
    let (pane, _tab) = {
        let sm = sm.clone();
        let work = env.work.clone();
        tokio::time::timeout(
            Duration::from_secs(10),
            tokio::task::spawn_blocking(move || sm.create_tab(0, Some(work), size, None).unwrap()),
        )
        .await
        .expect("create_tab hangs")
        .unwrap()
    };
    let mut seen = Seen { rx, pane, states: Vec::new() };
    seen.wait("the tab's first prompt", 0, |s| s.phase == Phase::Input).await;
    let rx = sm.subscribe_meta();
    let child = {
        let sm = sm.clone();
        let work = env.work.clone();
        tokio::time::timeout(
            Duration::from_secs(10),
            tokio::task::spawn_blocking(move || {
                sm.split_pane(pane, "horizontal", size, Some(work)).unwrap()
            }),
        )
        .await
        .expect("split_pane hangs")
        .unwrap()
    };
    let mut seen = Seen { rx, pane: child, states: Vec::new() };
    seen.wait("the split's first prompt", 0, |s| s.phase == Phase::Input).await;
    sm.close_pane(child).unwrap();
    sm.close_pane(pane).unwrap();
}
