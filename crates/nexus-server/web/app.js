const MAX_ASSISTANT_CHARS = 200_000;
const MAX_TOOL_CHARS = 64_000;
const MAX_TITLE_CHARS = 56;
const MAX_STREAM_RETRIES = 2;
const MAX_SSE_FRAME_CHARS = 4 * 1024 * 1024;
const TOKEN_PATTERN = /^[a-f\d]{64}$/i;

const dom = {
  root: document.documentElement,
  theme: document.querySelector('#theme'),
  settings: document.querySelector('#settings'),
  openSettings: document.querySelector('#open-settings'),
  closeSettings: document.querySelector('#close-settings'),
  connectionForm: document.querySelector('#connection-form'),
  token: document.querySelector('#token'),
  connect: document.querySelector('#connect'),
  disconnect: document.querySelector('#disconnect'),
  connectionStatus: document.querySelector('#connection-status'),
  connectionDetail: document.querySelector('#connection-detail'),
  model: document.querySelector('#model'),
  modelLabel: document.querySelector('#model-label'),
  modelBadge: document.querySelector('#model-badge'),
  message: document.querySelector('#message'),
  composer: document.querySelector('#composer'),
  send: document.querySelector('#send'),
  cancel: document.querySelector('#cancel-run'),
  runStatus: document.querySelector('#run-status'),
  usage: document.querySelector('#usage-status'),
  main: document.querySelector('#main'),
  welcome: document.querySelector('#welcome'),
  conversation: document.querySelector('#conversation'),
  conversationTitle: document.querySelector('#conversation-title'),
  sessionCount: document.querySelector('#session-count'),
  sessionItem: document.querySelector('#session-item'),
  sessionItemTitle: document.querySelector('#session-item-title'),
  sessionItemMeta: document.querySelector('#session-item-meta'),
  noSessionNote: document.querySelector('#no-session-note'),
  newSession: document.querySelector('#new-session'),
  newSessionMobile: document.querySelector('#new-session-mobile'),
};

const state = {
  connected: false,
  connecting: false,
  credential: '',
  config: null,
  choices: [],
  selection: null,
  session: null,
  sessionProvider: null,
  sessionModel: null,
  sessionTitle: '',
  run: null,
  active: false,
  submitting: false,
  cancelling: false,
  cancelRequested: false,
  inFlightUnknown: false,
  runTerminal: false,
  streaming: false,
  streamController: null,
  streamGeneration: 0,
  reconnectTimer: 0,
  retries: 0,
  lastSeq: null,
  assistantItems: new Map(),
  tools: new Map(),
  approvals: new Map(),
  pendingApprovalSeen: false,
  noticeKeys: new Set(),
  notices: new Map(),
  lastOutcome: null,
  pinned: true,
  connectGeneration: 0,
};

const pendingText = new Map();
let textFrame = 0;

/* Appearance */

const themes = new Set(['system', 'graphite', 'paper', 'forest', 'sand']);

function applyTheme(value) {
  const selected = themes.has(value) ? value : 'system';
  if (selected === 'system') delete dom.root.dataset.theme;
  else dom.root.dataset.theme = selected;
  dom.theme.value = selected;
}

try {
  applyTheme(localStorage.getItem('nexus.theme'));
} catch {
  applyTheme('system');
}

dom.theme.addEventListener('change', () => {
  applyTheme(dom.theme.value);
  try {
    localStorage.setItem('nexus.theme', dom.theme.value);
  } catch {
    // Appearance remains usable when browser storage is unavailable.
  }
});

/* Small helpers */

function node(tag, className, text) {
  const element = document.createElement(tag);
  if (className) element.className = className;
  if (text !== undefined) element.textContent = text;
  return element;
}

function clip(text, limit) {
  if (text.length <= limit) return text;
  let end = limit;
  const code = text.charCodeAt(end - 1);
  if (code >= 0xd800 && code <= 0xdbff) end -= 1;
  return text.slice(0, end);
}

function setStatus(element, text, stateName) {
  element.textContent = text;
  if (stateName) element.dataset.state = stateName;
  else delete element.dataset.state;
}

function setRunStatus(text, stateName) {
  setStatus(dom.runStatus, text, stateName);
}

function setConnectionStatus(text, stateName) {
  setStatus(dom.connectionStatus, text, stateName);
}

function describeModelLabel() {
  if (!state.selection) return 'No model';
  return state.selection.label;
}

function updateModelDisplay() {
  const label = describeModelLabel();
  dom.modelBadge.textContent = label;
  dom.modelBadge.hidden = !state.connected || !state.selection;
  dom.modelLabel.textContent = label;
}

function selectedOutcome(outcome) {
  switch (outcome) {
    case 'completed':
      return 'Completed';
    case 'failed':
      return 'Failed';
    case 'refused':
      return 'Refused';
    case 'cancelled':
      return 'Cancelled';
    case 'limit-reached':
      return 'Limit reached';
    default:
      return outcome || 'Finished';
  }
}

function outcomeTone(outcome) {
  if (outcome === 'completed') return 'ok';
  if (outcome === 'cancelled' || outcome === 'limit-reached') return 'busy';
  return 'error';
}

function retryLabel(value) {
  switch (value) {
    case 'DoNotRetry':
      return 'do not retry';
    case 'RetryAfterBackoff':
      return 'retry after backoff';
    case 'SafeToRetry':
      return 'safe to retry';
    default:
      return value || 'unknown';
  }
}

/* Scrolling and incremental text */

function nearBottom() {
  return dom.main.scrollHeight - dom.main.scrollTop - dom.main.clientHeight < 120;
}

function scrollToBottom() {
  dom.main.scrollTop = dom.main.scrollHeight;
}

function flushText() {
  if (textFrame) {
    cancelAnimationFrame(textFrame);
    textFrame = 0;
  }
  if (!pendingText.size) return;
  for (const [textNode, text] of pendingText) textNode.appendData(text);
  pendingText.clear();
  if (state.pinned) scrollToBottom();
}

function queueText(textNode, text) {
  pendingText.set(textNode, (pendingText.get(textNode) || '') + text);
  if (!textFrame) {
    textFrame = requestAnimationFrame(() => {
      textFrame = 0;
      flushText();
    });
  }
}

function appendNode(element) {
  flushText();
  const pinned = state.pinned || nearBottom();
  dom.conversation.append(element);
  dom.welcome.hidden = true;
  dom.conversation.hidden = false;
  if (pinned) scrollToBottom();
}

/* Notices */

