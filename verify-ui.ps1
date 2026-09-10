param([string]$Binary="$PSScriptRoot\target\debug\rs_agent_router.exe")
# 只针对 verify.ps1 生成的临时会话验证交互，不删除用户任务。
$ErrorActionPreference='Stop'
if(!(Get-Process rs_agent_router -ErrorAction SilentlyContinue)){throw 'Run verify.ps1 first'}
. "$PSScriptRoot\tests\support.ps1"
$shown=Finish-Router (Start-Router @('show'))
if(!$shown.events[-1].runs_dir.StartsWith((Join-Path $env:TEMP 'router-v3-live-'))){throw 'Refusing to operate outside isolated live verification'}
Capture-Manager $shown.events[-1].manager_pid
$archive=Find-Control '显示归档';$toggle=$archive.GetCurrentPattern([System.Windows.Automation.TogglePattern]::Pattern);if($toggle.Current.ToggleState -eq [System.Windows.Automation.ToggleState]::On){$toggle.Toggle();Start-Sleep -Milliseconds 500}
$status=Task-Status 'claude-session'
$root=[System.Windows.Automation.AutomationElement]::FromHandle($script:windowHandle)
$nodes=$root.FindAll([System.Windows.Automation.TreeScope]::Descendants,[System.Windows.Automation.Condition]::TrueCondition)
if(@($nodes|Where-Object {$_.Current.Name -eq '发送' -or $_.Current.Name -eq '新建任务'}).Count){throw 'Chat-client controls must not remain in the service monitor'}
Write-Output 'PASS: service monitor has no chat-client controls'
# 树行无回答正文；点击只查看，不改变活动时间，Esc 关闭详情。
$activity=$status.last_activity
Click-Control 'claude-session'
$null=Find-Control '刷新'
[RouterWindow]::PostMessageW($script:windowHandle,0x100,[UIntPtr]27,[IntPtr]65537)|Out-Null
[RouterWindow]::PostMessageW($script:windowHandle,0x101,[UIntPtr]27,[IntPtr]65537)|Out-Null
Start-Sleep -Milliseconds 350
$condition=[System.Windows.Automation.PropertyCondition]::new([System.Windows.Automation.AutomationElement]::NameProperty,'刷新')
if($root.FindFirst([System.Windows.Automation.TreeScope]::Descendants,$condition)){throw 'Esc did not close task details'}
if((Task-Status 'claude-session').last_activity -ne $activity){throw 'Opening details changed activity'}
Write-Output 'PASS: task details Esc + read-only selection'
if([RouterWindow]::ExtractIconEx($Binary,-1,[IntPtr]::Zero,[IntPtr]::Zero,0) -eq 0){throw 'EXE icon resource missing'}
Write-Output 'PASS: embedded EXE icon (context menu and deletion covered by Rust tests)'
Write-Output 'PASS: UI verification complete'
