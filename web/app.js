/* =========================================================
   Rustyfi — App JavaScript
   ========================================================= */

// ---------------------------------------------------------------------------
// Background canvas — animated grid + particles
// ---------------------------------------------------------------------------
(function initCanvas() {
  const canvas = document.getElementById('bg-canvas');
  const ctx    = canvas.getContext('2d');
  let W, H, particles = [], rafId;

  function resize() {
    W = canvas.width  = window.innerWidth;
    H = canvas.height = window.innerHeight;
  }

  function mkParticle() {
    return {
      x: Math.random() * W,
      y: Math.random() * H,
      vx: (Math.random() - 0.5) * 0.18,
      vy: (Math.random() - 0.5) * 0.18,
      r: Math.random() * 1.5 + 0.4,
      a: Math.random() * 0.4 + 0.1,
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

  function frame() {
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

    rafId = requestAnimationFrame(frame);
  }

  window.addEventListener('resize', () => { resize(); initParticles(); });
  resize();
  initParticles();
  frame();
})();

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------
const API_BASE = window.location.origin;

const state = {
  file:     null,      // File | null
  running:  false,
  zipData:  null,      // ArrayBuffer | null (for download)
  crateName: 'app',
  totalFiles: 0,
  doneFiles:  0,
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

// ---------------------------------------------------------------------------
// Terminal helpers
// ---------------------------------------------------------------------------
function log(msg, cls = 't-ok') {
  const p = document.createElement('p');
  p.className = `t-line ${cls}`;
  p.textContent = `> ${msg}`;
  termBody.appendChild(p);
  termBody.scrollTop = termBody.scrollHeight;
}

function logRaw(msg, cls = 't-ok') {
  const p = document.createElement('p');
  p.className = `t-line ${cls}`;
  p.textContent = msg;
  termBody.appendChild(p);
  termBody.scrollTop = termBody.scrollHeight;
}

// ---------------------------------------------------------------------------
// Stage tracker
// ---------------------------------------------------------------------------
const STAGE_ORDER = ['Idle','Parsing','Scaffolding','Translating','Verifying','Optimizing','Completed','Failed'];
let currentStageIndex = 0;

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
dropzone.addEventListener('click', (e) => {
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
  const f = e.dataTransfer.files[0];
  if (f) selectFile(f);
});

function selectFile(f) {
  if (!f.name.endsWith('.zip')) {
    log(`Please drop a .zip file (got: ${f.name})`, 't-warn');
    return;
  }
  state.file = f;
  state.crateName = f.name.replace(/\.zip$/, '').replace(/[^a-z0-9_]/gi, '_').toLowerCase();

  // Update UI
  const inner = dropzone.querySelector('.dropzone-inner');
  inner.querySelector('.drop-title').textContent = f.name;
  inner.querySelector('.drop-sub').textContent = `${(f.size / 1024).toFixed(1)} KB · Ready to translate`;

  log(`Selected: ${f.name} (${(f.size / 1024).toFixed(1)} KB)`, 't-info');
  btnTranslate.hidden = false;
  btnReset.hidden = false;
}

// ---------------------------------------------------------------------------
// Translation
// ---------------------------------------------------------------------------
btnTranslate.addEventListener('click', startTranslation);

async function startTranslation() {
  if (!state.file || state.running) return;
  state.running = true;
  state.zipData = null;
  state.doneFiles = 0;

  btnTranslate.hidden = true;
  btnDownload.hidden  = true;
  showProgress('Uploading…');
  setStage('Parsing');
  log('Uploading archive to Rustyfi server…', 't-info');

  const form = new FormData();
  form.append('archive', state.file, state.file.name);

  let response;
  try {
    response = await fetch(`${API_BASE}/api/translate`, {
      method: 'POST',
      body: form,
    });
  } catch (err) {
    log(`Network error: ${err.message}`, 't-err');
    state.running = false;
    btnReset.hidden = false;
    return;
  }

  if (!response.ok) {
    const text = await response.text();
    log(`Server error ${response.status}: ${text}`, 't-err');
    state.running = false;
    return;
  }

  // SSE stream
  const reader = response.body.getReader();
  const decoder = new TextDecoder();
  let buf = '';

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
        handleProgress(JSON.parse(json));
      } catch { /* ignore malformed */ }
    }
  }
}

