/* =========================================================
   Rustyfi — App JavaScript
   ========================================================= */

// ---------------------------------------------------------------------------
// Background canvas — animated grid + particles
// ---------------------------------------------------------------------------
const REDUCED_MOTION = window.matchMedia('(prefers-reduced-motion: reduce)').matches;

(function initCanvas() {
  const canvas = document.getElementById('bg-canvas');
  const ctx    = canvas.getContext('2d');
  let W, H, particles = [];

  function resize() {
    W = canvas.width  = window.innerWidth;
    H = canvas.height = window.innerHeight;
  }

  function mkParticle() {
    return {
      x:  Math.random() * W,
      y:  Math.random() * H,
      vx: (Math.random() - 0.5) * 0.18,
      vy: (Math.random() - 0.5) * 0.18,
      r:  Math.random() * 1.5 + 0.4,
      a:  Math.random() * 0.4 + 0.1,
    };
  }

  function initParticles(n = 80) {
    particles = Array.from({ length: n }, mkParticle);
  }

  function drawGrid() {
    ctx.strokeStyle = 'rgba(255,107,53,0.04)';
    ctx.lineWidth   = 1;
    const step = 60;
    for (let x = 0; x < W; x += step) {
      ctx.beginPath(); ctx.moveTo(x, 0); ctx.lineTo(x, H); ctx.stroke();
    }
    for (let y = 0; y < H; y += step) {
      ctx.beginPath(); ctx.moveTo(0, y); ctx.lineTo(W, y); ctx.stroke();
    }
  }

  function drawOnce() {
    ctx.clearRect(0, 0, W, H);
    drawGrid();

    for (const p of particles) {
      p.x += p.vx; p.y += p.vy;
      if (p.x < 0) p.x = W;
      if (p.x > W) p.x = 0;
      if (p.y < 0) p.y = H;
      if (p.y > H) p.y = 0;

      ctx.beginPath();
      ctx.arc(p.x, p.y, p.r, 0, Math.PI * 2);
      ctx.fillStyle = `rgba(255,107,53,${p.a})`;
      ctx.fill();
    }
  }

  function frame() {
    // Skip work while the tab is hidden — no point burning battery.
    if (!document.hidden) drawOnce();
    requestAnimationFrame(frame);
  }

  window.addEventListener('resize', () => {
    resize();
    initParticles();
    if (REDUCED_MOTION) drawOnce();
  });
  resize();
  initParticles();
  if (REDUCED_MOTION) {
    drawOnce(); // a single static frame — no motion
  } else {
    frame();
  }
})();

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------
const API_BASE = window.location.origin;

const state = {
  file:        null,
  running:     false,
  zipData:     null,
  crateName:   'app',
  totalFiles:  0,
  doneFiles:   0,   // incremented only on file_complete — ground truth
  startTime:   null,
  elapsedTimer: null,
  translateStartTime: null, // when Translating phase began (for ETA)
  abortController: null,    // aborts the in-flight translate fetch on reset
  generation:  0,           // bumped on reset so late SSE events are ignored
};

// ---------------------------------------------------------------------------
// DOM refs
// ---------------------------------------------------------------------------
const dropzone     = document.getElementById('dropzone');
const fileInput    = document.getElementById('file-input');
const btnTranslate = document.getElementById('btn-translate');
const btnDownload  = document.getElementById('btn-download');
const btnReset     = document.getElementById('btn-reset');
const progressWrap = document.getElementById('progress-wrap');
const progressFill = document.getElementById('progress-bar-fill');
const progressRole = document.getElementById('progress-bar-role');
const progressLbl  = document.getElementById('progress-label');
const fileInd      = document.getElementById('file-indicator');
const fiText       = document.getElementById('fi-text');
const termBody     = document.getElementById('terminal-body');
const statFiles    = document.getElementById('stat-files');
const statProgress = document.getElementById('stat-progress');
const statSize     = document.getElementById('stat-size');
const statElapsed  = document.getElementById('stat-elapsed');

// ---------------------------------------------------------------------------
// Terminal helpers
// ---------------------------------------------------------------------------
// Only autoscroll when the user is already at the bottom — yanking them
// down while they're reading an earlier error is infuriating.
function isPinnedToBottom() {
  return termBody.scrollHeight - termBody.scrollTop - termBody.clientHeight < 40;
}

function log(msg, cls = 't-ok') {
  const pinned = isPinnedToBottom();
  const p = document.createElement('p');
  p.className = `t-line ${cls}`;
  p.textContent = `> ${msg}`;
  termBody.appendChild(p);
  if (pinned) termBody.scrollTop = termBody.scrollHeight;
}

function logRaw(msg, cls = 't-ok') {
  const pinned = isPinnedToBottom();
  const p = document.createElement('p');
  p.className = `t-line ${cls}`;
  p.textContent = msg;
  termBody.appendChild(p);
  if (pinned) termBody.scrollTop = termBody.scrollHeight;
}

// Throttled screen-reader status (the terminal + spinner would otherwise
// announce hundreds of times per run).
const srStatus = document.getElementById('sr-status');
function announce(msg) {
  if (srStatus) srStatus.textContent = msg;
}