function addNotice(kind, text, key, action) {
  if (key && state.noticeKeys.has(key)) return null;
  if (key) state.noticeKeys.add(key);
  const notice = node('div', 'notice');
  notice.dataset.kind = kind;
  notice.setAttribute('role', 'status');
  notice.append(node('span', 'notice-text', text));
  if (action) {
    const button = node('button', 'notice-action', action.label);
    button.type = 'button';
    button.addEventListener('click', () => {
      button.disabled = true;
      if (key) removeNotice(key);
      Promise.resolve(action.run()).catch(() => {});
    });
    notice.append(button);
  }
  if (key) {
    state.notices.set(key, notice);
    notice.dataset.noticeKey = key;
  }
  appendNode(notice);
  return notice;
}

function removeNotice(key) {
  const notice = state.notices.get(key);
  if (!notice) return;
  state.notices.delete(key);
  state.noticeKeys.delete(key);
  notice.remove();
}

/* HTTP */

class ApiError extends Error {
  constructor(status, message) {
    super(message);
    this.name = 'ApiError';
    this.status = status;
  }
}

async function api(path, options = {}) {
  const method = options.method || 'GET';
  const headers = { Accept: 'application/json' };
  if (options.body !== undefined) headers['Content-Type'] = 'application/json';
  const credential = options.credential !== undefined ? options.credential : state.credential;
  if (credential) headers.Authorization = `Bearer ${credential}`;
  const response = await fetch(path, {
    method,
    headers,
    body: options.body === undefined ? undefined : JSON.stringify(options.body),
    signal: options.signal,
    cache: 'no-store',
    credentials: 'omit',
    redirect: 'error',
  });
  let payload = null;
  try {
    payload = await response.json();
  } catch {
    // A missing or non-JSON body is reported through the status below.
  }
  if (!response.ok) {
    if (response.status === 401 && options.credential === undefined) markUnauthorized();
    throw new ApiError(response.status, payload?.error || `Server returned HTTP ${response.status}.`);
  }
  return payload;
}

function markUnauthorized() {
  state.connected = false;
  state.credential = '';
  setConnectionStatus('Authentication required', 'error');
  dom.connectionDetail.textContent = 'Authentication failed. Enter the current server bearer token.';
  setRunStatus('Authentication required', 'error');
  updateControls();
}

/* Configuration and model choices */

function buildChoices(config) {
  const models = Array.isArray(config?.models) ? config.models : [];
  if (!models.length) {
    return [{
      demo: true,
      provider: null,
      model: null,
      label: 'Demo script',
      detail: 'test-only · no provider configured',
    }];
  }
  const byId = new Map();
  for (const entry of models) {
    if (!entry || typeof entry.id !== 'string' || typeof entry.provider !== 'string') continue;
    if (!byId.has(entry.id)) byId.set(entry.id, entry);
  }
  const ordered = [];
  const seen = new Set();
  const add = (id) => {
    if (seen.has(id)) return;
    const entry = byId.get(id);
    if (!entry) return;
    seen.add(id);
    ordered.push(entry);
  };
  if (Array.isArray(config.favourites)) config.favourites.forEach(add);
  if (Array.isArray(config.recent)) config.recent.forEach(add);
  for (const id of byId.keys()) add(id);
  return ordered.map((entry) => ({
    demo: false,
    provider: entry.provider,
    model: entry.id,
    label: entry.name || entry.id,
    detail: `${entry.provider} · ${entry.id}`,
  }));
}

function choiceKey(choice) {
  return choice?.demo ? 'demo' : `${choice?.provider}\u0000${choice?.model}`;
}

function populateModels() {
  const previous = state.selection ? choiceKey(state.selection) : '';
  state.choices = buildChoices(state.config);
  const sessionChoice = state.session
    ? state.choices.find((choice) =>
        choice.demo
          ? !state.sessionProvider
          : choice.provider === state.sessionProvider && choice.model === state.sessionModel)
    : null;
  let selected = state.session
    ? sessionChoice
    : state.choices.find((choice) => choiceKey(choice) === previous);
  if (state.session && !selected) {
    selected = {
      demo: !state.sessionProvider,
      provider: state.sessionProvider,
      model: state.sessionModel,
      label: state.sessionModel || 'Unknown model',
      detail: 'not in the current configuration',
    };
  }
  if (!selected) selected = state.choices[0] || null;
  state.selection = selected;

  dom.model.replaceChildren();
  state.choices.forEach((choice, index) => {
    const option = node('option', '', `${choice.label} — ${choice.detail}`);
    option.value = String(index);
    dom.model.append(option);
  });
  if (selected) {
    const index = state.choices.indexOf(selected);
    dom.model.value = String(index);
  }
  updateModelDisplay();
}

/* Connection */

async function connect(candidate) {
  candidate = typeof candidate === 'string' ? candidate.trim() : '';
  if (candidate && !TOKEN_PATTERN.test(candidate)) {
    dom.connectionDetail.textContent = 'Expected exactly 64 hexadecimal characters.';
    return false;
  }
  if (state.connecting) return false;

  const generation = ++state.connectGeneration;
  const previous = { connected: state.connected, credential: state.credential };
  const controller = new AbortController();
  const timeout = setTimeout(() => controller.abort(), 10_000);
  state.connecting = true;
  dom.connect.disabled = true;
  dom.connectionDetail.textContent = 'Connecting to the local server…';
  setConnectionStatus('Connecting…', 'busy');
  if (!previous.connected) setRunStatus('Connecting…', 'busy');

  try {
    const health = await api('/health', { credential: '', signal: controller.signal });
    if (health?.status !== 'ok') throw new Error('Unexpected health response.');
    const config = await api('/config', { credential: candidate, signal: controller.signal });
    if (generation !== state.connectGeneration) return false;

    state.connected = true;
    state.credential = candidate;
    state.config = config;
    populateModels();
    dom.token.value = '';
    setConnectionStatus('Connected', 'ok');

    const modelCount = Array.isArray(config?.models) ? config.models.length : 0;
    dom.connectionDetail.textContent = modelCount
      ? `Connected. ${modelCount} configured model${modelCount === 1 ? '' : 's'} available.`
      : 'Connected. No provider is configured; sessions use the demo script.';

    updateSessionDisplay();
    updateControls();
    await resumeIfNeeded();
    return true;
  } catch (error) {
    if (generation !== state.connectGeneration) return false;
    state.connected = previous.connected;
    state.credential = previous.credential;
    if (!previous.connected) {
      dom.model.replaceChildren();
      state.selection = null;
      updateModelDisplay();
    }
    if (previous.connected) {
      setConnectionStatus('Connected', 'ok');
      dom.connectionDetail.textContent = 'The new connection attempt failed; the previous connection is still active.';
    } else if (error instanceof ApiError && error.status === 401) {
      setConnectionStatus('Authentication required', 'error');
      dom.connectionDetail.textContent = 'Authentication required. Enter the server bearer token.';
      setRunStatus('Authentication required', 'error');
    } else if (error instanceof ApiError) {
      setConnectionStatus('Connection failed', 'error');
      dom.connectionDetail.textContent = error.message;
      setRunStatus('Connection failed', 'error');
    } else if (error.name === 'AbortError') {
      setConnectionStatus('Connection timed out', 'error');
      dom.connectionDetail.textContent = 'The local server did not answer in time.';
      setRunStatus('Connection timed out', 'error');
    } else {
      setConnectionStatus('Server unreachable', 'error');
      dom.connectionDetail.textContent = 'Server unreachable. Check that nexus-server is running.';
      setRunStatus('Server unreachable', 'error');
    }
    updateControls();
    return false;
  } finally {
    clearTimeout(timeout);
    if (generation === state.connectGeneration) {
      state.connecting = false;
      updateControls();
    }
  }
}

