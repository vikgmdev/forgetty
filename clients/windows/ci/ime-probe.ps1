# Can this runner install a Japanese or Chinese IME? Records what it finds in ime-probe.json.
# Nothing here fails the job: the answer is the result. Used by the `ime-probe` job.
$ErrorActionPreference = 'Continue'
$r = [ordered]@{}
$os = Get-CimInstance Win32_OperatingSystem
$r.os = $os.Caption
$r.build = $os.BuildNumber
$r.install_language_cmdlet = [bool](Get-Command Install-Language -ErrorAction SilentlyContinue)
$r.language_pack_module = [bool](Get-Module -ListAvailable -Name LanguagePackManagement)
$r.interactive_session = [Environment]::UserInteractive
$r.session_name = $env:SESSIONNAME

# Microsoft IME (Japanese) and Microsoft Pinyin (Chinese Simplified) text input processors.
$tips = [ordered]@{
  'ja-JP-microsoft-ime' = '{03B5835F-F03C-411B-9CE2-AA23E1171E36}'
  'zh-CN-microsoft-pinyin' = '{81D4E9C9-1D3B-41BC-9E6C-4B40BF79E35E}'
}
function Test-Tips {
  $o = [ordered]@{}
  foreach ($k in $tips.Keys) { $o[$k] = Test-Path "HKLM:\SOFTWARE\Microsoft\CTF\TIP\$($tips[$k])" }
  $o
}
$r.tips_before = Test-Tips
$r.imejp_dir_before = Test-Path "$env:windir\System32\IME\IMEJP"

function Try-Step($name, [scriptblock]$body, [int]$timeoutSec) {
  $sw = [Diagnostics.Stopwatch]::StartNew()
  $job = Start-Job -ScriptBlock $body
  $done = Wait-Job $job -Timeout $timeoutSec
  $res = [ordered]@{ seconds = [math]::Round($sw.Elapsed.TotalSeconds, 1) }
  if ($done) {
    $res.state = $job.State
    $res.output = (Receive-Job $job -ErrorAction SilentlyContinue 2>&1 | Out-String).Trim()
    if ($job.ChildJobs[0].Error.Count -gt 0) { $res.error = ($job.ChildJobs[0].Error | Out-String).Trim() }
  } else {
    $res.state = 'timeout'
    Stop-Job $job
  }
  Remove-Job $job -Force
  $res
}

$r.steps = [ordered]@{}
if ($r.install_language_cmdlet) {
  foreach ($lang in 'ja-JP', 'zh-CN') {
    $r.steps["install_language_$lang"] = Try-Step "install $lang" ([scriptblock]::Create("Install-Language -Language $lang -ErrorAction Stop")) 180
  }
} else {
  foreach ($cap in 'Language.Basic~~~ja-JP~0.0.1.0', 'Language.Basic~~~zh-CN~0.0.1.0') {
    $r.steps["capability_$cap"] = Try-Step "cap $cap" ([scriptblock]::Create("Add-WindowsCapability -Online -Name '$cap' -ErrorAction Stop")) 180
  }
}
try { $r.installed_languages = @(Get-InstalledLanguage -ErrorAction Stop | ForEach-Object { $_.LanguageId }) } catch { $r.installed_languages_error = "$_" }
try {
  $r.capabilities = @(Get-WindowsCapability -Online -ErrorAction Stop | Where-Object { $_.Name -match 'ja-JP|zh-CN' -and $_.State -eq 'Installed' } | ForEach-Object { $_.Name })
} catch { $r.capabilities_error = "$_" }

# Turn the input methods on for the user.
try {
  $list = Get-WinUserLanguageList
  foreach ($tag in 'ja-JP', 'zh-CN') { if (-not ($list | Where-Object LanguageTag -eq $tag)) { $list.Add($tag) } }
  Set-WinUserLanguageList $list -Force -ErrorAction Stop
  $r.user_languages = @(Get-WinUserLanguageList | ForEach-Object { [ordered]@{ tag = $_.LanguageTag; tips = @($_.InputMethodTips) } })
} catch { $r.user_languages_error = "$_" }

$r.tips_after = Test-Tips
$r.imejp_dir_after = Test-Path "$env:windir\System32\IME\IMEJP"
$r.keyboard_layouts = @(Get-ChildItem 'HKCU:\Keyboard Layout\Preload' -ErrorAction SilentlyContinue | ForEach-Object { $_.Name } )
$r.can_install_ime = ($r.tips_after.Values -contains $true)
$r | ConvertTo-Json -Depth 6 | Tee-Object -FilePath ime-probe.json
