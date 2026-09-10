# 验证脚本公共过程：逐项传参、查询本次启动的窗口，不操作其它用户应用。
Add-Type -AssemblyName UIAutomationClient
Add-Type -TypeDefinition 'using System; using System.Runtime.InteropServices; public struct RouterPoint { public int X; public int Y; } public static class RouterWindow { [DllImport("user32.dll")] public static extern bool ShowWindow(IntPtr h,int n); [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr h); [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr h); [DllImport("user32.dll")] public static extern bool PostMessageW(IntPtr h,uint m,UIntPtr w,IntPtr l); [DllImport("user32.dll")] public static extern bool ScreenToClient(IntPtr h,ref RouterPoint p); [DllImport("shell32.dll",CharSet=CharSet.Unicode)] public static extern uint ExtractIconEx(string path,int index,IntPtr large,IntPtr small,uint count); }'

# 启动一次 CLI 客户端并把机器输出写到独立测试文件；返回进程对象供等待。
function Start-Router([string[]]$Arguments) {
    $info=[System.Diagnostics.ProcessStartInfo]::new($Binary)
    $info.UseShellExecute=$false
    $info.CreateNoWindow=$true
    $info.RedirectStandardOutput=$true
    $info.RedirectStandardError=$true
    foreach($arg in $Arguments){$info.ArgumentList.Add($arg)}
    $process=[System.Diagnostics.Process]::Start($info)
    return @{process=$process;out=$process.StandardOutput.ReadToEndAsync();err=$process.StandardError.ReadToEndAsync()}
}
# 等待客户端结束并返回最后一条 JSON；超时保留进程供诊断，不全局终止应用。
function Finish-Router($call,[int]$Timeout=20000) {
    if(!$call.process.WaitForExit($Timeout)){throw "CLI wait timeout: $($call.process.Id)"}
    $stdout=$call.out.GetAwaiter().GetResult()
    $stderr=$call.err.GetAwaiter().GetResult()
    if(!$stdout.Trim()){throw "No stdout, code=$($call.process.ExitCode): $stderr"}
    $events=@($stdout.Trim() -split "`n" | ForEach-Object {$_ | ConvertFrom-Json})
    # 测试断言在本地读取完整证据；不要求 Harness 默认输出完整报告。
    foreach($event in $events) {
        if($event.type -eq 'finished' -and !$event.PSObject.Properties['result'] -and (Test-Path -LiteralPath $event.result_path -PathType Leaf)) {
            $event | Add-Member -NotePropertyName result -NotePropertyValue (Get-Content -LiteralPath $event.result_path -Raw | ConvertFrom-Json)
        }
    }
    return @{code=$call.process.ExitCode;events=$events;stderr=$stderr}
}
# 读取指定 ID 的状态，不更改 last_activity。
function Task-Status([string]$Id) {
    $reply=Finish-Router (Start-Router @('status','--task-id',$Id,'--full'))
    return $reply.events[-1].task
}
# 等待 UIA 控件出现，只从本次管理窗口中查找。
function Find-Control([string]$Name) {
    $deadline=(Get-Date).AddSeconds(12)
    do {
        $element=[System.Windows.Automation.AutomationElement]::FromHandle($script:windowHandle)
        $condition=[System.Windows.Automation.PropertyCondition]::new([System.Windows.Automation.AutomationElement]::NameProperty,$Name)
        $found=$element.FindFirst([System.Windows.Automation.TreeScope]::Descendants,$condition)
        if($null -ne $found){return $found}
        Start-Sleep -Milliseconds 200
    }while((Get-Date) -lt $deadline)
    throw "UI control not found: $Name"
}
# 调用真实 UIA Invoke 控件，覆盖用户确认路径。
function Click-Control([string]$Name) {
    $found=Find-Control $Name
    $pattern=$found.GetCurrentPattern([System.Windows.Automation.InvokePattern]::Pattern)
    $pattern.Invoke()
    Start-Sleep -Milliseconds 350
}
# 记录唯一管理实例的窗口，窗口加载完成前不发送关闭消息。
function Capture-Manager([int]$ManagerId) {
    $script:managerProcess=Get-Process -Id $ManagerId
    if(!$script:managerProcess.WaitForInputIdle(15000)){throw 'Manager UI did not initialize'}
    $deadline=(Get-Date).AddSeconds(15)
    do{$script:managerProcess.Refresh();$script:windowHandle=$script:managerProcess.MainWindowHandle;if($script:windowHandle -ne 0){break};Start-Sleep -Milliseconds 100}while((Get-Date) -lt $deadline)
    if($script:windowHandle -eq 0){throw 'Manager window missing'}
    [RouterWindow]::SetForegroundWindow($script:windowHandle)|Out-Null
    $null=Find-Control '入口'
}
# 保存测试任务，路径可含中文和空格。
function Write-Task([string]$Id,[string]$Prompt,[string[]]$Groups=@('Codex桌面端','rs_agent_router','Feat-verify')) {
    $workdir=Join-Path $testRoot $Id
    New-Item -ItemType Directory -Path $workdir -Force|Out-Null
    [System.IO.File]::WriteAllText((Join-Path $workdir 'input.json'),'{"numbers":[7,11,24]}')
    $path=Join-Path $testRoot "$Id.json"
    @{task_id=$Id;title=$Id;group_path=$Groups;workdir=$workdir;prompt=$Prompt}|ConvertTo-Json|Set-Content -LiteralPath $path -Encoding utf8NoBOM
    return $path
}

# 通过真实管理页切换 CLI，任务请求中不携带路由字段。
function Select-CLI([string]$Name) {
    $control=Find-Control $Name
    $control.GetCurrentPattern([System.Windows.Automation.TogglePattern]::Pattern).Toggle()
    Start-Sleep -Milliseconds 200
}
# 通过数值控件配置超时；此配置属于 App，不由 Harness 请求覆盖。
function Set-Timeout([double]$Seconds) {
    $root=[System.Windows.Automation.AutomationElement]::FromHandle($script:windowHandle)
    $nodes=$root.FindAll([System.Windows.Automation.TreeScope]::Descendants,[System.Windows.Automation.Condition]::TrueCondition)
    $field=@($nodes|Where-Object {$_.Current.Name -eq '超时秒' -and @($_.GetSupportedPatterns()|Where-Object {$_.ProgrammaticName -eq 'RangeValuePatternIdentifiers.Pattern'}).Count})[0]
    if(!$field){throw 'Timeout control missing'}
    $field.GetCurrentPattern([System.Windows.Automation.RangeValuePattern]::Pattern).SetValue($Seconds)
}