// ---------------------------------------------------------------------------
// SSE event handler
// ---------------------------------------------------------------------------
function handleProgress(event) {
  switch (event.kind) {
    case 'state_changed':
      setStage(event.state);
      setProgress(stagePercent(event.state), event.state);
      log(`→ ${event.state}`, 't-info');
      break;

    case 'file_started':
      state.totalFiles = event.total;
      state.doneFiles  = event.index;
      statFiles.textContent = `${event.index}/${event.total}`;
      const shortName = event.file.split('/').pop();
      setFileIndicator(`Translating ${shortName}`);
      log(`[${event.index + 1}/${event.total}] ${shortName}`, 't-ok');
      setProgress(
        10 + (event.index / Math.max(event.total, 1)) * 50,
        `Translating ${shortName}`
      );
      break;

    case 'file_complete':
      state.doneFiles++;
      statFiles.textContent = `${state.doneFiles}/${state.totalFiles}`;
      logRaw(`  ✓ ${event.file}`, 't-dim');
      break;

    case 'compiler_error':
      log('Compiler errors detected — entering fix loop…', 't-warn');
      logRaw(event.message.slice(0, 400), 't-err');
      break;

    case 'fix_cycle':
      log(`Fix cycle ${event.attempt}…`, 't-warn');
      setProgress(65 + event.attempt * 5, `Fix cycle ${event.attempt}`);
      break;

    case 'done':
      onDone(event.zip_bytes);
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
function onDone(zipBytes) {
  state.running = false;
  hideFileIndicator();
  setStage('Completed');
  setProgress(100, 'Complete!');
  statSize.textContent = `${(zipBytes / 1024).toFixed(1)} KB`;
  log('Translation complete!', 't-ok');
  log(`Output ZIP: ${(zipBytes / 1024).toFixed(1)} KB`, 't-info');

  // Show download button — triggers a new fetch for the actual zip
  btnDownload.hidden = false;
  btnDownload.onclick = downloadResult;
  btnReset.hidden = false;
}

function onFailed(reason) {
  state.running = false;
  hideFileIndicator();
  setStage('Failed');
  log(`Failed: ${reason}`, 't-err');
  btnReset.hidden = false;
}

async function downloadResult() {
  // Re-fetch the zip from the server download endpoint.
  // The server stores it at /api/download/<crate-name>.zip.
  // For the MVP the pipeline stores it in temp — we reconstruct by re-reading.
  // This is simplified: the real download hits the SSE-provided zip_bytes.
  // Since we receive zip_bytes count (not content) over SSE, we fetch the
  // download endpoint the server exposes separately.
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
  state.file      = null;
  state.running   = false;
  state.zipData   = null;
  state.doneFiles = 0;
  state.totalFiles = 0;

  // Reset dropzone text
  const inner = dropzone.querySelector('.dropzone-inner');
  inner.querySelector('.drop-title').textContent = 'Drop Your App Here';
  inner.querySelector('.drop-sub').textContent = 'ZIP your project folder and drag it in — or click to browse';

  // Reset UI
  setStage('Idle');
  progressWrap.hidden = true;
  setProgress(0, '');
  hideFileIndicator();
  btnTranslate.hidden = true;
  btnDownload.hidden  = true;
  btnReset.hidden     = true;
  statFiles.textContent    = '—';
  statProgress.textContent = '0%';
  statSize.textContent     = '—';

  termBody.innerHTML = '<p class="t-line t-dim">Rustyfi v0.1.0 — ready</p>';
  fileInput.value = '';
  log('Reset. Drop a new app to begin.', 't-dim');
});