function disconnect() {
  if (state.active || state.submitting || state.connecting) return;
  state.connectGeneration += 1;
  state.connected = false;
  state.credential = '';
  setConnectionStatus('Not connected', '');
  setRunStatus('Not connected', '');
  dom.connectionDetail.textContent = 'Disconnected. The page no longer holds a server token.';
  updateControls();
}

/* Session and controls */

function updateSessionDisplay() {
  const exists = Boolean(state.session);
  const stateLabel = state.inFlightUnknown
    ? 'Unknown'
    : state.active
      ? state.cancelling ? 'Cancelling' : state.pendingApprovalSeen ? 'Awaiting approval' : 'Running'
      : state.runTerminal ? selectedOutcome(state.lastOutcome) : 'Ready';
  dom.sessionCount.textContent = exists ? '1' : '0';
  dom.noSessionNote.hidden = exists;
  dom.sessionItem.hidden = !exists;
  if (exists) {
    dom.sessionItemTitle.textContent = state.sessionTitle || 'New conversation';
    dom.sessionItemMeta.textContent = `${describeModelLabel()} · ${stateLabel}`;
    dom.sessionItem.dataset.state = state.active ? 'active' : 'idle';
  }
  dom.conversationTitle.textContent = state.sessionTitle || 'New conversation';
}

function updateControls() {
  const connected = state.connected;
  const busy = state.active || state.submitting || state.connecting;
  const blocked = state.inFlightUnknown;
  const canNewSession = connected && !busy && !blocked;

  dom.newSession.disabled = !canNewSession;
  dom.newSessionMobile.disabled = !canNewSession;
  dom.message.disabled = !connected || blocked;
  dom.send.disabled = !connected || busy || blocked || !state.selection || !dom.message.value.trim();
  dom.model.disabled = !connected || busy || Boolean(state.session);
  dom.model.hidden = Boolean(state.session);
  dom.modelLabel.hidden = !state.session;
  dom.cancel.hidden = !state.active;
  dom.cancel.disabled = !connected || !state.active || state.cancelling || state.cancelRequested || !state.run;
  dom.cancel.textContent = state.cancelling || state.cancelRequested ? 'Cancelling…' : 'Cancel';
  dom.connect.disabled = state.connecting;
  dom.disconnect.disabled = !connected || busy;
  dom.message.placeholder = !connected
    ? 'Connect to start a task…'
    : blocked
      ? 'Resolve the previous submission first…'
      : state.active
        ? 'A run is active. You can draft the next task…'
        : 'Describe a task…';
  for (const record of state.approvals.values()) {
    const available = connected && !record.attempted && !state.runTerminal;
    record.approve.disabled = !available;
    record.deny.disabled = !available;
    record.actions.hidden = record.attempted || state.runTerminal;
    if (!available && !record.attempted && !connected && !state.runTerminal && !record.result.textContent) {
      record.result.textContent = 'Reconnect before deciding.';
    } else if (available && record.result.textContent === 'Reconnect before deciding.') {
      record.result.textContent = '';
    }
  }
  updateModelDisplay();
}

function resetRunState() {
  state.run = null;
  state.active = false;
  state.cancelling = false;
  state.cancelRequested = false;
  state.inFlightUnknown = false;
  state.runTerminal = false;
  state.streaming = false;
  state.lastSeq = null;
  state.retries = 0;
  state.lastOutcome = null;
  state.pendingApprovalSeen = false;
  state.assistantItems.clear();
  state.tools.clear();
  state.approvals.clear();
  state.noticeKeys.clear();
  state.notices.clear();
  dom.usage.hidden = true;
  dom.usage.textContent = '';
}

function stopStream() {
  state.streamGeneration += 1;
  if (state.streamController) state.streamController.abort();
  state.streamController = null;
  state.streaming = false;
  if (state.reconnectTimer) {
    clearTimeout(state.reconnectTimer);
    state.reconnectTimer = 0;
  }
}

function startNewSession({ force = false } = {}) {
  if (!state.connected || state.active || state.submitting || state.connecting) return;
  if (state.inFlightUnknown && !force) {
    addNotice(
      'error',
      'The previous submission has an unknown outcome. Starting a new session does not cancel any run that may still be executing.',
      `unknown-session-${Date.now()}`,
    );
    return;
  }
  if (!force && dom.conversation.childElementCount > 0) {
    if (!window.confirm('Start a new session? This page cannot restore the current transcript.')) return;
  }
  flushText();
  stopStream();
  resetRunState();
  state.session = null;
  state.sessionProvider = null;
  state.sessionModel = null;
  state.sessionTitle = '';
  if (state.connected) populateModels();
  dom.conversation.replaceChildren();
  dom.conversation.hidden = true;
  dom.welcome.hidden = false;
  dom.main.scrollTop = 0;
  state.pinned = true;
  dom.message.value = '';
  updateSessionDisplay();
  updateControls();
  setRunStatus('Ready', 'ok');
  dom.message.focus();
}

function setSessionTitleFromInput(text) {
  if (state.sessionTitle) return;
  const compact = text.replace(/\s+/g, ' ').trim();
  state.sessionTitle = compact.length > MAX_TITLE_CHARS
    ? `${compact.slice(0, MAX_TITLE_CHARS).trimEnd()}…`
    : compact;
}