// ---------------------------------------------------------------------------
// Elapsed timer
// ---------------------------------------------------------------------------
function startElapsed() {
  state.startTime = Date.now();
  if (state.elapsedTimer) clearInterval(state.elapsedTimer);
  state.elapsedTimer = setInterval(() => {
    const secs = Math.floor((Date.now() - state.startTime) / 1000);
    const m = String(Math.floor(secs / 60)).padStart(2, '0');
    const s = String(secs % 60).padStart(2, '0');
    statElapsed.textContent = `${m}:${s}`;
  }, 1000);
}

function stopElapsed() {
  if (state.elapsedTimer) {
    clearInterval(state.elapsedTimer);
    state.elapsedTimer = null;
  }
}

// ---------------------------------------------------------------------------
// Stage tracker
// ---------------------------------------------------------------------------
const STAGE_ORDER = ['Idle','Parsing','Scaffolding','Translating','Verifying','Optimizing','Completed','Failed'];
let currentStageIndex = 0;

// ---------------------------------------------------------------------------
// Heartbeat — shows the pipeline is alive between slow LLM events
// ---------------------------------------------------------------------------
let heartbeatTimer = null;
let lastEventAt = null;
const DOTS = ['⠋','⠙','⠹','⠸','⠼','⠴','⠦','⠧','⠇','⠏'];
let dotIdx = 0;

function startHeartbeat() {
  lastEventAt = Date.now();
  if (heartbeatTimer) clearInterval(heartbeatTimer);
  heartbeatTimer = setInterval(() => {
    if (!state.running) return;
    const age = Math.floor((Date.now() - (lastEventAt ?? Date.now())) / 1000);
    const dot = DOTS[dotIdx++ % DOTS.length];
    let ageStr = '';
    if (age > 120)    ageStr = ` (${age}s — still waiting on the model; big files take a while)`;
    else if (age > 4) ageStr = ` (${age}s ago)`;
    const currentFile = fiText.textContent.replace(/ ⠋.*| ⠙.*| ⠹.*| ⠸.*| ⠼.*| ⠴.*| ⠦.*| ⠧.*| ⠇.*| ⠏.*/, '');
    fiText.textContent = `${currentFile} ${dot}${ageStr}`;
  }, 300);
}

function stopHeartbeat() {
  if (heartbeatTimer) { clearInterval(heartbeatTimer); heartbeatTimer = null; }
}

function pingHeartbeat() {
  lastEventAt = Date.now();
  dotIdx = 0;
}

function setStage(name) {
  const idx = STAGE_ORDER.indexOf(name);
  if (idx === -1) return;

  for (let i = 0; i < STAGE_ORDER.length; i++) {
    const el = document.getElementById(`stage-${STAGE_ORDER[i]}`);
    if (!el) continue;
    el.classList.remove('active', 'done', 'failed');
    if (name === 'Failed') {
      if (i < currentStageIndex) el.classList.add('done');
      if (STAGE_ORDER[i] === 'Failed') el.classList.add('active', 'failed');
    } else {
      if (i < idx)  el.classList.add('done');
      if (i === idx) el.classList.add('active');
    }
  }
  currentStageIndex = idx;
}

// ---------------------------------------------------------------------------
// Progress helpers
// ---------------------------------------------------------------------------
function setProgress(pct, label) {
  progressFill.style.width = `${pct}%`;
  progressRole.setAttribute('aria-valuenow', pct);
  if (label) progressLbl.textContent = label;
  statProgress.textContent = `${Math.round(pct)}%`;
}

function showProgress(label = 'Starting…') {
  progressWrap.hidden = false;
  setProgress(0, label);
}

function setFileIndicator(text) {
  fileInd.hidden = false;
  fiText.textContent = text;
}

function hideFileIndicator() {
  fileInd.hidden = true;
}

// ---------------------------------------------------------------------------
// File selection
// ---------------------------------------------------------------------------
dropzone.addEventListener('click', () => {
  if (!state.running) fileInput.click();
});

dropzone.addEventListener('keydown', (e) => {
  if ((e.key === 'Enter' || e.key === ' ') && !state.running) {
    e.preventDefault();
    fileInput.click();
  }
});

fileInput.addEventListener('change', () => {
  if (fileInput.files[0]) selectFile(fileInput.files[0]);
});

dropzone.addEventListener('dragover', (e) => {
  e.preventDefault();
  if (!state.running) dropzone.classList.add('drag-over');
});

dropzone.addEventListener('dragleave', () => dropzone.classList.remove('drag-over'));

dropzone.addEventListener('drop', (e) => {
  e.preventDefault();
  dropzone.classList.remove('drag-over');
  if (state.running) return;

  // Dropped a folder? Browsers expose it as a File with no type — catch it
  // with a friendly hint instead of uploading garbage.
  const entry = e.dataTransfer.items?.[0]?.webkitGetAsEntry?.();
  if (entry && entry.isDirectory) {
    log(`"${entry.name}" looks like a folder — compress it to a .zip first, then drop that in.`, 't-warn');
    return;
  }

  const files = Array.from(e.dataTransfer.files);
  if (files.length === 0) return;
  // Prefer the first .zip if several files were dropped together.
  const f = files.find(x => x.name.toLowerCase().endsWith('.zip')) ?? files[0];
  if (files.length > 1) {
    log(`One at a time! Taking ${f.name} — drop the next one when this run finishes.`, 't-warn');
  }
  selectFile(f);
});

