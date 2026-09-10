param([string]$Binary="$PSScriptRoot\target\debug\rs_agent_router.exe",[string]$Config="$PSScriptRoot\router.local.json")
# 真实验证 App 路由与CLI 原生上下文复用；Harness 仅提交工作和消息。
$ErrorActionPreference='Stop'
if(Get-Process rs_agent_router -ErrorAction SilentlyContinue){throw 'Close existing router before isolated verification'}
. "$PSScriptRoot\tests\support.ps1"
$testRoot=Join-Path $env:TEMP ('router-v3-live-'+(Get-Date -Format 'yyyyMMdd-HHmmss'))
New-Item -ItemType Directory -Path $testRoot|Out-Null
Write-Output "Evidence: $testRoot"
$root=Join-Path $testRoot 'runs'
$source=Get-Content -LiteralPath $Config -Raw|ConvertFrom-Json
$configPath=Join-Path $testRoot 'app-config.json'
@{claude=@{program=$source.claude.program;model='GLM-5.2';effort='low'};codex=@{program=$source.codex.program;model=$source.codex.model;effort='low'}}|ConvertTo-Json|Set-Content -LiteralPath $configPath -Encoding utf8NoBOM
$managerCall=Start-Router @('--config',$configPath,'--runs-dir',$root)
Start-Sleep -Seconds 2
$shown=Finish-Router (Start-Router @('show'))
Capture-Manager $shown.events[-1].manager_pid
Set-Timeout 180
$records=@()
foreach($cli in @('claude','codex')){
    Select-CLI $(if($cli -eq 'claude'){'Claude'}else{'Codex'})
    $path=Write-Task "$cli-session" 'Use a tool to read input.json and sum its numbers. Remember the secret code MAPLE-731 for this conversation. Reply exactly 42. Do not edit files or delegate.'
    $before=(Get-FileHash -LiteralPath (Join-Path $testRoot "$cli-session/input.json")).Hash
    $reply=Finish-Router (Start-Router @('submit','--request',$path,'--wait')) 180000
    $final=$reply.events[-1]
    if($reply.code -ne 0 -or $final.result.outcome.answer.Trim() -ne '42'){throw "First turn failed: $cli, $($final|ConvertTo-Json -Depth 5 -Compress)"}
    $status=Task-Status "$cli-session"
    if($status.task.backend -ne $cli -or $status.task.effort -ne 'low'){throw 'App routing snapshot mismatch'}
    if((Get-FileHash -LiteralPath (Join-Path $testRoot "$cli-session/input.json")).Hash -ne $before){throw 'Input file changed'}
    $records+=@{case="$cli-first";state=$status.state;session_id=$status.session_id;effort=$status.task.effort;model=$status.task.model}
    Write-Output ($records[-1]|ConvertTo-Json -Compress)
}
# 默认 CLI 已切到 Codex；Claude 旧会话仍必须用原配置续聊。
foreach($cli in @('claude','codex')){
    $previous=Task-Status "$cli-session"
    $message=Join-Path $testRoot "$cli-message.txt"
    [System.IO.File]::WriteAllText($message,'What secret code did I ask you to remember? Reply with only that code. Do not use tools.')
    $reply=Finish-Router (Start-Router @('send','--task-id',"$cli-session",'--message-file',$message,'--wait')) 180000
    $status=Task-Status "$cli-session"
    if($reply.code -ne 0 -or $status.result.outcome.answer.Trim() -ne 'MAPLE-731' -or $status.session_id -ne $previous.session_id -or $status.turn -ne 2 -or $status.task.backend -ne $cli){throw "Native resume failed: $cli, $($status.result|ConvertTo-Json -Depth 5 -Compress)"}
    if(!(Test-Path -LiteralPath (Join-Path $status.directory 'turns/0001/result.json')) -or !(Test-Path -LiteralPath (Join-Path $status.directory 'turns/0002/result.json'))){throw 'Per-turn evidence missing'}
    $records+=@{case="$cli-resume";state=$status.state;session_id=$status.session_id;turn=$status.turn;answer=$status.result.outcome.answer;tool_calls=$status.result.outcome.tool_calls}
    Write-Output ($records[-1]|ConvertTo-Json -Compress)
}
# 写权限来自 App 勾选项，任务 JSON 不携带权限和路由字段。
Select-CLI 'Claude'
$check=Find-Control '允许修改文件'
$check.GetCurrentPattern([System.Windows.Automation.TogglePattern]::Pattern).Toggle()
Click-Control '保存设置'
$path=Write-Task 'write-check' 'Read input.json and use Write to create answer.txt containing exactly 42. Only answer.txt may be changed. Do not run shell commands or commit. Reply exactly 42.'
$reply=Finish-Router (Start-Router @('submit','--request',$path,'--wait')) 180000
if($reply.code -ne 0 -or (Get-Content -LiteralPath (Join-Path $testRoot 'write-check/answer.txt') -Raw).Trim() -ne '42'){throw 'Write verification failed'}
$records+=@{case='write';state='succeeded'}
$records|ConvertTo-Json -Depth 5|Set-Content -LiteralPath (Join-Path $testRoot 'verification.json') -Encoding utf8NoBOM
Write-Output "PASS: routing, native conversation, evidence and write. Manager remains open: $testRoot"
