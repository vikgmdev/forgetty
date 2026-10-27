# Helpers for the installer job. The runner is an administrator, and the server never runs
# elevated, so a real server has to be started at MEDIUM integrity, as a normal shell of the user
# would. `runas /trustlevel:0x20000` (basic user) gives a restricted token at medium integrity.
# Dot-source this file.

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
  & runas.exe /trustlevel:0x20000 "`"$Exe`" server start" | Out-Host
  $deadline = (Get-Date).AddSeconds(60)
  $serverPid = $null
  while ((Get-Date) -lt $deadline -and -not $serverPid) {
    Start-Sleep -Milliseconds 500
    $serverPid = Get-ServerPid $Exe
  }
  if (-not $serverPid) { throw "the medium server did not start" }
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
