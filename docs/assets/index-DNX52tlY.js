(function(){let e=document.createElement(`link`).relList;if(e&&e.supports&&e.supports(`modulepreload`))return;for(let e of document.querySelectorAll(`link[rel="modulepreload"]`))n(e);new MutationObserver(e=>{for(let t of e)if(t.type===`childList`)for(let e of t.addedNodes)e.tagName===`LINK`&&e.rel===`modulepreload`&&n(e)}).observe(document,{childList:!0,subtree:!0});function t(e){let t={};return e.integrity&&(t.integrity=e.integrity),e.referrerPolicy&&(t.referrerPolicy=e.referrerPolicy),e.crossOrigin===`use-credentials`?t.credentials=`include`:e.crossOrigin===`anonymous`?t.credentials=`omit`:t.credentials=`same-origin`,t}function n(e){if(e.ep)return;e.ep=!0;let n=t(e);fetch(e.href,n)}})(),document.querySelector(`#app`).innerHTML=`
<div class="viridis-bar"></div>

<section class="hero">
  <div class="container">
    <h1><span class="accent">fancontrol</span></h1>
    <p class="tagline">
      Minimal cross-platform fan speed control for Linux and Windows.
      CLI, TUI dashboard, and GUI &mdash; all in one binary.
    </p>
    <div class="hero-demo">
      <div class="placeholder">
        <span style="color: var(--v-5)">Fan Control TUI</span> &mdash; SmartFanMode: Balanced

        &gt; CPU Fan    2100 RPM    BIOS auto
          GPU Fan    1800 RPM    BIOS auto

        Curve: CPU Fan &gt; Sensor 3 (Active)
        <span style="color: var(--v-0)">█</span><span style="color: var(--v-1)">█</span><span style="color: var(--v-2)">█</span><span style="color: var(--v-3)">█</span><span style="color: var(--v-4)">█</span><span style="color: var(--v-5)">█</span><span style="color: var(--v-6)">█</span><span style="color: var(--v-7)">█</span><span style="color: var(--v-8)">█</span><span style="color: var(--v-9)">█</span><span style="color: var(--v-10)">█</span>  viridis gradient
      </div>
    </div>
    <div class="hero-buttons">
      <a href="https://github.com/pjt222/fancontrol" class="btn btn-primary">
        View on GitHub
      </a>
      <a href="#quickstart" class="btn btn-secondary">
        Quickstart
      </a>
    </div>
  </div>
</section>

<section class="features">
  <div class="container">
    <h2>Features</h2>
    <div class="feature-grid">
      <div class="feature-card">
        <span class="icon" style="color: var(--v-5)">⚙</span>
        <h3>Custom Fan Curves</h3>
        <p>Define 10-point temperature-to-speed curves via the EC's Fan_Set_Table.
           Safety minimums enforced at high temperatures.</p>
      </div>
      <div class="feature-card">
        <span class="icon" style="color: var(--v-7)">███</span>
        <h3>TUI Dashboard</h3>
        <p>Interactive terminal UI with real-time fan speeds, viridis-colored curve editor,
           and keyboard-driven controls. No GUI needed.</p>
      </div>
      <div class="feature-card">
        <span class="icon" style="color: var(--v-4)">⚡</span>
        <h3>CLI + JSON Output</h3>
        <p>Scriptable command-line interface with <code>--json</code> output.
           List, get, set, monitor, and configure fan curves.</p>
      </div>
      <div class="feature-card">
        <span class="icon" style="color: var(--v-8)">🖥</span>
        <h3>GUI (egui)</h3>
        <p>Native desktop interface built with egui/eframe.
           Per-fan sliders, curve editor, and SmartFanMode display.</p>
      </div>
      <div class="feature-card">
        <span class="icon" style="color: var(--v-3)">💾</span>
        <h3>Config Persistence</h3>
        <p>Save custom curves to <code>fancontrol.json</code>.
           Auto-reapplied on startup &mdash; survives reboots.</p>
      </div>
      <div class="feature-card">
        <span class="icon" style="color: var(--v-6)">🌐</span>
        <h3>Cross-Platform</h3>
        <p>Linux via sysfs/hwmon, Windows via WMI.
           Lenovo Legion laptops get full curve control via vendor WMI.</p>
      </div>
    </div>
  </div>
</section>

<section class="quickstart" id="quickstart">
  <div class="container">
    <h2>Quickstart</h2>
    <div class="code-block">
      <span class="label">Build from source</span>
      <pre><span class="comment"># Clone and build</span>
<span class="cmd">git clone</span> https://github.com/pjt222/fancontrol.git
<span class="cmd">cd</span> fancontrol
<span class="cmd">cargo build</span> --release

<span class="comment"># Run the TUI dashboard (admin/root required)</span>
<span class="cmd">./target/release/fancontrol</span> tui</pre>
    </div>
    <div class="code-block">
      <span class="label">CLI examples</span>
      <pre><span class="cmd">fancontrol</span> list              <span class="comment"># List detected fans</span>
<span class="cmd">fancontrol</span> get 0              <span class="comment"># Get fan 0 speed</span>
<span class="cmd">fancontrol</span> monitor            <span class="comment"># Live monitoring</span>
<span class="cmd">fancontrol</span> table              <span class="comment"># Show EC fan curves</span>
<span class="cmd">fancontrol</span> tui                <span class="comment"># Interactive dashboard</span>
<span class="cmd">fancontrol</span> gui                <span class="comment"># Desktop GUI</span>

<span class="comment"># Set a custom fan curve (Lenovo)</span>
<span class="cmd">fancontrol</span> set-curve --fan-id 0 --sensor-id 3 \\
  --steps 0,0,0,1,2,4,6,7,8,10 --save</pre>
    </div>
  </div>
</section>

<section class="architecture">
  <div class="container">
    <h2>Architecture</h2>
    <div class="arch-diagram">
      <div class="arch-box wide">
        <div class="title">CLI / TUI / GUI</div>
        <div class="detail">clap &bull; ratatui &bull; egui/eframe</div>
      </div>
      <div class="arch-arrow">↓</div>
      <div class="arch-box wide">
        <div class="title">FanController Trait</div>
        <div class="detail">Platform abstraction &bull; discover / get / set / curves</div>
      </div>
      <div class="arch-arrow">↓</div>
      <div class="arch-box">
        <div class="title">Linux</div>
        <div class="detail">sysfs/hwmon</div>
      </div>
      <div class="arch-box">
        <div class="title">Windows</div>
        <div class="detail">WMI Win32_Fan</div>
      </div>
      <div class="arch-box">
        <div class="title">Lenovo</div>
        <div class="detail">LENOVO_FAN_METHOD</div>
      </div>
    </div>
  </div>
</section>

<footer class="footer">
  <div class="container">
    <p>
      <a href="https://github.com/pjt222/fancontrol">fancontrol</a>
      &mdash; MIT License &mdash; Built with Rust
    </p>
  </div>
</footer>

<div class="viridis-bar"></div>
`;