async function ensureSession() {
  if (state.session) return;
  if (!state.selection) throw new ApiError(400, 'Select a model before sending.');
  const body = {};
  if (!state.selection.demo) {
    body.provider = state.selection.provider;
    body.model = state.selection.model;
  }
  const reply = await api('/sessions', { method: 'POST', body });
  if (!reply || typeof reply.session !== 'string' || !reply.session) {
    throw new ApiError(200, 'Server did not return a session identity.');
  }
  state.session = reply.session;
  state.sessionProvider = state.selection.demo ? null : state.selection.provider;
  state.sessionModel = state.selection.demo ? null : state.selection.model;
  updateSessionDisplay();
}

/* Sending */

async function sendMessage() {
  const draft = dom.message.value;
  const text = draft.trim();
  if (!text || !state.connected || state.active || state.submitting || state.connecting || state.inFlightUnknown) return;
  state.submitting = true;
  state.pinned = true;
  let submittingRun = false;
  setRunStatus(state.session ? 'Starting run…' : 'Creating session…', 'busy');
  updateControls();

  try {
    await ensureSession();
    setRunStatus('Starting run…', 'busy');
    submittingRun = true;
    const reply = await api(`/sessions/${encodeURIComponent(state.session)}/runs`, {
      method: 'POST',
      body: { input: text },
    });
    if (!reply || reply.reply !== 'accepted' || typeof reply.run !== 'string') {
      throw new ApiError(200, reply?.reply || 'Server did not accept the run.');
    }

    stopStream();
    resetRunState();
    setSessionTitleFromInput(text);
    appendUserMessage(text);
    if (dom.message.value === draft) dom.message.value = '';
    state.run = reply.run;
    state.active = true;
    updateSessionDisplay();
    setRunStatus('Starting…', 'busy');
    updateControls();
    void attachStream();
  } catch (error) {
    if (error instanceof ApiError) {
      addNotice('error', `Request failed: ${error.message}`, null);
      setRunStatus(error.status === 401 ? 'Authentication required' : 'Request failed', 'error');
      if (error.status === 401) markUnauthorized();
    } else if (submittingRun) {
      state.inFlightUnknown = true;
      addNotice(
        'error',
        'The submission outcome is unknown. The server may have started a run; this page cannot verify or cancel it. Do not resend blindly.',
        'submit-unknown',
        { label: 'Start new session', run: () => startNewSession({ force: true }) },
      );
      setRunStatus('Submission outcome unknown', 'error');
    } else {
      addNotice('error', 'Could not create a session. Check the server connection.', null);
      setRunStatus('Session creation failed', 'error');
    }
  } finally {
    state.submitting = false;
    updateSessionDisplay();
    updateControls();
  }
}

function appendUserMessage(text) {
  flushText();
  const article = node('article', 'message user');
  const head = node('div', 'message-head');
  head.append(node('span', '', 'YOU'));
  const body = node('div', 'message-body', text);
  article.append(head, body);
  appendNode(article);
}

/* SSE */

async function attachStream({ automatic = false } = {}) {
  if (!state.session || !state.run || state.runTerminal || !state.connected) return;
  const session = state.session;
  const run = state.run;
  const generation = ++state.streamGeneration;
  if (state.streamController) state.streamController.abort();
  if (state.reconnectTimer) {
    clearTimeout(state.reconnectTimer);
    state.reconnectTimer = 0;
  }
  const controller = new AbortController();
  state.streamController = controller;
  const headers = { Accept: 'text/event-stream' };
  if (state.credential) headers.Authorization = `Bearer ${state.credential}`;
  if (!automatic) setRunStatus('Connecting to run…', 'busy');

  try {
    const response = await fetch(
      `/sessions/${encodeURIComponent(session)}/runs/${encodeURIComponent(run)}/events`,
      {
        headers,
        signal: controller.signal,
        cache: 'no-store',
        credentials: 'omit',
        redirect: 'error',
      },
    );
    if (!response.ok) {
      let payload = null;
      try {
        payload = await response.json();
      } catch {
        // The status is enough when the error body is unavailable.
      }
      throw new ApiError(response.status, payload?.error || `Server returned HTTP ${response.status}.`);
    }
    if (!response.body) throw new Error('Stream body is unavailable.');
    if (generation !== state.streamGeneration) return;

    state.streaming = true;
    state.retries = 0;
    removeNotice('stream-lost-active');
    removeNotice('stream-attached');
    setRunStatus('Running', 'busy');
    updateSessionDisplay();
    await readEventStream(response.body, generation);
  } catch (error) {
    if (generation !== state.streamGeneration) return;
    if (controller.signal.aborted) return;
    if (!(error instanceof ApiError)) {
      if (state.retries < MAX_STREAM_RETRIES) {
        state.retries += 1;
        setRunStatus(`Connection lost · retrying ${state.retries}/${MAX_STREAM_RETRIES}`, 'busy');
        state.reconnectTimer = setTimeout(() => {
          state.reconnectTimer = 0;
          if (generation === state.streamGeneration) void attachStream({ automatic: true });
        }, 700 * state.retries);
        return;
      }
    }
    state.streaming = false;
    await handleStreamFailure(error);
    return;
  }

  if (generation !== state.streamGeneration) return;
  state.streaming = false;
  flushText();
  if (state.runTerminal) return;
  await reconcileAfterLoss(new Error('The event stream closed before the run finished.'));
}

async function readEventStream(body, generation) {
  const reader = body.getReader();
  const decoder = new TextDecoder();
  let buffer = '';
  for (;;) {
    const { done, value } = await reader.read();
    if (generation !== state.streamGeneration) {
      await reader.cancel().catch(() => {});
      return;
    }
    if (done) break;
    buffer += decoder.decode(value, { stream: true });
    buffer = drainFrames(buffer);
    if (buffer.length > MAX_SSE_FRAME_CHARS) {
      throw new Error('The server sent an oversized event frame.');
    }
  }
  buffer += decoder.decode();
  drainFrames(buffer);
}

function drainFrames(buffer) {
  for (;;) {
    const lf = buffer.indexOf('\n\n');
    const crlf = buffer.indexOf('\r\n\r\n');
    let index = -1;
    let width = 2;
    if (lf >= 0 && (crlf < 0 || lf <= crlf)) {
      index = lf;
    } else if (crlf >= 0) {
      index = crlf;
      width = 4;
    }
    if (index < 0) return buffer;
    const frame = buffer.slice(0, index);
    buffer = buffer.slice(index + width);
    handleFrame(frame);
  }
}