function selectFile(f) {
  if (!f.name.toLowerCase().endsWith('.zip')) {
    log(`Please drop a .zip file (got: ${f.name})`, 't-warn');
    return;
  }
  state.file      = f;
  state.crateName = f.name.replace(/\.zip$/i, '').replace(/[^a-z0-9_]/gi, '_').toLowerCase();

  const inner = dropzone.querySelector('.dropzone-inner');
  inner.querySelector('.drop-title').textContent = f.name;
  inner.querySelector('.drop-sub').textContent   = `${(f.size / 1024).toFixed(1)} KB · Ready to translate`;

  log(`Selected: ${f.name} (${(f.size / 1024).toFixed(1)} KB)`, 't-info');
  btnTranslate.hidden = false;
  btnReset.hidden     = false;
}

// ---------------------------------------------------------------------------
// Translation
// ---------------------------------------------------------------------------
btnTranslate.addEventListener('click', startTranslation);

// Shared cleanup for upload-phase failures: stop the run visuals, show the
// message, and let the user retry without re-selecting the file.
function failUpload(message) {
  state.running = false;
  stopElapsed();
  stopHeartbeat();
  hideFileIndicator();
  setStage('Failed');
  progressWrap.hidden = true;
  log(message, 't-err');
  announce(message);
  btnTranslate.hidden = false; // same file is still selected — retry is one click
  btnReset.hidden     = false;
}

async function startTranslation() {
  if (!state.file || state.running) return;
  state.running    = true;
  state.zipData    = null;
  state.doneFiles  = 0;
  const generation = state.generation;

  btnTranslate.hidden = true;
  btnDownload.hidden  = true;
  const banner = document.getElementById('result-banner');
  if (banner) banner.hidden = true;
  const compEl = document.getElementById('stage-Completed');
  if (compEl) compEl.classList.remove('completed-warn');
  showProgress('Uploading…');
  setStage('Parsing');
  startElapsed();
  startHeartbeat();
  setFileIndicator('Uploading & analysing…');
  log('Uploading archive to Rustyfi server…', 't-info');
  announce('Translation started');

  const form = new FormData();
  form.append('archive', state.file, state.file.name);

  state.abortController = new AbortController();
  let response;
  try {
    response = await fetch(`${API_BASE}/api/translate`, {
      method: 'POST',
      body: form,
      signal: state.abortController.signal,
    });
  } catch (err) {
    if (err.name === 'AbortError') return; // user hit Start Over
    failUpload(`Couldn't reach the Rustyfi server (${err.message}). Is it running? Start it with: cargo run -p rustyfi-server`);
    return;
  }

  if (!response.ok) {
    let msg = `Server error ${response.status}`;
    try {
      const body = await response.json();
      if (body && body.error) msg = body.error;
    } catch { /* non-JSON body, keep generic message */ }
    failUpload(msg);
    return;
  }

  // SSE stream
  const reader  = response.body.getReader();
  const decoder = new TextDecoder();
  let buf = '';
  let sawTerminal = false;

  try {
    while (true) {
      const { done, value } = await reader.read();
      if (done) break;

      buf += decoder.decode(value, { stream: true });
      const lines = buf.split('\n');
      buf = lines.pop() ?? '';

      for (const line of lines) {
        const trimmed = line.trim();
        if (!trimmed.startsWith('data:')) continue;
        const json = trimmed.slice('data:'.length).trim();
        if (!json) continue;
        try {
          const event = JSON.parse(json);
          if (event.kind === 'done' || event.kind === 'failed') sawTerminal = true;
          if (state.generation === generation) {
            handleProgress(event);
            pingHeartbeat();
          }
        } catch { /* ignore malformed */ }
      }
    }
  } catch (err) {
    if (err.name === 'AbortError') return;
    // fall through to the dead-stream check below
  }

  // The stream ended without a done/failed event — server crashed or the
  // connection dropped. Never leave the spinner running forever.
  if (!sawTerminal && state.generation === generation && state.running) {
    onFailed('Lost the connection to the server mid-run. Your progress is checkpointed — drop the same ZIP and hit Translate to resume where it left off.');
  }
}

