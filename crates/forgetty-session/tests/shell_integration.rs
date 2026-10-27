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

    // The first prompt (A and B), with the directory.
    let i = seen.wait("the first prompt (B)", 0, |s| s.phase == Phase::Input).await;
    let cwd_ok = seen.states.iter().any(|s| s.cwd.as_deref() == env.work.to_str());
    assert!(cwd_ok, "{name}: no state carries the directory {:?}", env.work);
    // The line editor edits the line itself: the kernel does not echo.
    let tty = seen
        .wait("the line editor tty mode", i, |s| s.tty.is_some_and(|t| !t.canonical && !t.echo))
        .await;

    // A command that fails: C, then the next prompt carries the exit code. The
    // server merges states that come closer than 10 ms, so D, A and B may reach
    // the client as one state: the phase is where the shell is, not an event.
    sm.write_pty(pane, b"sh -c 'exit 3'\n").unwrap();
    let c = seen.wait("command start (C)", tty, |s| s.phase == Phase::Running).await;
    let d = seen
        .wait("the prompt again with exit code 3", c, |s| {
            s.phase == Phase::Input && s.exit_code == Some(3)
        })
        .await;
    assert!(seen.states[d].rev > seen.states[c].rev);

    // A command that works: the code is 0.
    sm.write_pty(pane, b"true\n").unwrap();
    let c2 = seen.wait("command start again", d, |s| s.phase == Phase::Running).await;
    let b3 = seen
        .wait("the prompt again with exit code 0", c2, |s| {
            s.phase == Phase::Input && s.exit_code == Some(0)
        })
        .await;

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

/// All output bytes of one pane, collected in the background.
struct Output {
    buf: std::sync::Arc<std::sync::Mutex<Vec<u8>>>,
}

impl Output {
    fn start(sm: &SessionManager, pane: PaneId) -> Output {
        let buf = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut rx = sm.subscribe_output();
        let sink = buf.clone();
        tokio::spawn(async move {
            while let Ok(ev) = rx.recv().await {
                if let SessionEvent::PtyOutput { pane_id, data } = ev {
                    if pane_id == pane {
                        sink.lock().unwrap().extend_from_slice(&data);
                    }
                }
            }
        });
        Output { buf }
    }

    fn text(&self) -> String {
        String::from_utf8_lossy(&self.buf.lock().unwrap()).into_owned()
    }