function handleFrame(frame) {
  const data = [];
  for (const line of frame.split(/\r?\n/)) {
    if (!line || line.startsWith(':')) continue;
    if (line.startsWith('data:')) {
      data.push(line.slice(5).replace(/^ /, ''));
    }
  }
  if (!data.length) return;
  let event;
  try {
    event = JSON.parse(data.join('\n'));
  } catch {
    addNotice('error', 'A malformed server event was ignored.', 'malformed-event');
    return;
  }
  handleEvent(event);
}

function handleEvent(event) {
  if (!event || typeof event !== 'object') return;
  const seq = Number(event.seq);
  if (Number.isInteger(seq) && seq > 0) {
    if (state.lastSeq !== null && seq <= state.lastSeq) return;
    if (state.lastSeq === null ? seq !== 1 : seq !== state.lastSeq + 1) {
      addNotice(
        'warning',
        'Some earlier events were unavailable. This transcript may be incomplete.',
        'stream-gap',
      );
    }
    state.lastSeq = seq;
  }

  const kind = event.kind;
  const detail = event.detail && typeof event.detail === 'object' ? event.detail : {};
  switch (kind) {
    case 'run-started':
      setRunStatus('Running', 'busy');
      break;
    case 'assistant-text':
      appendAssistantText(detail);
      break;
    case 'reasoning-redacted':
      setRunStatus('Reasoning content is hidden', 'busy');
      break;
    case 'model-request-started':
      setRunStatus(detail.purpose === 'summary' ? 'Preparing context…' : 'Waiting for model…', 'busy');
      break;
    case 'model-request-finished':
      setRunStatus('Model response received', 'busy');
      break;
    case 'model-response-discarded':
      addNotice('warning', 'A model response was discarded. The run may retry.', 'response-discarded');
      break;
    case 'context-reduced':
      appendContextNotice(detail);
      break;
    case 'tool-call-preview':
      setRunStatus('Preparing tool call', 'busy');
      break;
    case 'tool-started':
      startTool(detail);
      break;
    case 'tool-output':
      updateToolOutput(detail);
      break;
    case 'tool-finished':
      finishTool(detail);
      break;
    case 'approval-required':
      showApproval(detail);
      break;
    case 'usage':
      updateUsage(detail);
      break;
    case 'run-finished':
      finishRun(detail, { fromSnapshot: false });
      break;
    default:
      break;
  }
  if (event.terminal === true && kind !== 'run-finished') {
    finishRun({ outcome: null }, { fromSnapshot: true });
  }
}

/* Assistant text */

function markStreamingDone() {
  for (const record of state.assistantItems.values()) {
    record.article.dataset.streaming = 'false';
  }
}

function appendAssistantText(detail) {
  const text = typeof detail.text === 'string' ? detail.text : '';
  if (!text) return;
  const key = `${detail.turn ?? ''}:${detail.item ?? ''}`;
  let record = state.assistantItems.get(key);
  if (!record) {
    const article = node('article', 'message assistant');
    article.dataset.streaming = 'true';
    const head = node('div', 'message-head');
    head.append(node('span', '', 'NEXUS'));
    const body = node('div', 'message-body');
    const prose = node('div', 'prose');
    const textNode = document.createTextNode('');
    prose.append(textNode);
    body.append(prose);
    article.append(head, body);
    appendNode(article);
    record = { article, prose, textNode, chars: 0, truncated: false };
    state.assistantItems.set(key, record);
  }
  if (record.chars >= MAX_ASSISTANT_CHARS) {
    if (!record.truncated) {
      flushText();
      record.truncated = true;
      record.prose.append(node('span', 'inline-warning', 'Message truncated in this view.'));
    }
    return;
  }
  const remaining = MAX_ASSISTANT_CHARS - record.chars;
  const piece = text.length > remaining ? clip(text, remaining) : text;
  queueText(record.textNode, piece);
  record.chars += piece.length;
  if (piece.length < text.length && !record.truncated) {
    flushText();
    record.truncated = true;
    record.prose.append(node('span', 'inline-warning', 'Message truncated in this view.'));
  }
}

function appendContextNotice(detail) {
  const removed = Number.isFinite(detail.removed_items) ? detail.removed_items : 'some';
  const method = typeof detail.method === 'string' ? detail.method : 'unknown method';
  addNotice(
    'info',
    `Context reduced: ${removed} item${removed === 1 ? '' : 's'} removed using ${method}.`,
    `context-${detail.removed_items ?? 'unknown'}`,
  );
}

/* Tools */

function startTool(detail) {
  const call = String(detail.call ?? '');
  if (!call || state.tools.has(call)) return;
  flushText();
  markStreamingDone();
  setRunStatus(`Running tool: ${detail.tool || 'tool'}`, 'busy');

  const root = node('details', 'tool-card');
  root.open = true;
  const summary = node('summary');
  summary.append(node('span', 'tool-name', typeof detail.tool === 'string' ? detail.tool : 'tool'));
  const status = node('span', 'tool-status', 'Running');
  status.dataset.state = 'running';
  summary.append(status);
  root.append(summary);

  const body = node('div', 'tool-body');
  if (typeof detail.args_preview === 'string' && detail.args_preview) {
    const section = node('div', 'tool-section');
    section.append(node('span', 'tool-label', 'ARGUMENTS'));
    section.append(node('pre', '', clip(detail.args_preview, MAX_TOOL_CHARS)));
    body.append(section);
  }
  const outputSection = node('div', 'tool-section');
  outputSection.hidden = true;
  outputSection.append(node('span', 'tool-label', 'OUTPUT'));
  const output = node('pre');
  const outputText = document.createTextNode('');
  output.append(outputText);
  outputSection.append(output);
  body.append(outputSection);
  const meta = node('div', 'tool-meta');
  body.append(meta);
  root.append(body);
  appendNode(root);

  state.tools.set(call, {
    root,
    status,
    outputSection,
    outputText,
    meta,
    chars: 0,
    truncated: false,
    finished: false,
  });
}