// ---------------------------------------------------------------------------
// SSE event handler
// ---------------------------------------------------------------------------
function handleProgress(event) {
  switch (event.kind) {
    case 'state_changed':
      setStage(event.state);
      if (event.state !== 'Translating') {
        // Only snap to stage % outside translation — during translation
        // the bar is driven by real file count below.
        setProgress(stagePercent(event.state), event.state === 'Optimizing' ? 'Packaging…' : event.state);
      }
      if (event.state === 'Translating') {
        state.translateStartTime = Date.now();
        setFileIndicator('Warming up the translators…');
      }
      log(`→ ${event.state}`, 't-info');
      announce(`Stage: ${event.state}`);
      break;

    case 'phase_resumed': {
      // A previous run for this exact ZIP left a checkpoint — celebrate it.
      log(`⏩ Resumed from checkpoint: ${event.phase}`, 't-ok');
      const phaseStage = {
        analysis: 'Scaffolding', scaffold: 'Translating',
        verification: 'Optimizing', packaging: 'Completed',
      };
      const stageKey = String(event.phase).split(' ')[0];
      if (phaseStage[stageKey]) setStage(phaseStage[stageKey]);
      if (stageKey === 'translation') {
        setStage('Translating');
        state.translateStartTime = Date.now();
        // "translation (file N)" → N files already done on the server side.
        const m = String(event.phase).match(/file (\d+)/);
        if (m) {
          state.doneFiles = parseInt(m[1], 10);
          statFiles.textContent = `${state.doneFiles} / ${state.totalFiles || '?'}`;
        }
      }
      break;
    }

    case 'note':
      log(event.message, 't-warn');
      break;

    case 'file_started': {
      if (event.total > 0) state.totalFiles = event.total;
      const shortName = event.file.split('/').pop();
      setFileIndicator(`Translating ${shortName}`);
      if (state.totalFiles < 500 || state.doneFiles % 100 === 0) {
        log(`[${state.doneFiles + 1}/${state.totalFiles}] ${shortName}`, 't-ok');
      }
      // Real file-based progress: 10% (scaffold done) → 70% (verify start)
      const pct = state.totalFiles > 0
        ? 10 + (state.doneFiles / state.totalFiles) * 60
        : 10;
      // ETA: files/sec from translation start
      let etaLabel = shortName;
      if (state.translateStartTime && state.doneFiles > 0) {
        const elapsedSec = (Date.now() - state.translateStartTime) / 1000;
        const fps = state.doneFiles / elapsedSec;
        const remaining = state.totalFiles - state.doneFiles;
        const etaSec = Math.round(remaining / fps);
        const em = String(Math.floor(etaSec / 60)).padStart(2, '0');
        const es = String(etaSec % 60).padStart(2, '0');
        const fpsStr = fps >= 1 ? `${fps.toFixed(1)} f/s` : `${(fps * 60).toFixed(1)} f/min`;
        statElapsed.title = `${fpsStr} · ETA ${em}:${es}`;
        etaLabel = `${state.doneFiles}/${state.totalFiles} · ETA ${em}:${es}`;
      }
      setProgress(pct, etaLabel);
      break;
    }

    case 'file_complete':
      state.doneFiles++;
      statFiles.textContent = `${state.doneFiles} / ${state.totalFiles}`;
      if (state.doneFiles % 25 === 0) {
        announce(`Translated ${state.doneFiles} of ${state.totalFiles} files`);
      }
      // Real % update on every completion
      if (state.totalFiles > 0) {
        const pct = 10 + (state.doneFiles / state.totalFiles) * 60;
        setProgress(pct);
      }
      if (state.totalFiles >= 500) {
        if (state.doneFiles % 100 === 0) {
          // compute files/sec
          let fpsInfo = '';
          if (state.translateStartTime) {
            const elapsedSec = (Date.now() - state.translateStartTime) / 1000;
            const fps = state.doneFiles / elapsedSec;
            const remaining = state.totalFiles - state.doneFiles;
            const etaSec = Math.round(remaining / fps);
            const em = String(Math.floor(etaSec / 60)).padStart(2, '0');
            const es = String(etaSec % 60).padStart(2, '0');
            fpsInfo = ` · ${fps.toFixed(1)} f/s · ETA ${em}:${es}`;
          }
          logRaw(`  ✓ ${state.doneFiles}/${state.totalFiles} files done${fpsInfo}`, 't-dim');
        }
      } else {
        logRaw(`  ✓ ${event.file.split('/').pop()}`, 't-dim');
      }
      break;

    case 'compiler_error':
      log('Compiler errors detected — entering fix loop…', 't-warn');
      // Show first 3 error lines from the actual output
      if (event.message) {
        const errorLines = event.message
          .split('\n')
          .filter(l => l.includes('error[') || l.includes('error:'))
          .slice(0, 4);
        for (const el of errorLines) {
          logRaw(`  ${el.trim()}`, 't-err');
        }
      }
      break;

    case 'fix_cycle':
      log(`Fix cycle ${event.attempt}…`, 't-warn');
      setProgress(65 + event.attempt * 5, `Fix cycle ${event.attempt}`);
      break;

    case 'done':
      onDone(event);
      break;

    case 'failed':
      onFailed(event.reason);
      break;
  }
}

function stagePercent(stage) {
  const map = {
    Idle: 0, Parsing: 5, Scaffolding: 10,
    Translating: 15, Verifying: 70, Optimizing: 88, Completed: 100,
  };
  return map[stage] ?? 50;
}

// ---------------------------------------------------------------------------
// Done / Failed
// ---------------------------------------------------------------------------
function onDone(event) {
  state.running = false;
  stopElapsed();
  stopHeartbeat();
  hideFileIndicator();

  const zipBytes  = event.zip_bytes ?? 0;
  const clean     = event.cargo_clean === true;
  const errors    = event.error_count ?? 0;
  const todos     = event.todo_count ?? 0;
  const failed    = event.files_failed ?? 0;
  const done      = event.files_translated ?? 0;
  // The server's canonical crate name — using our client-side guess breaks
  // downloads for names the two sides sanitise differently.
  if (event.crate_name) state.crateName = event.crate_name;

  // Three honest outcomes, not one fake "success".
  let tier; // 'clean' | 'partial' | 'rough'
  if (clean && failed === 0 && todos === 0)      tier = 'clean';
  else if (clean)                                 tier = 'partial';
  else                                            tier = 'rough';

  // Stage strip: only a green check when it actually compiles; otherwise the
  // Completed dot turns amber so "done" never masquerades as "working".
  setStage('Completed');
  const compEl = document.getElementById('stage-Completed');
  if (compEl) compEl.classList.toggle('completed-warn', !clean);
  setProgress(100, clean ? 'Compiles ✓' : 'Translated — needs work');
  statSize.textContent = `${(zipBytes / 1024).toFixed(1)} KB`;

  renderResultBanner({ tier, errors, todos, failed, done, zipBytes });

  // Terminal stays honest too.
  if (tier === 'clean') {
    log('Translation complete — cargo check is clean ✓ 🎺🦀', 't-ok');
  } else if (tier === 'partial') {
    log(`Compiles ✓ — but ${todos} todo!() gap(s) / ${failed} stub(s) need you. See NEXT_STEPS.md.`, 't-warn');
  } else {
    log(`Translated, but it does NOT compile yet: ${errors} cargo error(s), ${todos} todo!() gap(s).`, 't-err');
    log('Open NEXT_STEPS.md in the download for the exact fix list.', 't-info');
  }
  announce(clean ? 'Done. The crate compiles. Download ready.'
                 : `Done, but the crate needs work: ${errors} errors. Download ready.`);

  btnDownload.hidden = false;
  btnDownload.classList.toggle('btn-success', clean);
  btnDownload.classList.toggle('btn-warn', !clean);
  btnDownload.innerHTML = clean
    ? '<span class="btn-icon" aria-hidden="true">⬇</span> Download Rust Project'
    : '<span class="btn-icon" aria-hidden="true">⬇</span> Download (needs work)';
  btnDownload.onclick = downloadResult;
  btnReset.hidden = false;

  if (tier === 'clean') crabParade(20);

  loadHistory();
}

