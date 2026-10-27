# Helpers for the installer job. The runner is an administrator, and the server never runs
# elevated, so a real server has to be started at MEDIUM integrity, as a normal shell of the user
# would. The token of this process is made a restricted token (CreateRestrictedToken: allowed
# without any privilege because it is derived from our own token) with the integrity level set to
# medium, and the command runs with it. Dot-source this file.

Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;
public static class MediumLaunch {
  [StructLayout(LayoutKind.Sequential, CharSet = CharSet.Unicode)]
  struct STARTUPINFO {
    public int cb; public string lpReserved; public string lpDesktop; public string lpTitle;
    public int dwX, dwY, dwXSize, dwYSize, dwXCountChars, dwYCountChars, dwFillAttribute, dwFlags;
    public short wShowWindow, cbReserved2;
    public IntPtr lpReserved2, hStdInput, hStdOutput, hStdError;
  }
  [StructLayout(LayoutKind.Sequential)]
  struct PROCESS_INFORMATION { public IntPtr hProcess; public IntPtr hThread; public int dwProcessId; public int dwThreadId; }
  [StructLayout(LayoutKind.Sequential)]
  struct SID_AND_ATTRIBUTES { public IntPtr Sid; public uint Attributes; }
  [DllImport("kernel32.dll")] static extern IntPtr GetCurrentProcess();
  [DllImport("advapi32.dll", SetLastError = true)] static extern bool OpenProcessToken(IntPtr h, uint access, out IntPtr tok);
  [DllImport("advapi32.dll", SetLastError = true)] static extern bool CreateRestrictedToken(IntPtr t, uint flags, uint nd, IntPtr d, uint np, IntPtr p, uint nr, IntPtr r, out IntPtr nt);
  [DllImport("advapi32.dll", SetLastError = true, CharSet = CharSet.Unicode)] static extern bool ConvertStringSidToSidW(string s, out IntPtr sid);
  [DllImport("advapi32.dll")] static extern uint GetLengthSid(IntPtr sid);
  [DllImport("advapi32.dll", SetLastError = true)] static extern bool SetTokenInformation(IntPtr t, int cls, ref SID_AND_ATTRIBUTES info, uint len);
  [DllImport("advapi32.dll", SetLastError = true, CharSet = CharSet.Unicode)]
  static extern bool CreateProcessAsUserW(IntPtr tok, string app, string cmd, IntPtr pa, IntPtr ta, bool inherit, uint flags, IntPtr env, string cwd, ref STARTUPINFO si, out PROCESS_INFORMATION pi);
  [DllImport("kernel32.dll")] static extern uint WaitForSingleObject(IntPtr h, uint ms);
  [DllImport("kernel32.dll")] static extern bool GetExitCodeProcess(IntPtr h, out uint code);
  [DllImport("kernel32.dll")] static extern bool CloseHandle(IntPtr h);

  static void Check(bool ok, string what) {
    if (!ok) throw new System.ComponentModel.Win32Exception(Marshal.GetLastWin32Error(), what);
  }

  // Run `cmdLine` with a restricted token at medium integrity and wait up to `ms` for it to end.
  // Returns its exit code.
  public static int Run(string cmdLine, uint ms) {
    IntPtr tok, rtok, sid;
    // TOKEN_ASSIGN_PRIMARY | TOKEN_DUPLICATE | TOKEN_QUERY
    Check(OpenProcessToken(GetCurrentProcess(), 0x1 | 0x2 | 0x8, out tok), "OpenProcessToken");
    // DISABLE_MAX_PRIVILEGE
    Check(CreateRestrictedToken(tok, 0x1, 0, IntPtr.Zero, 0, IntPtr.Zero, 0, IntPtr.Zero, out rtok), "CreateRestrictedToken");
    Check(ConvertStringSidToSidW("S-1-16-8192", out sid), "ConvertStringSidToSid");
    var label = new SID_AND_ATTRIBUTES { Sid = sid, Attributes = 0x20 }; // SE_GROUP_INTEGRITY
    // TokenIntegrityLevel = 25
    Check(SetTokenInformation(rtok, 25, ref label, (uint)Marshal.SizeOf(typeof(SID_AND_ATTRIBUTES)) + GetLengthSid(sid)), "SetTokenInformation");
    var si = new STARTUPINFO(); si.cb = Marshal.SizeOf(typeof(STARTUPINFO));
    PROCESS_INFORMATION pi;
    // CREATE_NO_WINDOW
    Check(CreateProcessAsUserW(rtok, null, cmdLine, IntPtr.Zero, IntPtr.Zero, false, 0x08000000, IntPtr.Zero, null, ref si, out pi), "CreateProcessAsUser");
    CloseHandle(pi.hThread);
    uint wait = WaitForSingleObject(pi.hProcess, ms);
    uint code = 0xFFFFFFFF;
    if (wait == 0) GetExitCodeProcess(pi.hProcess, out code);
    CloseHandle(pi.hProcess); CloseHandle(rtok); CloseHandle(tok);
    return unchecked((int)code);
  }
}
'@

# The id of the running `forgetty.exe server run` of $Exe, or $null.
function Get-ServerPid([string]$Exe) {
  $procs = Get-CimInstance Win32_Process -Filter "Name = 'forgetty.exe'" |
    Where-Object { $_.ExecutablePath -eq $Exe -and $_.CommandLine -match 'server\s+run' }
  if ($procs) { return [int]($procs | Select-Object -First 1).ProcessId }
  return $null
}

# Start the server of the installed copy at medium integrity. Returns its process id. Throws when
# it does not come up, or when it is not below this (elevated) shell: an elevated CLI refuses a
# lower server, and that refusal is the proof.
function Start-MediumServer([string]$Exe) {
  if (Get-ServerPid $Exe) { throw "a server is already running" }
  $log = Join-Path $env:RUNNER_TEMP "medium-start.log"
  Remove-Item $log -ErrorAction SilentlyContinue
  # cmd /s strips the outer quotes, so the quoted exe path and the redirect survive.
  $cmd = "cmd.exe /d /s /c `"`"$Exe`" server start > `"$log`" 2>&1`""
  $code = [MediumLaunch]::Run($cmd, 60000)
  Write-Host "medium 'server start': exit $code"
  if (Test-Path $log) { Write-Host (Get-Content $log -Raw) }
  $deadline = (Get-Date).AddSeconds(60)
  $serverPid = $null
  while ((Get-Date) -lt $deadline -and -not $serverPid) {
    Start-Sleep -Milliseconds 500
    $serverPid = Get-ServerPid $Exe
  }
  if (-not $serverPid) { throw "the medium server did not start (start exit $code)" }
  # Give it a moment to create the pipe, then ask it as an elevated client.
  Start-Sleep -Seconds 3
  $ErrorActionPreference = "Continue"
  $out = & $Exe server status 2>&1 | Out-String
  $code = $LASTEXITCODE
  $global:LASTEXITCODE = 0
  Write-Host "elevated 'server status' against the medium server: exit $code, $out"
  if ($code -eq 0 -or $out -notmatch 'lower integrity') {
    throw "the server (pid $serverPid) is not at a lower integrity level than this shell"
  }
  Write-Host "medium server running, pid $serverPid"
  return $serverPid
}

# Wait for a process id to be gone.
function Wait-Gone([int]$ProcessId, [int]$Seconds = 30) {
  $deadline = (Get-Date).AddSeconds($Seconds)
  while ((Get-Date) -lt $deadline) {
    if (-not (Get-Process -Id $ProcessId -ErrorAction SilentlyContinue)) { return $true }
    Start-Sleep -Milliseconds 300
  }
  return $false
}