    async fn wait_for(&self, needle: &str) {
        let deadline = Instant::now() + Duration::from_secs(15);
        while !self.text().contains(needle) {
            assert!(Instant::now() < deadline, "no {needle:?} in the output:\n{}", self.text());
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
}

fn integrated_pane(name: &str, rc: Option<(&str, &str)>) -> Option<(SessionManager, Env, PaneId)> {
    let shell = find_shell(name)?;
    let env = quiet_home();
    set_env(env.home.path());
    if let Some((file, content)) = rc {
        std::fs::write(env.home.path().join(file), content).unwrap();
    }
    let sm = SessionManager::new();
    sm.set_shell_integration(Some(ShellIntegration::install(&env.home.path().join("si")).unwrap()));
    let pane = spawn_pane(&sm, &shell, &env.work);
    Some((sm, env, pane))
}

/// A shell started from an integrated shell (nested bash, su, script, vim
/// :sh) must not inherit the hooks: no "command not found", a working prompt.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bash_started_from_an_integrated_bash_runs_without_the_hooks() {
    let _one = SERIAL.lock().await;
    let Some((sm, _env, pane)) = integrated_pane("bash", None) else {
        println!("SKIPPED: bash is not installed");
        return;
    };
    let rx = sm.subscribe_meta();
    let out = Output::start(&sm, pane);
    let mut seen = Seen { rx, pane, states: Vec::new() };
    let i = seen.wait("the first prompt", 0, |s| s.phase == Phase::Input).await;
    sm.write_pty(
        pane,
        b"bash -i\necho NEST-$((20+1)) pc=[$PROMPT_COMMAND] dir=[$FORGETTY_SHELL_INTEGRATION_DIR]\nexit\n",
    )
    .unwrap();
    out.wait_for("NEST-21 pc=[] dir=[]").await;
    // The outer shell has its prompt back.
    let r = seen.wait("the nested shell runs", i + 1, |s| s.phase == Phase::Running).await;
    seen.wait("the outer prompt after the nested shell", r, |s| s.phase == Phase::Input).await;
    sm.write_pty(pane, b"echo OUTER-$((5+5))\n").unwrap();
    out.wait_for("OUTER-10").await;
    let text = out.text();
    assert!(!text.contains("command not found"), "output:\n{text}");
    sm.close_pane(pane).unwrap();
}

/// The same for tmux started from a pane: the shell tmux starts runs clean.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tmux_started_from_an_integrated_bash_runs_without_the_hooks() {
    let _one = SERIAL.lock().await;
    if find_shell("tmux").is_none() {
        println!("SKIPPED: tmux is not installed");
        return;
    }
    let Some((sm, _env, pane)) = integrated_pane("bash", None) else {
        println!("SKIPPED: bash is not installed");
        return;
    };
    let rx = sm.subscribe_meta();
    let out = Output::start(&sm, pane);
    let mut seen = Seen { rx, pane, states: Vec::new() };
    seen.wait("the first prompt", 0, |s| s.phase == Phase::Input).await;
    let sock = format!("fgty-si-test-{}", std::process::id());
    let cmd = format!(
        "tmux -L {sock} -f /dev/null new-session -d -s t 'bash -i'; sleep 1; \
         tmux -L {sock} send-keys -t t 'echo TMUX-$((3+4))' Enter; sleep 1; \
         tmux -L {sock} capture-pane -p -t t; tmux -L {sock} kill-server; echo TMUX-$((1+1))END\n"
    );
    sm.write_pty(pane, cmd.as_bytes()).unwrap();
    out.wait_for("TMUX-2END").await;
    let text = out.text();
    assert!(text.contains("TMUX-7"), "tmux's shell did not run the command:\n{text}");
    assert!(!text.contains("command not found"), "output:\n{text}");
    sm.close_pane(pane).unwrap();
}

/// After every command the prompt marks come back (B follows C), so the
/// shell reaches the Input phase again.
async fn prompt_marks_survive(sm: &SessionManager, pane: PaneId, name: &str) {
    let rx = sm.subscribe_meta();
    let mut seen = Seen { rx, pane, states: Vec::new() };
    seen.wait("the line editor tty mode", 0, |s| s.tty.is_some_and(|t| !t.canonical && !t.echo))
        .await;
    for round in 0..3 {
        let from = seen.states.len();
        sm.write_pty(pane, b"true\n").unwrap();
        let c = seen
            .wait(&format!("{name} round {round}: command start"), from, |s| {
                s.phase == Phase::Running
            })
            .await;
        seen.wait(&format!("{name} round {round}: B after the theme"), c, |s| {
            s.phase == Phase::Input && s.exit_code == Some(0)
        })
        .await;
    }
}

/// A prompt theme that sets PS1 again on every prompt (starship, powerline)
/// does not remove the marks: they are added after the theme ran.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bash_theme_that_rewrites_ps1_every_prompt_keeps_the_marks() {
    let _one = SERIAL.lock().await;
    let rc = "__theme() { PS1='themed> '; }\nPROMPT_COMMAND=\"${PROMPT_COMMAND:+$PROMPT_COMMAND;}__theme\"\n";
    let Some((sm, _env, pane)) = integrated_pane("bash", Some((".bash_profile", rc))) else {
        println!("SKIPPED: bash is not installed");
        return;
    };
    prompt_marks_survive(&sm, pane, "bash").await;
    sm.close_pane(pane).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn zsh_theme_that_rewrites_ps1_every_prompt_keeps_the_marks() {
    let _one = SERIAL.lock().await;
    let rc = "_theme() { PS1='themed> ' }\nprecmd_functions+=(_theme)\n";
    let Some((sm, _env, pane)) = integrated_pane("zsh", Some((".zshrc", rc))) else {
        println!("SKIPPED: zsh is not installed");
        return;
    };
    prompt_marks_survive(&sm, pane, "zsh").await;
    sm.close_pane(pane).unwrap();
}