// Paint the at-a-glance result banner from the structured Done event.
function renderResultBanner({ tier, errors, todos, failed, done, zipBytes }) {
  const banner = document.getElementById('result-banner');
  const icon   = document.getElementById('result-icon');
  const title  = document.getElementById('result-title');
  const sub    = document.getElementById('result-sub');
  const chips  = document.getElementById('result-chips');
  if (!banner) return;

  banner.classList.remove('rb-clean', 'rb-partial', 'rb-rough');
  const cfg = {
    clean:   { cls: 'rb-clean',   icon: '✓',  title: 'Compiles cleanly',
               sub: `${done} file(s) translated · cargo check passed. Ready to build.` },
    partial: { cls: 'rb-partial', icon: '◑',  title: 'Compiles — with gaps to fill',
               sub: `It builds, but ${todos} todo!() / ${failed} stub(s) need you. See NEXT_STEPS.md.` },
    rough:   { cls: 'rb-rough',   icon: '⚠',  title: 'Translated — does not compile yet',
               sub: `A starting point: ${errors} cargo error(s) remain. NEXT_STEPS.md lists the fixes.` },
  }[tier];

  banner.classList.add(cfg.cls);
  icon.textContent  = cfg.icon;
  title.textContent = cfg.title;
  sub.textContent   = cfg.sub;

  const chip = (label, val, kind) =>
    `<span class="rb-chip rb-${kind}"><b>${val}</b> ${label}</span>`;
  chips.innerHTML = [
    chip('translated', done, 'ok'),
    errors > 0 ? chip('cargo errors', errors, 'bad') : chip('cargo check', 'clean', 'ok'),
    todos  > 0 ? chip('todo!() gaps', todos, 'warn') : '',
    failed > 0 ? chip('failed files', failed, 'bad') : '',
    chip('ZIP', `${(zipBytes / 1024).toFixed(0)}KB`, 'dim'),
  ].filter(Boolean).join('');

  banner.hidden = false;
}

function onFailed(reason) {
  state.running = false;
  stopElapsed();
  stopHeartbeat();
  hideFileIndicator();
  setStage('Failed');
  log(`Failed: ${friendlyReason(reason)}`, 't-err');
  log('womp womp 🎺 — your progress is checkpointed; drop the same ZIP to resume.', 't-dim');
  announce(`Translation failed: ${reason}`);
  sadTrombone();
  btnReset.hidden = false;
  btnTranslate.hidden = !state.file; // retry without re-selecting
}

// Translate raw engine errors into something a human can act on.
function friendlyReason(reason) {
  const r = String(reason ?? '');
  if (r.includes('no translatable source files')) {
    return 'No translatable source files in that ZIP. Rustyfi looks for .py .ts .js .go .c .cpp .java .cs .rb — make sure you zipped the project folder itself, not a build output.';
  }
  if (r.includes('RUSTYFI_LLM_API_KEY')) {
    return 'The server has no LLM API key. Set RUSTYFI_LLM_API_KEY on the server, restart it, then hit Translate again.';
  }
  return r;
}

async function downloadResult() {
  try {
    const resp = await fetch(`${API_BASE}/api/download/${state.crateName}`, { method: 'GET' });
    if (!resp.ok) {
      log('Download endpoint not ready yet — try again in a moment.', 't-warn');
      return;
    }
    const blob = await resp.blob();
    const url  = URL.createObjectURL(blob);
    const a    = document.createElement('a');
    a.href     = url;
    a.download = `${state.crateName}_rust.zip`;
    a.click();
    URL.revokeObjectURL(url);
  } catch (err) {
    log(`Download error: ${err.message}`, 't-err');
  }
}

