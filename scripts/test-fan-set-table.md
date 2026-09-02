# Fan_Set_Table Verification Test

Test whether `Fan_Set_Table` actually modifies EC fan curve behavior on Legion 82RG.

> **Superseded, kept as the March 2026 plan.** The question was settled on
> 2026-08-19 by `tools/Invoke-FanTableLoadTest.ps1` (#10): `Fan_Set_Table`
> works, and the step scale is a 0-10 index where 0 stops the fan and steps
> 1-10 map onto table entries 0-9 (#18), so the "index 5 = 3400 RPM" arithmetic
> below is off by one. The Cleanup section's volatility claim is wrong for the
> power-mode case; see the note there. Do not run this plan; run the tool.

## Prerequisites

- Windows native (not WSL)
- Administrator PowerShell
- Cross-compiled binary: `cargo build --release --target x86_64-pc-windows-gnu`

## Setup

```powershell
copy D:\dev\p\fancontrol\target\x86_64-pc-windows-gnu\release\fancontrol.exe D:\dev\p\fancontrol\fancontrol.exe
cd D:\dev\p\fancontrol
```

## Step 1: Record Baseline

```powershell
.\fancontrol.exe table
.\fancontrol.exe list
```

Note the current fan speeds and curve data. At idle (<58C), fans should be at 1600 RPM or stopped.

## Step 2: Apply Aggressive Custom Curve

```powershell
.\fancontrol.exe set-curve --fan-id 0 --sensor-id 3 --steps 5,5,5,5,5,5,5,5,8,10
```

This sets CPU fan (fan 0, sensor 3) to use speed index 5 (3400 RPM) for all temperature
thresholds 0-7, index 8 (4400 RPM) for threshold 8, and index 10 (max) for threshold 9.

The default curve uses index 0 (1600 RPM) at the first threshold (58C). If Fan_Set_Table
works, the fan should jump to 3400 RPM at 58C instead of 1600 RPM.

## Step 3: Stress the CPU

Generate CPU load to push temperature above 58C:

```powershell
# Quick CPU stress (~30 seconds)
1..4 | ForEach-Object -Parallel { while($true) { [Math]::Sqrt(12345) } } -TimeoutSeconds 30
```

Or run any CPU benchmark (Cinebench, Prime95, etc.).

## Step 4: Observe Fan Behavior

While CPU is above 58C, check fan speeds:

```powershell
.\fancontrol.exe list
```

Or use continuous monitoring:

```powershell
.\fancontrol.exe monitor
```

## Expected Results

### If Fan_Set_Table WORKS

Fan 0 should ramp to ~3400 RPM at 58C (index 5) instead of the default 1600 RPM (index 0).
The difference should be clearly audible -- 3400 RPM is significantly louder than 1600 RPM.

### If Fan_Set_Table is a STUB

Fan 0 follows the normal curve: 1600 RPM at 58C, ramping gradually through higher thresholds.
Behavior is identical to normal operation without set-curve.

## Cleanup

~~The custom curve is volatile -- it resets automatically on:~~ **Superseded 2026-08-19:** the EC retains the last written curve across a power-mode change (measured: a curve reactivated 24 minutes later on re-entering Custom, having survived a switch to Performance and back). Reboot and sleep/wake retention are unmeasured. The March 2026 assumption was:
- Reboot
- Sleep/wake
- Power mode change (Fn+Q)

~~To manually reset, change the power mode with Fn+Q or reboot.~~ Fn+Q only hides a retained curve; it runs again the next time Custom is selected. Run `tools/Reset-LenovoFanState.ps1` to leave a safe curve loaded.