/// A pane that writes prompt marks in a loop: the events stay few, and the
/// last state is the true one.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_pane_writing_marks_in_a_loop_gets_merged_states() {
    let _one = SERIAL.lock().await;
    let Some((sm, _env, pane)) = integrated_pane("bash", None) else {
        println!("SKIPPED: bash is not installed");
        return;
    };
    let rx = sm.subscribe_meta();
    let out = Output::start(&sm, pane);
    let mut seen = Seen { rx, pane, states: Vec::new() };
    seen.wait("the first prompt", 0, |s| s.phase == Phase::Input).await;
    let from = seen.states.len();
    // 20000 marks (A, B, C, D), then the prompt comes back.
    sm.write_pty(
        pane,
        b"for ((i=0;i<5000;i++)); do printf '\\e]133;A\\a\\e]133;B\\a\\e]133;C\\a\\e]133;D;0\\a'; done; echo FLOOD-$((1+1))\n",
    )
    .unwrap();
    out.wait_for("FLOOD-2").await;
    // The shell comes back to a prompt: the last state says so.
    seen.wait("the prompt after the loop", from, |s| s.phase == Phase::Input).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    seen.drain();
    let n = seen.states.len() - from;
    println!("{n} states for 20000 marks");
    assert!(n < 1500, "{n} states for 20000 marks");
    let last = seen.states.last().unwrap();
    assert!(seen.states.windows(2).all(|w| w[0].rev < w[1].rev), "revisions go up");
    let now = sm.shell_state(pane).unwrap();
    assert_eq!(now.rev, last.rev, "the last event is the current state");
    assert_eq!(now.phase, Phase::Input);
    sm.close_pane(pane).unwrap();
}

/// With `extquote` off, the user's last PROMPT_COMMAND command must not get a
/// literal `$'\n'` glued to it, and the last hook must still run.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bash_with_extquote_off_keeps_the_users_prompt_command_intact() {
    let _one = SERIAL.lock().await;
    let rc = "shopt -u extquote\n__mine() { :; }\n\
              PROMPT_COMMAND=\"${PROMPT_COMMAND:+$PROMPT_COMMAND;}__mine\"\n";
    let Some((sm, _env, pane)) = integrated_pane("bash", Some((".bash_profile", rc))) else {
        println!("SKIPPED: bash is not installed");
        return;
    };
    let rx = sm.subscribe_meta();
    let out = Output::start(&sm, pane);
    let mut seen = Seen { rx, pane, states: Vec::new() };
    seen.wait("the line editor tty mode", 0, |s| s.tty.is_some_and(|t| !t.canonical && !t.echo))
        .await;
    // Second prompt: the hooks are in PROMPT_COMMAND now.
    sm.write_pty(pane, b"true\n").unwrap();
    sm.write_pty(pane, b"echo PC-$((1+1))-[${PROMPT_COMMAND//$'\\n'/NL}]\n").unwrap();
    out.wait_for("PC-2-[").await;
    let text = out.text();
    let line = text.lines().find(|l| l.contains("PC-2-[") && !l.contains("$((1+1))")).unwrap_or("");
    assert!(
        line.contains("__mine") && line.contains("NL__forgetty_last"),
        "PROMPT_COMMAND is not what it should be: {line:?}\n{text}"
    );
    assert!(!text.contains("$'\\n'__forgetty_last"), "a literal $'\\n' in PROMPT_COMMAND:\n{text}");
    assert!(!text.contains("command not found"), "output:\n{text}");
    sm.close_pane(pane).unwrap();
}

