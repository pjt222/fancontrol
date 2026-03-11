# Fan_Set_Table Verification Test

Test whether `Fan_Set_Table` actually modifies EC fan curve behavior on Legion 82RG.

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

The custom curve is volatile -- it resets automatically on:
- Reboot
- Sleep/wake
- Power mode change (Fn+Q)

To manually reset, change the power mode with Fn+Q or reboot.
