<#
  Persistent elevated shell — one UAC prompt, unlimited commands after it.

    .\tools\admin.ps1 -Start          # UAC prompt -> opens the elevated listener window
    .\tools\admin.ps1 <command...>    # runs <command> in that elevated shell, prints its output
    .\tools\admin.ps1 -Status         # is the listener up?
    .\tools\admin.ps1 -Stop           # shut it down

  The listener is a normal PowerShell session, so `cd`, $env: and variables
  persist between commands. It talks over a named pipe ACL'd to the single SID
  that launched it — no other local account can push commands into it.
#>
[CmdletBinding(DefaultParameterSetName = 'Run')]
param(
  [Parameter(ParameterSetName = 'Run', Position = 0, ValueFromRemainingArguments = $true)]
  [string[]]$Command,
  [Parameter(ParameterSetName = 'Start')][switch]$Start,
  [Parameter(ParameterSetName = 'Stop')][switch]$Stop,
  [Parameter(ParameterSetName = 'Status')][switch]$Status,
  [Parameter(ParameterSetName = 'Server', Mandatory = $true)][switch]$Server,
  [Parameter(ParameterSetName = 'Server', Mandatory = $true)][string]$OwnerSid
)

$ErrorActionPreference = 'Stop'
$PipeName = 'asus-control.admin'

function ConvertTo-B64 { param([string]$s) [Convert]::ToBase64String([Text.Encoding]::UTF8.GetBytes($s)) }
function ConvertFrom-B64 { param([string]$s) [Text.Encoding]::UTF8.GetString([Convert]::FromBase64String($s)) }

# --- client -----------------------------------------------------------------

function Invoke-Remote {
  param([string]$Cmd, [int]$TimeoutMs = 2000)
  $pipe = New-Object System.IO.Pipes.NamedPipeClientStream('.', $PipeName, 'InOut')
  try { $pipe.Connect($TimeoutMs) } catch { $pipe.Dispose(); return $null }
  try {
    $w = New-Object System.IO.StreamWriter($pipe); $w.AutoFlush = $true
    $r = New-Object System.IO.StreamReader($pipe)
    $w.WriteLine((ConvertTo-B64 $Cmd))
    $out = ConvertFrom-B64 $r.ReadLine()
    $code = $r.ReadLine()
    [pscustomobject]@{ Output = $out; Exit = $code }
  } finally { $pipe.Dispose() }
}

# --- server (runs elevated) -------------------------------------------------

if ($Server) {
  $host.UI.RawUI.WindowTitle = 'asus-control — elevated shell (leave me open)'
  $sec = New-Object System.IO.Pipes.PipeSecurity
  $sid = New-Object System.Security.Principal.SecurityIdentifier($OwnerSid)
  $sec.AddAccessRule((New-Object System.IO.Pipes.PipeAccessRule($sid, 'FullControl', 'Allow')))
  $sec.AddAccessRule((New-Object System.IO.Pipes.PipeAccessRule(
        (New-Object System.Security.Principal.SecurityIdentifier('S-1-5-18')), 'FullControl', 'Allow')))

  Write-Host "elevated shell ready. pipe=$PipeName owner=$OwnerSid" -ForegroundColor Green
  Write-Host "close this window to revoke.`n" -ForegroundColor DarkGray

  while ($true) {
    $pipe = New-Object System.IO.Pipes.NamedPipeServerStream(
      $PipeName, 'InOut', 1, 'Byte', 'None', 4096, 4096, $sec)
    try {
      $pipe.WaitForConnection()
      $r = New-Object System.IO.StreamReader($pipe)
      $w = New-Object System.IO.StreamWriter($pipe); $w.AutoFlush = $true

      $line = $r.ReadLine()
      if (-not $line) { continue }
      $cmd = ConvertFrom-B64 $line

      if ($cmd -eq '__PING__') { $w.WriteLine((ConvertTo-B64 'pong')); $w.WriteLine('EXIT:0'); continue }
      if ($cmd -eq '__EXIT__') { $w.WriteLine((ConvertTo-B64 'bye')); $w.WriteLine('EXIT:0'); break }

      Write-Host "> $cmd" -ForegroundColor Cyan
      $global:LASTEXITCODE = 0
      $out = try { Invoke-Expression $cmd 2>&1 | Out-String } catch { "$_`n" }
      $w.WriteLine((ConvertTo-B64 $out))
      $w.WriteLine("EXIT:$(if ($null -eq $LASTEXITCODE) { 0 } else { $LASTEXITCODE })")
    } catch {
      Write-Host "pipe error: $_" -ForegroundColor Red
    } finally { $pipe.Dispose() }
  }
  exit 0
}

# --- start / stop / status --------------------------------------------------

if ($Status) {
  $r = Invoke-Remote '__PING__' 500
  if ($r) { Write-Host 'elevated shell: RUNNING' -ForegroundColor Green; exit 0 }
  Write-Host 'elevated shell: not running' -ForegroundColor Yellow; exit 1
}

if ($Stop) {
  if (Invoke-Remote '__EXIT__' 500) { Write-Host 'stopped.' } else { Write-Host 'was not running.' }
  exit 0
}

if ($Start) {
  if (Invoke-Remote '__PING__' 500) { Write-Host 'already running.' -ForegroundColor Green; exit 0 }
  $sid = [Security.Principal.WindowsIdentity]::GetCurrent().User.Value
  Start-Process powershell -Verb RunAs -ArgumentList @(
    '-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', $PSCommandPath, '-Server', '-OwnerSid', $sid)
  Write-Host 'waiting for UAC...' -NoNewline
  foreach ($i in 1..60) {
    if (Invoke-Remote '__PING__' 500) { Write-Host ' ready.' -ForegroundColor Green; exit 0 }
    Start-Sleep -Milliseconds 500; Write-Host '.' -NoNewline
  }
  Write-Host ' timed out.' -ForegroundColor Red; exit 1
}

# --- default: run a command -------------------------------------------------

if (-not $Command) { Get-Help $PSCommandPath -Detailed; exit 0 }

$r = Invoke-Remote ($Command -join ' ')
if (-not $r) {
  Write-Host "elevated shell not running. start it with:`n  .\tools\admin.ps1 -Start" -ForegroundColor Yellow
  exit 1
}
Write-Host $r.Output -NoNewline
exit ([int]($r.Exit -replace '^EXIT:', ''))