/// A pane that writes OSC 7 in a loop: the `CwdChanged` events merge (the same
/// burst and rate as `ShellState`) and the last directory is the true one.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_pane_writing_osc7_in_a_loop_gets_merged_cwd_events() {
    let _one = SERIAL.lock().await;
    let Some(shell) = find_shell("bash") else {
        println!("SKIPPED: bash is not installed");
        return;
    };
    let env = quiet_home();
    set_env(env.home.path());
    let sm = SessionManager::new();
    let mut rx = sm.subscribe_meta();
    let pane = spawn_pane(&sm, &shell, &env.work);
    let out = Output::start(&sm, pane);
    tokio::time::sleep(Duration::from_millis(500)).await;
    sm.write_pty(
        pane,
        b"for ((i=0;i<5000;i++)); do printf '\\e]7;file://%s/tmp/c%d\\a' \"$HOSTNAME\" $i; done; echo FLOOD-$((1+1))\n",
    )
    .unwrap();
    out.wait_for("FLOOD-2").await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let mut cwds: Vec<String> = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        if let SessionEvent::CwdChanged { pane_id, cwd } = ev {
            if pane_id == pane {
                cwds.push(cwd);
            }
        }
    }
    println!("{} CwdChanged for 5000 directories", cwds.len());
    assert!(cwds.len() < 300, "{} events for 5000 directories", cwds.len());
    assert_eq!(cwds.last().map(String::as_str), Some("/tmp/c4999"), "{:?}", cwds.last());
    sm.close_pane(pane).unwrap();
}

/// A startup file that turns PROMPT_COMMAND into an array with its own hooks:
/// the user's elements stay, in order, between the first and the last hook, and
/// they run on the first prompt and on the next ones.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bash_array_prompt_command_keeps_two_user_hooks() {
    let _one = SERIAL.lock().await;
    let rc = "__u1() { printf '[U1:%d]' $((1+1)); }\n__u2() { printf '[U2:%d]' $((1+2)); }\n\
              PROMPT_COMMAND+=(__u1 __u2)\n";
    let Some((sm, _env, pane)) = integrated_pane("bash", Some((".bash_profile", rc))) else {
        println!("SKIPPED: bash is not installed");
        return;
    };
    let rx = sm.subscribe_meta();
    let out = Output::start(&sm, pane);
    let mut seen = Seen { rx, pane, states: Vec::new() };
    seen.wait("the line editor tty mode", 0, |s| s.tty.is_some_and(|t| !t.canonical && !t.echo))
        .await;
    out.wait_for("[U2:3]").await;
    // Prompt 1: the user's elements ran (behind the start line, so their
    // output comes first on that prompt).
    assert!(out.text().contains("[U1:2][U2:3]"), "prompt 1 skipped the user hooks:\n{}", out.text());
    for round in 0..2 {
        let from = seen.states.len();
        sm.write_pty(pane, b"true\n").unwrap();
        let c = seen
            .wait(&format!("round {round}: command start"), from, |s| s.phase == Phase::Running)
            .await;
        seen.wait(&format!("round {round}: prompt again"), c, |s| s.phase == Phase::Input).await;
    }
    sm.write_pty(pane, b"declare -p PROMPT_COMMAND | tr -d '\\n'; echo ARRAY-$((1+1))\n").unwrap();
    out.wait_for("ARRAY-2").await;
    let text = out.text();
    let declared = text
        .lines()
        .find(|l| l.contains("declare -a PROMPT_COMMAND") && !l.contains("declare -p"))
        .unwrap_or("");
    let at = |needle: &str| declared.find(needle).unwrap_or(usize::MAX);
    assert!(
        at("__forgetty_first") < at("__u1")
            && at("__u1") < at("__u2")
            && at("__u2") < at("__forgetty_last"),
        "wrong PROMPT_COMMAND: {declared:?}\n{text}"
    );
    // The hooks ran on more than one prompt.
    assert!(text.matches("[U1:2]").count() >= 3, "user hooks did not run each prompt:\n{text}");
    assert!(!text.contains("command not found"), "output:\n{text}");
    sm.close_pane(pane).unwrap();
}