// ---------------------------------------------------------------------------
// Reset
// ---------------------------------------------------------------------------
btnReset.addEventListener('click', () => {
  // Abort the in-flight upload/stream and ignore any stragglers — otherwise
  // the old run keeps writing into the freshly-reset terminal.
  state.generation++;
  if (state.abortController) {
    state.abortController.abort();
    state.abortController = null;
  }

  state.file       = null;
  state.running    = false;
  state.zipData    = null;
  state.doneFiles  = 0;
  state.totalFiles = 0;

  const inner = dropzone.querySelector('.dropzone-inner');
  inner.querySelector('.drop-title').textContent = 'Drop Your App Here';
  inner.querySelector('.drop-sub').textContent   = 'ZIP your project folder and drag it in — or click to browse';

  setStage('Idle');
  const compEl = document.getElementById('stage-Completed');
  if (compEl) compEl.classList.remove('completed-warn');
  const banner = document.getElementById('result-banner');
  if (banner) banner.hidden = true;
  btnDownload.classList.remove('btn-warn');
  progressWrap.hidden = true;
  setProgress(0, '');
  hideFileIndicator();
  stopElapsed();
  stopHeartbeat();
  btnTranslate.hidden = true;
  btnDownload.hidden  = true;
  btnReset.hidden     = true;
  statFiles.textContent    = '—';
  statProgress.textContent = '0%';
  statSize.textContent     = '—';
  statElapsed.textContent  = '—';

  termBody.innerHTML = '<p class="t-line t-dim">Rustyfi v0.1.0 — ready</p>';
  fileInput.value = '';
  log('Reset. Drop a new app to begin.', 't-dim');
});

// ---------------------------------------------------------------------------
// Panel tabs
// ---------------------------------------------------------------------------
window.switchTab = function(tab) {
  document.getElementById('panel-mine').hidden      = (tab !== 'mine');
  document.getElementById('panel-community').hidden = (tab !== 'community');
  document.getElementById('panel-provider').hidden  = (tab !== 'provider');
  document.getElementById('tab-mine').classList.toggle('active',      tab === 'mine');
  document.getElementById('tab-community').classList.toggle('active', tab === 'community');
  document.getElementById('tab-provider').classList.toggle('active',  tab === 'provider');
  if (tab === 'community') loadCommunity();
  if (tab === 'provider')  loadProviderStatus();
};

// ---------------------------------------------------------------------------
// History panel
// ---------------------------------------------------------------------------
async function loadHistory() {
  const list = document.getElementById('history-list');
  const pip  = document.getElementById('history-pip');
  try {
    const resp = await fetch(`${API_BASE}/api/history`);
    if (!resp.ok) throw new Error('history fetch failed');
    const entries = await resp.json();

    if (!entries || entries.length === 0) {
      list.innerHTML = `
        <div class="panel-empty-wrap">
          <div class="panel-empty-icon">🎺</div>
          <p class="panel-empty-title">Nothing yet</p>
          <p class="panel-empty-sub">Drop a project and hit Translate — it'll show up here when it's done.</p>
        </div>`;
      if (pip) pip.hidden = true;
      return;
    }

    // Show the live pip
    if (pip) pip.hidden = false;

    list.innerHTML = entries.map(e => {
      const date    = new Date(e.timestamp * 1000);
      const dateStr = date.toLocaleDateString(undefined, { month: 'short', day: 'numeric', hour: '2-digit', minute: '2-digit' });
      const kb      = (e.zip_bytes / 1024).toFixed(1);
      const lang    = escHtml(e.language || 'rust');
      const name    = escHtml(e.original_name);
      const pct     = Math.min(100, Math.max(6, Math.round((e.zip_bytes / 200000) * 100)));
      return `
        <div class="project-card">
          <div class="project-card-header">
            <span class="project-card-name" title="${name}">${name}</span>
            <span class="lang-badge">${lang}</span>
          </div>
          <div class="project-card-meta">
            <span>📄 ${e.files} files</span>
            <span>📦 ${kb} KB</span>
          </div>
          <div class="project-stat-bar"><div class="project-stat-bar-fill" style="width:${pct}%"></div></div>
          <div class="project-card-actions">
            <a class="project-card-dl"
               href="/api/download/${encodeURIComponent(e.crate_name)}"
               download="${escHtml(e.crate_name)}_rust.zip">
              ⬇ Download
            </a>
            <span class="project-card-time">${dateStr}</span>
          </div>
        </div>`;
    }).join('');
  } catch {
    list.innerHTML = '<p class="panel-empty">Could not load history.</p>';
  }
}

// ---------------------------------------------------------------------------
// Community panel
// ---------------------------------------------------------------------------
async function loadCommunity() {
  const list = document.getElementById('community-list');
  list.innerHTML = '<p class="panel-empty">Loading…</p>';
  try {
    const resp = await fetch(`${API_BASE}/api/community`);
    if (!resp.ok) throw new Error('community fetch failed');
    const entries = await resp.json();

    if (!entries || entries.length === 0) {
      list.innerHTML = `
        <div class="panel-empty-wrap">
          <div class="panel-empty-icon">🌍</div>
          <p class="panel-empty-title">No projects yet</p>
          <p class="panel-empty-sub">Be the first to submit a translated project!</p>
        </div>`;
      return;
    }

    const LANG_EMOJI = { go: '🐹', python: '🐍', typescript: '🛠', javascript: '🛠', java: '☕', ruby: '💎', c: '⚡', cpp: '⚡', csharp: '🎵' };

    list.innerHTML = entries.map(e => {
      const lang   = (e.language || 'go').toLowerCase();
      const emoji  = LANG_EMOJI[lang] || '📦';
      const name   = escHtml(e.name);
      const desc   = escHtml(e.description || '');
      const by     = escHtml(e.submitted_by || 'anon');
      return `
        <div class="community-card">
          <div class="community-card-top">
            <div class="community-avatar">${emoji}</div>
            <span class="community-card-name" title="${name}">${name}</span>
            <span class="community-lang-badge">${escHtml(lang)}</span>
          </div>
          <p class="community-desc">${desc}</p>
          <div class="community-card-footer">
            ${e.github ? `<a class="community-gh-btn" href="${escHtml(e.github)}" target="_blank" rel="noopener">↗ GitHub</a>` : ''}
            <span class="community-by">by ${by}</span>
          </div>
        </div>`;
    }).join('');
  } catch {
    list.innerHTML = '<p class="panel-empty">Could not load community projects.</p>';
  }
}

