param([string]$Binary="$PSScriptRoot\target\debug\rs_agent_router.exe")
# 以专用临时管理实例验证 IPC/并发/确认退出；运行前要求没有既有管理实例。
$ErrorActionPreference='Stop'
if(Get-Process rs_agent_router -ErrorAction SilentlyContinue){throw 'Close existing router before isolated verification'}
. "$PSScriptRoot\tests\support.ps1"
$testRoot=Join-Path $env:TEMP ('router-v3-fixture-'+(Get-Date -Format 'yyyyMMdd-HHmmss'))
New-Item -ItemType Directory -Path $testRoot|Out-Null
Write-Output "Evidence: $testRoot"
$fixture=Join-Path $testRoot 'fixture.exe'
rustc --edition 2024 "$PSScriptRoot\tests\fixture_cli.rs" -o $fixture
if($LASTEXITCODE -ne 0){throw 'fixture compile failed'}
$config=Join-Path $testRoot '配置 路径.json'
@{claude=@{program=$fixture;model='fixture';effort='low'};codex=@{program=(Join-Path $testRoot 'absent.exe');model=$null;effort='low'}}|ConvertTo-Json|Set-Content -LiteralPath $config -Encoding utf8NoBOM
$root=Join-Path $testRoot '运行 记录'
# 首次 submit 自动拉起管理页，非 wait 客户端退出后任务仍运行。
$first=Write-Task 'running-1' 'hang'
$reply=Finish-Router (Start-Router @('submit','--request',$first,'--config',$config,'--runs-dir',$root))
Capture-Manager $reply.events[-1].manager_pid
$managerId=$script:managerProcess.Id
$second=Finish-Router (Start-Router @('show'))
if($second.events[-1].manager_pid -ne $managerId){throw 'Multiple manager instances'}
$waiting=@()
foreach($n in 2..4){$path=Write-Task "running-$n" 'hang';$waiting+=Start-Router @('submit','--request',$path,'--wait')}
Start-Sleep -Seconds 2
foreach($n in 1..4){if((Task-Status "running-$n").state -ne 'running'){throw 'Four tasks did not run concurrently'}}
# UIA 读取真实界面；右侧每个运行面板只有一个“取消”按钮。
$element=[System.Windows.Automation.AutomationElement]::FromHandle($script:windowHandle)
$condition=[System.Windows.Automation.PropertyCondition]::new([System.Windows.Automation.AutomationElement]::NameProperty,'取消')
$buttons=$element.FindAll([System.Windows.Automation.TreeScope]::Descendants,$condition)
if($buttons.Count -ne 3){throw "Expected 3 visible execution panels, got $($buttons.Count)"}
Write-Output 'PASS: singleton + four running tasks + three visible panels'
# 等待客户端被关闭后，已接收任务仍由管理器持有。
$detachedPath=Write-Task 'detached' 'hang'
$detached=Start-Router @('submit','--request',$detachedPath,'--wait')
Start-Sleep -Milliseconds 800
$detached.process.Kill()
$detached.process.WaitForExit()
if((Task-Status 'detached').state -ne 'running'){throw 'Disconnected waiting client cancelled task'}
$null=Finish-Router (Start-Router @('cancel','--task-id','detached'))
Start-Sleep -Milliseconds 600
if((Task-Status 'detached').state -ne 'cancelled'){throw 'Detached task cancellation failed'}
Write-Output 'PASS: waiting-client disconnection preserves task'
# 最小化后后台仍响应，show 能恢复相同窗口。
[RouterWindow]::ShowWindow($script:windowHandle,6)|Out-Null
Start-Sleep -Seconds 1
if([RouterWindow]::IsWindowVisible($script:windowHandle)){throw 'Minimize did not hide to tray'}
if((Task-Status 'running-1').state -ne 'running'){throw 'Hidden manager stopped working'}
$null=Finish-Router (Start-Router @('show'))
Start-Sleep -Milliseconds 400
if(![RouterWindow]::IsWindowVisible($script:windowHandle)){throw 'Show did not restore window'}
Write-Output 'PASS: tray hide + background status + restore'
# 关闭确认取消分支：保持四个任务运行。
if(!$script:managerProcess.CloseMainWindow()){throw 'Cannot request close'}
Click-Control '继续运行'
if((Task-Status 'running-1').state -ne 'running'){throw 'Dismissed close cancelled tasks'}
# 单任务取消不影响其它任务。
$null=Finish-Router (Start-Router @('cancel','--task-id','running-1'))
Start-Sleep -Milliseconds 700
if((Task-Status 'running-1').state -ne 'cancelled'){throw 'Individual cancellation failed'}
# 确认退出：等待客户端收到 manager_shutdown，并检查子进程全部退出。
if(!$script:managerProcess.CloseMainWindow()){throw 'Cannot request final close'}
Click-Control '终止全部并退出'
foreach($call in $waiting){$final=Finish-Router $call;if($final.code -eq 0 -or $final.events[-1].error_code -ne 'manager_shutdown'){throw 'Shutdown error not returned to harness'}}
if(!$script:managerProcess.WaitForExit(15000)){throw 'Manager did not exit'}
foreach($n in 1..4){$childId=[int](Get-Content (Join-Path $testRoot "running-$n/child.pid"));if(Get-Process -Id $childId -ErrorAction SilentlyContinue){throw 'Child process survived'}}
Write-Output 'PASS: close confirmation branches + cancellation + shutdown errors + process cleanup'
# 重启同一库并验证历史，随后测试四种失败和一个正常结果。
$managerCall=Start-Router @('show','--config',$config,'--runs-dir',$root)
Start-Sleep -Seconds 2
$shown=Finish-Router (Start-Router @('show'))
Capture-Manager $shown.events[-1].manager_pid
if((Task-Status 'running-2').result.error_code -ne 'manager_shutdown'){throw 'History not restored'}
foreach($case in @('incomplete','error','timeout','missing','success')){
    Select-CLI $(if($case -eq 'missing'){'Codex'}else{'Claude'})
    Set-Timeout $(if($case -eq 'timeout'){1}else{120})
    $prompt=if($case -eq 'timeout'){'hang'}else{$case}
    $path=Write-Task $case $prompt
    $result=Finish-Router (Start-Router @('submit','--request',$path,'--wait'))
    $expected=switch($case){success{'succeeded'}timeout{'timed_out'}default{'failed'}}
    if($result.events[-1].state -ne $expected){throw "$case returned incorrect state"}
    Write-Output "PASS: $case -> $expected"
}
$repeat=Finish-Router (Start-Router @('submit','--request',$path))
if($repeat.code -eq 0){throw 'Duplicate task accepted'}
$before=(Task-Status 'success').last_activity
$null=Task-Status 'success'
if((Task-Status 'success').last_activity -ne $before){throw 'Status query changed activity'}
if(!$script:managerProcess.CloseMainWindow()){throw 'Cannot close idle manager'}
if(!$script:managerProcess.WaitForExit(15000)){throw 'Idle exit failed'}
Write-Output "PASS: duplicate IDs + read-only status + idle exit. Evidence: $testRoot"