function updateToolOutput(detail) {
  const call = String(detail.call ?? '');
  const record = state.tools.get(call);
  if (!record) return;
  const preview = typeof detail.preview === 'string' ? detail.preview : '';
  if (preview) {
    record.outputSection.hidden = false;
    if (record.chars < MAX_TOOL_CHARS) {
      const remaining = MAX_TOOL_CHARS - record.chars;
      const piece = preview.length > remaining ? clip(preview, remaining) : preview;
      queueText(record.outputText, piece);
      record.chars += piece.length;
      if (piece.length < preview.length && !record.truncated) {
        flushText();
        record.truncated = true;
        record.meta.append(node('span', '', 'Output truncated in this view'));
      }
    }
  }
  if (record.status.dataset.state === 'running') {
    setStatus(record.status, preview ? 'Receiving output' : 'Running', 'running');
  }
}

function finishTool(detail) {
  const call = String(detail.call ?? '');
  const record = state.tools.get(call);
  if (!record) return;
  flushText();
  record.finished = true;
  const outcome = detail.outcome && typeof detail.outcome === 'object' ? detail.outcome : {};
  const status = typeof outcome.status === 'string' ? outcome.status : 'Unknown';
  const content = typeof outcome.content === 'string' ? outcome.content : '';
  if (content) {
    record.outputSection.hidden = false;
    record.outputText.data = clip(content, MAX_TOOL_CHARS);
    record.chars = record.outputText.data.length;
    if (content.length > MAX_TOOL_CHARS) record.truncated = true;
  }
  const tone = status === 'Succeeded' ? 'ok'
    : status === 'Cancelled' || status === 'Interrupted' || status === 'TimedOut' ? 'busy'
      : 'error';
  const label = status === 'Succeeded' ? 'Completed'
    : status === 'TimedOut' ? 'Timed out'
      : status;
  setStatus(record.status, label, tone);
  setRunStatus(`Tool finished: ${label}`, tone === 'error' ? 'error' : 'busy');
  record.root.open = tone !== 'ok';

  record.meta.replaceChildren();
  if (typeof outcome.effect === 'string') record.meta.append(node('span', '', `Effect: ${outcome.effect}`));
  if (typeof outcome.evidence === 'string') record.meta.append(node('span', '', `Evidence: ${outcome.evidence}`));
  if (outcome.truncated) record.meta.append(node('span', '', 'Server truncated output'));
  if (record.truncated) record.meta.append(node('span', '', 'View truncated output'));
}

function markUnfinishedTools() {
  for (const record of state.tools.values()) {
    if (!record.finished) {
      setStatus(record.status, 'No final result received', 'error');
      record.meta.append(node('span', '', 'The run finished before this call reported a result'));
    }
  }
}

/* Approvals */

function showApproval(detail) {
  const approval = typeof detail.approval === 'string' ? detail.approval : '';
  const call = typeof detail.call === 'string' ? detail.call : '';
  if (!approval || !call || state.approvals.has(approval)) return;
  flushText();
  removeNotice('approval-identity-missing');
  markStreamingDone();
  state.pendingApprovalSeen = true;
  setRunStatus('Waiting for approval', 'busy');
  updateSessionDisplay();

  const root = node('article', 'approval-card');
  root.setAttribute('aria-live', 'polite');
  const head = node('div', 'approval-head');
  head.append(node('span', 'approval-title', 'APPROVAL REQUIRED'));
  head.append(node('span', 'approval-scope', typeof detail.scope === 'string' ? detail.scope : ''));
  root.append(head);
  root.append(node('p', 'approval-summary', typeof detail.summary === 'string' ? detail.summary : 'Review this tool call.'));

  let directoryChoice = null;
  if (typeof detail.args_preview === 'string' && detail.args_preview) {
    const args = node('details', 'approval-args');
    args.append(node('summary', '', 'Arguments'));
    args.append(node('pre', '', clip(detail.args_preview, MAX_TOOL_CHARS)));
    root.append(args);
  }
  if (typeof detail.session_directory === 'string' && detail.session_directory) {
    root.append(node('p', 'approval-path', `Directory: ${detail.session_directory}`));
    const label = node('label', 'scope-choice');
    const checkbox = document.createElement('input');
    checkbox.type = 'checkbox';
    label.append(checkbox, node('span', '', 'Allow this directory for the rest of the session'));
    root.append(label);
    directoryChoice = checkbox;
  }

  const actions = node('div', 'approval-actions');
  const approve = node('button', 'primary', 'Approve once');
  approve.type = 'button';
  const deny = node('button', 'danger', 'Deny');
  deny.type = 'button';
  const result = node('p', 'approval-result', '');
  actions.append(approve, deny);
  root.append(actions, result);
  appendNode(root);

  const record = {
    root,
    actions,
    approve,
    deny,
    result,
    directoryChoice,
    approval,
    call,
    busy: false,
    attempted: false,
  };
  state.approvals.set(approval, record);
  updateControls();

  approve.addEventListener('click', () => void decide(record, true));
  deny.addEventListener('click', () => void decide(record, false));
  directoryChoice?.addEventListener('change', () => {
    approve.textContent = directoryChoice.checked ? 'Approve for directory' : 'Approve once';
  });
}

async function decide(record, approve) {
  if (record.busy || record.attempted) return;
  if (!state.connected) {
    record.result.textContent = 'Reconnect before deciding.';
    return;
  }
  const session = state.session;
  const run = state.run;
  record.busy = true;
  record.attempted = true;
  record.approve.disabled = true;
  record.deny.disabled = true;
  const scope = approve && record.directoryChoice?.checked ? 'session-directory' : 'once';
  const body = { approval: record.approval, call: record.call };
  if (scope === 'session-directory') body.scope = scope;
  record.result.textContent = approve ? 'Sending approval…' : 'Sending denial…';
  setRunStatus('Sending decision…', 'busy');

  try {
    const reply = await api(
      `/sessions/${encodeURIComponent(session)}/runs/${encodeURIComponent(run)}/${approve ? 'approve' : 'deny'}`,
      { method: 'POST', body },
    );
    if (reply?.reply !== 'accepted') {
      throw new ApiError(200, reply?.reply || 'Decision was not accepted.');
    }
    record.root.dataset.decision = approve ? 'approved' : 'denied';
    record.result.textContent = approve
      ? scope === 'session-directory'
        ? 'Approved for the session directory.'
        : 'Approved once.'
      : 'Denied.';
    state.pendingApprovalSeen = false;
    if (state.session === session && state.run === run && !state.runTerminal) {
      setRunStatus(approve ? 'Approved · waiting for run' : 'Denied · waiting for run', 'busy');
    }
  } catch (error) {
    record.result.textContent = error instanceof ApiError
      ? `Decision was not accepted: ${error.message}`
      : 'Decision outcome unknown. Do not retry blindly; cancel the run if it remains active.';
    if (state.session === session && state.run === run && !state.runTerminal) {
      setRunStatus('Decision not confirmed', 'error');
    }
    if (error instanceof ApiError && error.status === 401) {
      record.attempted = false;
      markUnauthorized();
    }
  }
  updateSessionDisplay();
  updateControls();
}

