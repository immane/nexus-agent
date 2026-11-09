const root = document.documentElement;
const theme = document.querySelector('#theme');
const themes = new Set(['system', 'graphite', 'paper', 'forest', 'sand']);

function applyTheme(value) {
  const selected = themes.has(value) ? value : 'system';
  if (selected === 'system') delete root.dataset.theme;
  else root.dataset.theme = selected;
  theme.value = selected;
}

try { applyTheme(localStorage.getItem('nexus.theme')); }
catch { applyTheme('system'); }

theme.addEventListener('change', () => {
  applyTheme(theme.value);
  try { localStorage.setItem('nexus.theme', theme.value); }
  catch { /* Appearance remains usable when browser storage is unavailable. */ }
});

const settings = document.querySelector('#settings');
const token = document.querySelector('#token');
document.querySelector('#open-settings').addEventListener('click', () => settings.showModal());
document.querySelector('#close-settings').addEventListener('click', () => settings.close());
settings.addEventListener('close', () => { token.value = ''; });

// A single same-origin boundary for future API interactions. Never retry mutations.
async function request(path, credential, signal) {
  const headers = { Accept: 'application/json' };
  if (credential) headers.Authorization = `Bearer ${credential}`;
  const response = await fetch(path, {
    headers, signal, cache: 'no-store', credentials: 'omit', redirect: 'error',
  });
  if (!response.ok) {
    const error = new Error(`Server returned HTTP ${response.status}.`);
    error.status = response.status;
    throw error;
  }
  return response.json();
}

document.querySelector('#connection-form').addEventListener('submit', async (event) => {
  event.preventDefault();
  const button = document.querySelector('#check-connection');
  if (button.disabled) return;
  const status = document.querySelector('#connection-status');
  const detail = document.querySelector('#connection-detail');
  let credential = token.value.trim();
  token.value = '';
  if (credential && !/^[a-f\d]{64}$/i.test(credential)) {
    detail.textContent = 'Expected a 64-character hexadecimal bearer token.';
    return;
  }
  button.disabled = true;
  status.textContent = 'Checking…';
  delete status.dataset.state;
  detail.textContent = 'Checking the local server…';
  const controller = new AbortController();
  const timeout = setTimeout(() => controller.abort(), 8000);
  try {
    const health = await request('/health', '', controller.signal);
    if (health.status !== 'ok') throw new Error('Unexpected health response.');
    await request('/config', credential, controller.signal);
    status.textContent = 'API connected';
    status.dataset.state = 'ok';
    detail.textContent = 'Server reachable. API access verified. No session started.';
  } catch (error) {
    status.textContent = error.status === 401 ? 'Authentication required' : 'Connection not ready';
    status.dataset.state = 'error';
    detail.textContent = error.status === 401
      ? 'Server reachable. Enter a valid server bearer token to check API access.'
      : error.status === 503
        ? 'Server reachable, but configuration or authentication is not ready.'
        : error.name === 'AbortError' ? 'Connection check timed out.' : error.message;
  } finally {
    clearTimeout(timeout);
    credential = '';
    button.disabled = false;
  }
});