function escHtml(str) {
  return String(str ?? '').replace(/&/g,'&amp;').replace(/</g,'&lt;').replace(/>/g,'&gt;').replace(/"/g,'&quot;');
}

// ---------------------------------------------------------------------------
// Provider status
// ---------------------------------------------------------------------------
async function loadProviderStatus() {
  const dot   = document.getElementById('provider-dot');
  const name  = document.getElementById('provider-name');
  const model = document.getElementById('provider-model');
  const badge = document.getElementById('grok-authed-badge');
  try {
    const resp = await fetch(`${API_BASE}/api/status`);
    if (!resp.ok) throw new Error();
    const d = await resp.json();
    const isGrok = d.provider === 'grok' || d.provider === 'xai';
    name.textContent  = isGrok ? '𝕏 Grok (xAI)' : providerFromUrl(d.base_url);
    model.textContent = `model: ${d.model}${d.key_configured ? '' : ' · no credentials!'}`;
    dot.classList.toggle('inactive', !d.key_configured);
    if (d.grok_authed) {
      badge.hidden = false;
      document.getElementById('btn-grok-connect').hidden = true;
    }
  } catch {
    name.textContent = 'Server offline';
    if (dot) dot.classList.add('inactive');
  }
}

// ---------------------------------------------------------------------------
// Grok device-code OAuth
// ---------------------------------------------------------------------------
let _grokPollTimer = null;

window.grokConnect = async function() {
  const btn  = document.getElementById('btn-grok-connect');
  const box  = document.getElementById('grok-device-code-box');
  const url  = document.getElementById('grok-verify-url');
  const code = document.getElementById('grok-user-code');
  const badge = document.getElementById('grok-authed-badge');

  btn.disabled = true;
  btn.textContent = '⠙ Starting OAuth…';

  try {
    const resp = await fetch(`${API_BASE}/api/grok/login`, { method: 'POST' });
    if (!resp.ok) { const e = await resp.json().catch(() => ({})); throw new Error(e.error || 'login failed'); }
    const dc = await resp.json();

    // Show device code UI
    const verifyUrl = dc.verification_uri_complete || dc.verification_uri;
    url.href = verifyUrl;
    url.textContent = verifyUrl;
    code.textContent = dc.user_code;
    box.hidden = false;
    btn.textContent = '𝕏 Connect Grok Account';
    btn.disabled = false;

    // Open the URL automatically
    window.open(verifyUrl, '_blank', 'noopener');

    // Poll every interval seconds, and give up when the device code expires
    // instead of showing "Waiting for approval…" forever.
    const interval = (dc.interval || 5) * 1000;
    const deadline = Date.now() + (dc.expires_in || 600) * 1000;
    if (_grokPollTimer) clearInterval(_grokPollTimer);
    _grokPollTimer = setInterval(async () => {
      if (Date.now() > deadline) {
        clearInterval(_grokPollTimer);
        _grokPollTimer = null;
        box.hidden = true;
        btn.disabled = false;
        logRaw('That Grok code expired — hit Connect again for a fresh one.', 't-warn');
        return;
      }
      try {
        const pr = await fetch(`${API_BASE}/api/grok/poll`, {
          method: 'POST',
          headers: { 'Content-Type': 'application/json' },
          body: JSON.stringify({ device_code: dc.device_code }),
        });
        if (!pr.ok) {
          // Real OAuth error (denied, expired server-side) — stop polling.
          const e = await pr.json().catch(() => ({}));
          clearInterval(_grokPollTimer);
          _grokPollTimer = null;
          box.hidden = true;
          btn.disabled = false;
          logRaw(`Grok OAuth error: ${e.error || pr.status}`, 't-err');
          return;
        }
        const pd = await pr.json();
        if (pd.done) {
          clearInterval(_grokPollTimer);
          _grokPollTimer = null;
          box.hidden = true;
          btn.hidden = true;
          badge.hidden = false;
          // Update status card
          document.getElementById('provider-name').textContent = '𝕏 Grok (xAI) — Authed';
          logRaw('✓ Grok OAuth complete — restart server with RUSTYFI_PROVIDER=grok to use it', 't-ok');
        }
      } catch { /* transient network blip — keep polling until deadline */ }
    }, interval);

  } catch (err) {
    btn.disabled = false;
    btn.textContent = '𝕏 Connect Grok Account';
    logRaw(`Grok OAuth error: ${err.message}`, 't-err');
  }
};

// ---------------------------------------------------------------------------
// Startup status — tell the user NOW if the server isn't ready, not after
// they've zipped, dragged and clicked.
// ---------------------------------------------------------------------------
async function initStatus() {
  try {
    const resp = await fetch(`${API_BASE}/api/status`);
    if (!resp.ok) throw new Error();
    const d = await resp.json();
    const isGrok = d.provider === 'grok' || d.provider === 'xai';
    const providerLabel = isGrok ? 'xAI Grok' : providerFromUrl(d.base_url);
    log(`Server v${d.version} · ${providerLabel} · model ${d.model}`, 't-dim');
    if (d.fix_distinct) {
      log(`Fix loop uses a stronger model: ${d.fix_model} 🔧`, 't-info');
    }
    if (d.key_configured) {
      log('Provider ready ✓ — drop a ZIP to begin.', 't-ok');
    } else if (isGrok) {
      log('⚠ Grok selected but not connected — open the Provider tab and connect, or run `grok login`.', 't-warn');
    } else {
      log('⚠ No API key configured — set RUSTYFI_LLM_API_KEY on the server and restart, or translations will fail.', 't-warn');
    }
  } catch {
    log('Could not reach the Rustyfi server — is it running?', 't-err');
  }
}

function providerFromUrl(url) {
  const u = String(url ?? '');
  if (u.includes('openrouter')) return 'OpenRouter';
  if (u.includes('openai.com')) return 'OpenAI';
  if (u.includes('googleapis')) return 'Google Gemini';
  if (u.includes('cerebras'))   return 'Cerebras';
  if (u.includes('x.ai'))       return 'xAI';
  return 'OpenAI-compatible';
}

// ---------------------------------------------------------------------------
// Easter eggs 🦀🎺
// ---------------------------------------------------------------------------

// Crab parade — celebratory crustaceans on success (and on the Konami code).
function crabParade(count = 18) {
  if (REDUCED_MOTION) return;
  for (let i = 0; i < count; i++) {
    const crab = document.createElement('span');
    crab.className = 'crab-fall';
    crab.textContent = '🦀';
    crab.style.left = `${Math.random() * 100}vw`;
    crab.style.animationDelay = `${Math.random() * 0.9}s`;
    crab.style.animationDuration = `${2.2 + Math.random() * 1.8}s`;
    crab.style.fontSize = `${18 + Math.random() * 22}px`;
    document.body.appendChild(crab);
    crab.addEventListener('animationend', () => crab.remove());
  }
}

// Sad trombone — the logo droops when a run fails. The product is literally
// named after the instrument; the joke writes itself.
function sadTrombone() {
  const logo = document.querySelector('.trombone-logo');
  if (!logo || REDUCED_MOTION) return;
  logo.classList.add('trombone-sad');
  setTimeout(() => logo.classList.remove('trombone-sad'), 2600);
}

// Konami code → FERRIS MODE.
const KONAMI = ['ArrowUp','ArrowUp','ArrowDown','ArrowDown','ArrowLeft','ArrowRight','ArrowLeft','ArrowRight','b','a'];
let konamiIdx = 0;
let ferrisMode = false;
document.addEventListener('keydown', (e) => {
  if (e.key === KONAMI[konamiIdx]) {
    konamiIdx++;
  } else if (e.key === 'ArrowUp') {
    // Up,Up,Up,Down… must still trigger — an extra Up keeps the Up,Up prefix.
    konamiIdx = konamiIdx >= 2 ? 2 : 1;
  } else {
    konamiIdx = 0;
  }
  if (konamiIdx === KONAMI.length) {
    konamiIdx = 0;
    ferrisMode = !ferrisMode;
    document.body.classList.toggle('ferris-mode', ferrisMode);
    if (ferrisMode) {
      log('🦀 FERRIS MODE ACTIVATED — fearless concurrency engaged.', 't-ok');
      crabParade(30);
    } else {
      log('Ferris mode off. The crabs return to the sea. 🌊', 't-dim');
    }
  }
});

// Click the trombone 5 times → a tiny synthesized "womp womp" slide.
// Audio only ever plays from an explicit user click.
let tromboneClicks = 0;
let tromboneClickTimer = null;
document.querySelector('.nav-brand')?.addEventListener('click', () => {
  tromboneClicks++;
  clearTimeout(tromboneClickTimer);
  tromboneClickTimer = setTimeout(() => { tromboneClicks = 0; }, 1500);
  if (tromboneClicks === 5) {
    tromboneClicks = 0;
    playWompWomp();
    log('🎺 toot toot — you found the trombone. TROMBONE was here all along.', 't-info');
  }
});

function playWompWomp() {
  try {
    const ctx = new (window.AudioContext || window.webkitAudioContext)();
    const note = (startTime, fromHz, toHz, dur) => {
      const osc = ctx.createOscillator();
      const gain = ctx.createGain();
      osc.type = 'sawtooth';
      osc.frequency.setValueAtTime(fromHz, startTime);
      osc.frequency.linearRampToValueAtTime(toHz, startTime + dur);
      gain.gain.setValueAtTime(0.001, startTime);
      gain.gain.linearRampToValueAtTime(0.12, startTime + 0.04);
      gain.gain.exponentialRampToValueAtTime(0.001, startTime + dur);
      osc.connect(gain).connect(ctx.destination);
      osc.start(startTime);
      osc.stop(startTime + dur + 0.05);
    };
    const t = ctx.currentTime;
    note(t,        233, 220, 0.35); // womp
    note(t + 0.45, 220, 196, 0.55); // woooomp
  } catch { /* no audio context — silence is also a valid trombone solo */ }
}

// ---------------------------------------------------------------------------
// Init — load history + server status on page open
// ---------------------------------------------------------------------------
loadHistory();
initStatus();