/* Usage */

function updateUsage(detail) {
  const parts = [];
  if (Number.isFinite(detail.input)) parts.push(`${detail.input} in`);
  if (Number.isFinite(detail.output)) parts.push(`${detail.output} out`);
  dom.usage.textContent = parts.length ? `Usage: ${parts.join(' / ')}` : 'Usage: unknown';
  dom.usage.hidden = false;
}

/* Terminal state */

function finishRun(detail, { fromSnapshot = false } = {}) {
  if (state.runTerminal) return;
  const session = state.session;
  const run = state.run;
  const lastSeq = state.lastSeq;
  flushText();
  markStreamingDone();
  markUnfinishedTools();
  removeNotice('stream-lost-active');
  removeNotice('stream-attached');
  removeNotice('approval-identity-missing');
  removeNotice('run-state-unknown');
  for (const record of state.approvals.values()) {
    record.approve.disabled = true;
    record.deny.disabled = true;
    if (!record.result.textContent) {
      record.result.textContent = 'Run finished before a decision was sent.';
    }
  }
  state.active = false;
  state.runTerminal = true;
  state.cancelling = false;
  state.cancelRequested = false;
  state.inFlightUnknown = false;
  state.retries = 0;
  state.lastOutcome = detail.outcome || null;
  const outcome = selectedOutcome(detail.outcome);
  setRunStatus(outcome, outcomeTone(detail.outcome));
  appendRunSummary(detail, fromSnapshot);
  updateSessionDisplay();
  updateControls();
  void verifyTerminalSnapshot(session, run, lastSeq, fromSnapshot);
}

function appendRunSummary(detail, fromSnapshot) {
  const card = node('div', 'run-card');
  const outcome = node('span', 'run-outcome', selectedOutcome(detail.outcome));
  outcome.dataset.outcome = detail.outcome || 'unknown';
  card.append(outcome);
  card.append(node('span', '', fromSnapshot ? 'Confirmed by snapshot' : 'Run finished'));

  if (detail.persistence) {
    card.append(node('span', '', `Persistence: ${detail.persistence}`));
  }
  if (detail.error && typeof detail.error === 'object') {
    const errorBlock = node('div', 'run-detail');
    errorBlock.append(node('div', '', `${detail.error.category || 'Error'} · ${retryLabel(detail.error.retry)}`));
    if (detail.error.message) errorBlock.append(node('pre', '', String(detail.error.message)));
    card.append(errorBlock);
  }
  if (fromSnapshot) {
    card.append(node('div', 'run-detail', 'The terminal event was not received; the final state comes from the server snapshot.'));
  }
  appendNode(card);
}

async function verifyTerminalSnapshot(session, run, lastSeq, fromSnapshot) {
  if (!session || !run) return;
  const result = await fetchSnapshot(session, run);
  if (result.error) return;
  if (state.session !== session || state.run !== run) return;
  const snapshot = result.snapshot;
  if (!snapshot) return;
  if (snapshot.content_truncated) {
    addNotice('warning', 'The server reports truncated run content. This transcript is incomplete.', 'snapshot-truncated');
  }
  if (Number.isInteger(snapshot.last_sequence)
      && lastSeq !== null
      && snapshot.last_sequence > lastSeq) {
    addNotice('warning', 'Some events were not delivered. This transcript may be incomplete.', 'terminal-gap');
  }
  if (fromSnapshot && snapshot.last_sequence > 0 && lastSeq === null) {
    addNotice('warning', 'No events were received for this run. Only its final state is shown.', 'no-events');
  }
}

/* Snapshot recovery */

async function fetchSnapshot(session = state.session, run = state.run) {
  if (!session || !run) return { error: new ApiError(404, 'No run is available.') };
  try {
    const snapshot = await api(
      `/sessions/${encodeURIComponent(session)}/snapshot?run=${encodeURIComponent(run)}`,
    );
    return { snapshot };
  } catch (error) {
    return { error };
  }
}

async function resumeIfNeeded() {
  if (!state.connected) return;
  if (!state.session || !state.run || state.runTerminal) {
    setRunStatus('Ready', 'ok');
    return;
  }
  if (state.streaming) return;
  const session = state.session;
  const run = state.run;
  setRunStatus('Checking run…', 'busy');
  const result = await fetchSnapshot(session, run);
  if (state.session !== session || state.run !== run) return;
  if (result.error) {
    if (result.error instanceof ApiError && result.error.status === 401) {
      markUnauthorized();
    } else if (result.error instanceof ApiError && result.error.status === 404) {
      state.active = false;
      state.inFlightUnknown = true;
      setRunStatus('Run state unavailable', 'error');
      addNotice(
        'error',
        'The server no longer retains this run. Its final state is unknown. Starting a new session does not cancel anything that may still be executing.',
        'run-state-unknown',
        { label: 'Start new session', run: () => startNewSession({ force: true }) },
      );
    } else {
      state.inFlightUnknown = true;
      setRunStatus('Run state unavailable', 'error');
      addNotice(
        'warning',
        'The server did not confirm the current run state. It may still be active.',
        'run-state-unknown',
        { label: 'Retry status', run: () => resumeIfNeeded() },
      );
    }
    updateControls();
    return;
  }
  removeNotice('run-state-unknown');
  const snapshot = result.snapshot;
  updateSessionDisplay();
  if (snapshot.lifecycle === 'finalized') {
    finishRun({ outcome: snapshot.outcome }, { fromSnapshot: true });
    return;
  }
  state.active = true;
  state.inFlightUnknown = false;
  updateControls();
  if (Array.isArray(snapshot.pending_approvals) && snapshot.pending_approvals.length && !state.pendingApprovalSeen) {
    addNotice(
      'warning',
      'An approval is pending, but its exact identity was not received. Reconnect the stream; if it does not arrive, cancel the run.',
      'approval-identity-missing',
      { label: 'Reconnect stream', run: () => attachStream({ automatic: true }) },
    );
  }
  await attachStream({ automatic: true });
}

