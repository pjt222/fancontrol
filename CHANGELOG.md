# Changelog

All notable changes to this project are recorded here.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and
this project intends to follow [Semantic Versioning](https://semver.org/spec/v2.0.0.html)
once it reaches a tagged release. Nothing has been released yet — the crate sits
at `0.1.0` with no tags — so everything below is unreleased, and dates refer to
when the change landed on `main`.

## [Unreleased]

### Changed

- **`set-curve` now refuses to run when SmartFanMode cannot be read.** *(2026-08-19)*

  Previously it logged a warning and attempted the curve write anyway. That is
  the worst option available, because the write requires switching the machine
  into Custom SmartFanMode — and **Custom mode with no curve loaded stops the
  fans and keeps them stopped under load.** Measured on a Legion 82RG: 0 RPM
  sustained across 61–67 °C with every CPU thread pinned, where the same machine
  held 2200 RPM in Performance.

  Proceeding without a readable mode enters that state with no recorded mode to
  return to, so neither the program nor the user can undo it. It now fails with:

  ```
  platform error: cannot read SmartFanMode, so there is no mode to restore if
  the curve write fails; refusing to enter Custom mode (Custom with no curve
  stops the fans)
  ```

  The machine is left on its BIOS curve, which is always safe.

- **A failed curve write no longer leaves the machine in Custom mode.**
  *(2026-08-19)*

  The mode switch and the table write were two separate fallible steps, so any
  failure of the write returned an error with the fans stopped and no
  indication. They now happen in a single PowerShell invocation whose `finally`
  restores the previous mode, backed by a Rust-side guard that unwinds on every
  early return and escalates to full speed if the restore itself fails.

  No change to a successful `set-curve`.

- **Breaking (CLI): `set-curve` now rejects a step 7 value of 0.** *(2026-08-12)*

  Custom curve validation gained a safety floor of 1 on step 7, alongside the
  existing floors of 3 on step 8 and 5 on step 9. Invocations such as
  `set-curve --steps "0,0,0,0,0,0,0,0,3,5"` were accepted before and now fail
  with:

  ```
  platform error: step 7 (approaching high temp) must be >= 1 for safety, got 0
  ```

  The floors match LenovoLegionToolkit's GodMode V1 minimum table,
  `[0,0,0,0,0,0,0,1,3,5]`. Steps 0–6 may still be 0.

  Explicit CLI input fails loudly rather than being adjusted, so that the user
  can see and fix it. Curves loaded from `fancontrol.json` are treated
  differently: the TUI raises them to meet the floors and warns, rather than
  refusing to start.

- **Fan RPM ranges are read from the firmware rather than inferred from the
  curve table.** *(2026-08-16)*

  The Lenovo backend derived each fan's RPM range from the span of
  `FanTable_Data`, which describes the curve rather than the fan. It now reads
  `CurrentFanMinSpeed` and `CurrentFanMaxSpeed` from `LENOVO_FAN_TABLE_DATA`.
  Both are 1600 and 4800 on the Legion 82RG, so there is no observable change on
  that hardware; models whose curve does not span the fan's full range get
  correct PWM conversion instead of an approximation.

- **Saved curves that fail the safety limits are adjusted in memory, never on
  disk.** *(2026-08-16)*

  `fancontrol.json` is authoritative. A saved curve outside the current limits
  is sanitized before being applied, and the adjustment is reported once per
  session with the path to the file, but the file is left unchanged until you
  save deliberately (`s` in the TUI).

  Writing the repair back was considered and rejected: sanitizing restores the
  non-decreasing invariant by raising, so `[5,4,3,2,1,1,1,1,3,5]` becomes
  `[5,5,5,5,5,5,5,5,5,5]` — near-full speed in every band. Overwriting a
  hand-edited file with a rewrite that drastic needs consent. The cost is that
  an unfixed config is adjusted again on every launch, which is visible and
  reversible where a silent overwrite would be neither.

### Added

- Custom fan curve support for Lenovo Legion via `Fan_Set_Table`, with config
  persistence to `fancontrol.json`.
- TUI dashboard (ratatui) with an interactive curve editor.
- GUI (egui/eframe) with per-fan sliders and SmartFanMode display.
- `tools/` for reusable Windows tooling, built on `tools/LenovoWmi.psm1`.

### Fixed

- An inverted RPM range no longer panics the PWM conversion. *(2026-08-16)*
  `pwm_to_rpm` computed `max_rpm - min_rpm` on `u32`, which underflows when
  `min > max` — reachable from a `TABLE|` line whose two speed fields parse
  unevenly. The conversions are now total; a degenerate range yields the
  conservative end.
- Curve sanitization in the TUI restores monotonicity by raising values only,
  clamps out-of-range steps, and is guaranteed to produce curves the validator
  accepts. Previously a saved config that violated the limits was dropped.
- `Get-LenovoBiosVersion` returned a null prefix on the unavailable path, where
  a hashtable lookup throws rather than missing. It now returns an empty string,
  with `Raw = $null` as the unavailable signal.

[Unreleased]: https://github.com/pjt222/fancontrol/commits/main
