# Command line and the console

The release `forgetty-win.exe` is a GUI-subsystem program. It has no console of its own, so a launch from the
Start menu or a shortcut opens no extra terminal window. Debug builds keep a console.

Only `--smoke` prints. With `--smoke` the exe attaches to the console that started it, so its JSON (stdout) and
log lines (stderr) appear there. A normal launch never joins the console of the terminal that started it, so
Ctrl+C or closing that tab does not kill Forgetty.

## Waiting for it, and the exit code

`cmd.exe` and PowerShell do not wait for a GUI-subsystem program. At a prompt, `forgetty-win.exe --smoke`
returns at once and the exit code is lost. Start it so the shell waits:

```
start /wait forgetty-win.exe --smoke
echo %ERRORLEVEL%
```

```
$p = Start-Process .\forgetty-win.exe -ArgumentList '--smoke' -Wait -PassThru -RedirectStandardOutput smoke.json
$p.ExitCode
```

CI runs the smoke the second way (`.github/workflows/windows-client.yml`). Redirect stdout to a file in both cases
if you want the JSON.

## Errors with no console

A Start-menu launch has no stderr. A config error (the app then runs with defaults) and a fatal startup error
show in a message box.