async function handleStreamFailure(error) {
  if (error instanceof ApiError && error.status === 401) {
    markUnauthorized();
    return;
  }
  if (error instanceof ApiError && error.status === 409) {
    setRunStatus('Stream unavailable', 'error');
    addNotice(
      'warning',
      'Another client is already attached to this session stream. This page cannot receive events until it disconnects.',
      'stream-attached',
      { label: 'Retry stream', run: () => attachStream({ automatic: true }) },
    );
    updateControls();
    return;
  }
  await reconcileAfterLoss(error);
}

async function reconcileAfterLoss(error) {
  const session = state.session;
  const run = state.run;
  if (!session || !run) return;
  setRunStatus('Checking run state…', 'busy');
  const result = await fetchSnapshot(session, run);
  if (state.session !== session || state.run !== run) return;
  if (result.error) {
    if (result.error instanceof ApiError && result.error.status === 401) {
      markUnauthorized();
      return;
    }
    const notRetained = result.error instanceof ApiError && result.error.status === 404;
    state.inFlightUnknown = true;
    if (notRetained) {
      state.active = false;
      setRunStatus('Run state is no longer available', 'error');
    } else {
      setRunStatus('Run state unknown', 'error');
    }
    addNotice(
      'error',
      notRetained
        ? 'The server no longer retains this run. Its final state is unknown. Starting a new session does not cancel anything that may still be executing.'
        : 'The event stream was lost and the server did not confirm the run state. It may still be active. Do not resend the task blindly.',
      'run-state-unknown',
      notRetained
        ? { label: 'Start new session', run: () => startNewSession({ force: true }) }
        : { label: 'Retry status', run: () => reconcileAfterLoss(error) },
    );
    updateSessionDisplay();
    updateControls();
    return;
  }
  removeNotice('run-state-unknown');

  const snapshot = result.snapshot;
  if (snapshot.content_truncated) {
    addNotice('warning', 'The server reports truncated run content. This transcript is incomplete.', 'snapshot-truncated');
  }
  if (snapshot.lifecycle === 'finalized') {
    finishRun({ outcome: snapshot.outcome }, { fromSnapshot: true });
    return;
  }

  state.active = true;
  state.runTerminal = false;
  state.inFlightUnknown = false;
  updateSessionDisplay();
  updateControls();
  if (Array.isArray(snapshot.pending_approvals) && snapshot.pending_approvals.length && !state.pendingApprovalSeen) {
    addNotice(
      'warning',
      'An approval is pending, but its exact identity was not received. Reconnect the stream; if it does not arrive, cancel the run.',
      'approval-identity-missing',
      { label: 'Reconnect stream', run: () => attachStream({ automatic: true }) },
    );
  }
  setRunStatus('Stream disconnected · run still active', 'busy');
  addNotice(
    'warning',
    'The event stream was lost. The run is still active; reconnect to receive subsequent events. Earlier gaps cannot be recovered.',
    'stream-lost-active',
    { label: 'Reconnect stream', run: () => attachStream({ automatic: true }) },
  );
}

/* Wiring */

dom.openSettings.addEventListener('click', () => {
  dom.settings.showModal();
  requestAnimationFrame(() => {
    if (!state.connected) dom.token.focus();
    else dom.settings.querySelector('#theme')?.focus();
  });
});
dom.closeSettings.addEventListener('click', () => dom.settings.close());
dom.settings.addEventListener('close', () => {
  dom.token.value = '';
});

dom.connectionForm.addEventListener('submit', (event) => {
  event.preventDefault();
  void connect(dom.token.value.trim() || state.credential);
});
dom.disconnect.addEventListener('click', disconnect);

dom.model.addEventListener('change', () => {
  const index = Number(dom.model.value);
  state.selection = state.choices[index] || null;
  updateModelDisplay();
  updateControls();
});

dom.message.addEventListener('input', updateControls);
dom.message.addEventListener('keydown', (event) => {
  if (event.key !== 'Enter' || event.shiftKey || event.isComposing || event.keyCode === 229) return;
  event.preventDefault();
  dom.composer.requestSubmit();
});
dom.composer.addEventListener('submit', (event) => {
  event.preventDefault();
  void sendMessage();
});

dom.cancel.addEventListener('click', () => void cancelRun());
dom.newSession.addEventListener('click', () => startNewSession());
dom.newSessionMobile.addEventListener('click', () => startNewSession());

dom.main.addEventListener('scroll', () => {
  state.pinned = nearBottom();
});

window.addEventListener('pagehide', () => {
  flushText();
  stopStream();
});

window.addEventListener('pageshow', (event) => {
  if (event.persisted && state.connected && state.active) void resumeIfNeeded();
});

window.addEventListener('beforeunload', (event) => {
  if (!state.active && !state.inFlightUnknown) return;
  event.preventDefault();
  event.returnValue = '';
});

async function cancelRun() {
  if (!state.session || !state.run || !state.active || state.cancelling || state.cancelRequested) return;
  const session = state.session;
  const run = state.run;
  state.cancelling = true;
  setRunStatus('Cancelling…', 'busy');
  updateSessionDisplay();
  updateControls();
  try {
    const reply = await api(
      `/sessions/${encodeURIComponent(session)}/runs/${encodeURIComponent(run)}/cancel`,
      { method: 'POST', body: {} },
    );
    state.cancelRequested = true;
    state.cancelling = false;
    if (state.session === session && state.run === run && !state.runTerminal) {
      setRunStatus(reply?.reply === 'accepted' ? 'Cancellation requested' : 'Run is no longer active', 'busy');
    }
  } catch (error) {
    state.cancelling = false;
    if (state.session !== session || state.run !== run || state.runTerminal) {
      updateSessionDisplay();
      updateControls();
      return;
    }
    if (error instanceof ApiError) {
      setRunStatus(error.status === 409 ? 'Run already finished' : 'Cancel was not accepted', 'error');
      addNotice('warning', `Cancel request was not accepted: ${error.message}`, null);
      if (error.status === 401) markUnauthorized();
      if (error.status === 404 || error.status === 409) await reconcileAfterLoss(error);
    } else {
      state.cancelRequested = true;
      setRunStatus('Cancel outcome unknown', 'error');
      addNotice(
        'error',
        'The cancellation outcome is unknown. The run may still be active; reconnect to observe it. Do not assume cancellation succeeded.',
        'cancel-unknown',
      );
    }
  }
  updateSessionDisplay();
  updateControls();
}

/* Initial state */

updateSessionDisplay();
updateControls();
setConnectionStatus('Connecting…', 'busy');
setRunStatus('Connecting…', 'busy');
void connect('');