/// A hook added after the start lands behind `__forgetty_last`; the next
/// prompt moves the last hook back to the end, so the marks come after the
/// hook (which rewrites PS1). `setup` runs in the pane as a command.
async fn late_hook_keeps_marks(rc: Option<(&str, &str)>, setup: &str) {
    let Some((sm, _env, pane)) = integrated_pane("bash", rc) else {
        println!("SKIPPED: bash is not installed");
        return;
    };
    let rx = sm.subscribe_meta();
    let out = Output::start(&sm, pane);
    let mut seen = Seen { rx, pane, states: Vec::new() };
    seen.wait("the line editor tty mode", 0, |s| s.tty.is_some_and(|t| !t.canonical && !t.echo))
        .await;
    sm.write_pty(pane, format!("{setup}\n").as_bytes()).unwrap();
    sm.write_pty(pane, b"echo SETUP-$((1+1))\n").unwrap();
    out.wait_for("SETUP-2").await;
    // The first prompt after the change already has its list: it may lack the
    // marks. From the next one on they are back after the hook.
    sm.write_pty(pane, b"true\n").unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;
    for round in 0..2 {
        seen.drain();
        let from = seen.states.len();
        sm.write_pty(pane, b"true\n").unwrap();
        let c = seen
            .wait(&format!("late hook round {round}: command start"), from, |s| {
                s.phase == Phase::Running
            })
            .await;
        seen.wait(&format!("late hook round {round}: prompt marks after the hook"), c, |s| {
            s.phase == Phase::Input
        })
        .await;
    }
    sm.write_pty(pane, b"echo OK-$((2+3))\n").unwrap();
    out.wait_for("OK-5").await;
    assert!(!out.text().contains("command not found"), "output:\n{}", out.text());
    sm.close_pane(pane).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bash_hook_added_later_as_a_string_still_gets_the_marks_after_it() {
    let _one = SERIAL.lock().await;
    late_hook_keeps_marks(None, "__late() { PS1='late> '; }; PROMPT_COMMAND+=';__late'").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bash_hook_added_later_to_a_string_with_plus_array_still_gets_the_marks_after_it() {
    let _one = SERIAL.lock().await;
    late_hook_keeps_marks(None, "__late() { PS1='late> '; }; PROMPT_COMMAND+=(__late)").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bash_hook_added_later_to_an_array_still_gets_the_marks_after_it() {
    let _one = SERIAL.lock().await;
    let rc = "__u1() { :; }\nPROMPT_COMMAND+=(__u1)\n";
    late_hook_keeps_marks(
        Some((".bash_profile", rc)),
        "__late() { PS1='late> '; }; PROMPT_COMMAND+=(__late)",
    )
    .await;
}

/// bash before 5.1 runs element 0 of an array PROMPT_COMMAND only: the string
/// path is used (the elements behind 0 never ran there, and are dropped). (The version is forced; the bash of the test is newer.)
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bash_before_5_1_uses_the_string_path_for_an_array() {
    let _one = SERIAL.lock().await;
    let rc = "__u1() { printf '[U1:%d]' $((1+1)); }\nPROMPT_COMMAND+=(__u1)\n";
    // SAFETY: serialized by SERIAL, no other test reads the variable.
    unsafe { std::env::set_var("FORGETTY_BASH_VERSION_FOR_TEST", "5.0") };
    let made = integrated_pane("bash", Some((".bash_profile", rc)));
    let Some((sm, _env, pane)) = made else {
        println!("SKIPPED: bash is not installed");
        return;
    };
    let rx = sm.subscribe_meta();
    let out = Output::start(&sm, pane);
    let mut seen = Seen { rx, pane, states: Vec::new() };
    seen.wait("the line editor tty mode", 0, |s| s.tty.is_some_and(|t| !t.canonical && !t.echo))
        .await;
    for round in 0..2 {
        let from = seen.states.len();
        sm.write_pty(pane, b"true\n").unwrap();
        let c = seen
            .wait(&format!("round {round}: command start"), from, |s| s.phase == Phase::Running)
            .await;
        seen.wait(&format!("round {round}: prompt again"), c, |s| s.phase == Phase::Input).await;
    }
    sm.write_pty(pane, b"declare -p PROMPT_COMMAND | tr -d '\\n'; echo STR-$((1+1))\n").unwrap();
    out.wait_for("STR-2").await;
    let text = out.text();
    let declared = text
        .lines()
        .find(|l| l.contains("declare -- PROMPT_COMMAND") && !l.contains("declare -p"))
        .unwrap_or("");
    // The string path: a plain string with first and last (an array would be
    // `declare -a`, with the hooks as elements).
    assert!(
        declared.contains("__forgetty_first;") && declared.contains("__forgetty_last"),
        "wrong PROMPT_COMMAND: {declared:?}\n{text}"
    );
    assert!(!text.contains("command not found"), "output:\n{text}");
    // SAFETY: as above.
    unsafe { std::env::remove_var("FORGETTY_BASH_VERSION_FOR_TEST") };
    sm.close_pane(pane).unwrap();
}

/// A hook added in front after the start pushes `__forgetty_first` back. In the
/// string form the prompt that follows cannot know the command's status (the
/// front hook's status is what `$?` holds), so its D mark has no code: never a
/// wrong one.
/// The order is put right, and the next command's code is the true one.
async fn front_hook_does_not_leak_its_status(setup: &str, array: bool) {
    let Some((sm, _env, pane)) = integrated_pane("bash", None) else {
        println!("SKIPPED: bash is not installed");
        return;
    };
    let rx = sm.subscribe_meta();
    let out = Output::start(&sm, pane);
    let mut seen = Seen { rx, pane, states: Vec::new() };
    seen.wait("the line editor tty mode", 0, |s| s.tty.is_some_and(|t| !t.canonical && !t.echo))
        .await;
    // The setup command adds the front hook; the prompt after it is the
    // displaced one: the front hook returns 1, the command's own status is 0.
    seen.drain();
    let from = seen.states.len();
    sm.write_pty(pane, format!("{setup}\n").as_bytes()).unwrap();
    let c = seen.wait("displaced round: command start", from, |s| s.phase == Phase::Running).await;
    seen.wait("displaced round: prompt again", c, |s| s.phase == Phase::Input).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    seen.drain();
    assert!(
        seen.states[from..].iter().all(|s| s.exit_code != Some(1)),
        "the front hook's status leaked into a D mark: {:?}",
        seen.states[from..].iter().map(|s| s.exit_code).collect::<Vec<_>>()
    );
    if array {
        // bash gives each element of an array the command's status: the D mark
        // of the displaced prompt has the true code.
        assert!(
            seen.states[from..].iter().any(|s| s.exit_code == Some(0)),
            "{:?}",
            seen.states[from..].iter().map(|s| s.exit_code).collect::<Vec<_>>()
        );
    }
    // Put right: the next command reports its own code.
    let from = seen.states.len();
    sm.write_pty(pane, b"sh -c 'exit 3'\n").unwrap();
    let c = seen.wait("next round: command start", from, |s| s.phase == Phase::Running).await;
    seen.wait("next round: the true exit code", c, |s| {
        s.phase == Phase::Input && s.exit_code == Some(3)
    })
    .await;
    assert!(!out.text().contains("command not found"), "output:\n{}", out.text());
    sm.close_pane(pane).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bash_hook_added_in_front_as_a_string_does_not_leak_its_status() {
    let _one = SERIAL.lock().await;
    front_hook_does_not_leak_its_status("__front() { return 1; }; PROMPT_COMMAND=\"__front;$PROMPT_COMMAND\"", false)
        .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bash_hook_added_in_front_as_an_array_does_not_leak_its_status() {
    let _one = SERIAL.lock().await;
    front_hook_does_not_leak_its_status(
        "__front() { return 1; }; PROMPT_COMMAND=(__front \"${PROMPT_COMMAND[@]}\")",
        true,
    )
    .await;
}

/// The hooks run on every prompt: no `$(...)`, no pipe, no external command in
/// them (a fork per prompt shows in a slow shell).
#[test]
fn the_bash_prompt_hooks_start_no_process() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("shell-integration/forgetty.bash");
    let script = std::fs::read_to_string(path).unwrap();
    let from = script.find("__forgetty_osc7() {").unwrap();
    let to = script.find("# C is written by the shell itself").unwrap();
    let hooks = &script[from..to];
    for (n, line) in hooks.lines().enumerate() {
        let code = line.split('#').next().unwrap_or("");
        assert!(!code.contains("$(") && !code.contains('`'), "hook line {n} forks: {line}");
        assert!(!code.contains(" | ") && !code.contains("<("), "hook line {n} pipes: {line}");
        for cmd in [
            "sed", "awk", "cat", "tr", "cut", "grep", "date", "hostname", "head", "tail", "env",
            "basename", "dirname", "sleep", "stty", "tput", "perl", "python",
        ] {
            assert!(
                !code.split(|c: char| !c.is_alphanumeric() && c != '_' && c != '-')
                    .any(|w| w == cmd),
                "hook line {n} runs {cmd}: {line}"
            );
        }
    }
}
