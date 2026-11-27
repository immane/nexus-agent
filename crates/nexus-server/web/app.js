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
  settingsButtons: [...document.querySelectorAll('[data-settings-section]')],
  settingsPanels: [...document.querySelectorAll('.settings-panel')],
  settingsContent: document.querySelector('.settings-content'),
  serverVersion: document.querySelector('#server-version'),
  installApp: document.querySelector('#install-app'),
  installNote: document.querySelector('#install-note'),
  connectionForm: document.querySelector('#connection-form'),
  token: document.querySelector('#token'),
  connect: document.querySelector('#connect'),
  disconnect: document.querySelector('#disconnect'),
  connectionStatus: document.querySelector('#connection-status'),
  connectionDetail: document.querySelector('#connection-detail'),
  thinkingForm: document.querySelector('#thinking-form'),
  taskThinking: document.querySelector('#task-thinking'),
  saveThinking: document.querySelector('#save-thinking'),
  thinkingStatus: document.querySelector('#thinking-status'),
  project: document.querySelector('#project'),
  composerProject: document.querySelector('.composer-project'),
  projectLabel: document.querySelector('#project-label'),
  projectBadge: document.querySelector('#project-badge'),
  sidebarTree: document.querySelector('#sidebar-tree'),
  sidebar: document.querySelector('#sidebar'),
  sidebarBackdrop: document.querySelector('#sidebar-backdrop'),
  toggleSidebar: document.querySelector('#toggle-sidebar'),
  addProjectSidebar: document.querySelector('#add-project-sidebar'),
  projectsNote: document.querySelector('#projects-note'),
  projectList: document.querySelector('#project-list'),
  projectForm: document.querySelector('#project-form'),
  projectName: document.querySelector('#project-name'),
  projectPath: document.querySelector('#project-path'),
  chooseProjectFolder: document.querySelector('#choose-project-folder'),
  addProject: document.querySelector('#add-project'),
  projectStatus: document.querySelector('#project-status'),
  providerList: document.querySelector('#provider-list'),
  providerForm: document.querySelector('#provider-form'),
  providerFields: document.querySelector('#provider-fields'),
  providerEditorTitle: document.querySelector('#provider-editor-title'),
  providerEditorMode: document.querySelector('#provider-editor-mode'),
  providerAdvanced: document.querySelector('#provider-advanced'),
  cancelProvider: document.querySelector('#cancel-provider'),
  providerCredentialLabel: document.querySelector('#provider-credential-label'),
  providerCredentialHint: document.querySelector('#provider-credential-hint'),
  providerId: document.querySelector('#provider-id'),
  providerDisplay: document.querySelector('#provider-display'),
  providerAdapter: document.querySelector('#provider-adapter'),
  providerProtocol: document.querySelector('#provider-protocol'),
  providerEndpoint: document.querySelector('#provider-endpoint'),
  providerEnabled: document.querySelector('#provider-enabled'),
  providerCredentialKind: document.querySelector('#provider-credential-kind'),
  providerCredentialName: document.querySelector('#provider-credential-name'),
  providerTokenField: document.querySelector('#provider-token-field'),
  providerApiToken: document.querySelector('#provider-api-token'),
  providerDefaultModel: document.querySelector('#provider-default-model'),
  providerContextWindow: document.querySelector('#provider-context-window'),
  providerTaskThinking: document.querySelector('#provider-task-thinking'),
  saveProvider: document.querySelector('#save-provider'),
  newProvider: document.querySelector('#new-provider'),
  deleteProvider: document.querySelector('#delete-provider'),
  providerDeleteHint: document.querySelector('#provider-delete-hint'),
  providerStatus: document.querySelector('#provider-status'),
  modelList: document.querySelector('#model-list'),
  modelForm: document.querySelector('#model-form'),
  modelFields: document.querySelector('#model-fields'),
  modelEditorTitle: document.querySelector('#model-editor-title'),
  modelEditorMode: document.querySelector('#model-editor-mode'),
  modelAdvanced: document.querySelector('#model-advanced'),
  cancelModel: document.querySelector('#cancel-model'),
  modelProviderGuide: document.querySelector('#model-provider-guide'),
  modelAddProvider: document.querySelector('#model-add-provider'),
  modelId: document.querySelector('#model-id'),
  modelProvider: document.querySelector('#model-provider'),
  modelName: document.querySelector('#model-name'),
  modelContextWindow: document.querySelector('#model-context-window'),
  modelMaxOutput: document.querySelector('#model-max-output'),
  modelTaskThinking: document.querySelector('#model-task-thinking'),
  saveModel: document.querySelector('#save-model'),
  newModel: document.querySelector('#new-model'),
  modelStatus: document.querySelector('#model-status'),
  model: document.querySelector('#model'),
  modelLabel: document.querySelector('#model-label'),
  message: document.querySelector('#message'),
  composer: document.querySelector('#composer'),
  promptInbox: document.querySelector('#prompt-inbox'),
  steerRun: document.querySelector('#steer-run'),
  sideQuestion: document.querySelector('#side-question'),
  sideQuestionAnswer: document.querySelector('#side-question-answer'),
  sideQuestionUsage: document.querySelector('#side-question-usage'),
  imageLightbox: document.querySelector('#image-lightbox'),
  imageLightboxImage: document.querySelector('#image-lightbox-image'),
  composerActivity: document.querySelector('#composer-activity'),
  send: document.querySelector('#send'),
  cancel: document.querySelector('#cancel-run'),
  jumpLatest: document.querySelector('#jump-latest'),
  runStatus: document.querySelector('#run-status'),
  toolsMode: document.querySelector('#tools-mode'),
  appBackdrop: document.querySelector('#app-backdrop'),
  backgroundPreview: document.querySelector('#background-preview'),
  backgroundFile: document.querySelector('#background-file'),
  backgroundChoose: document.querySelector('#background-choose'),
  backgroundRemove: document.querySelector('#background-remove'),
  backgroundDim: document.querySelector('#background-dim'),
  backgroundDimValue: document.querySelector('#background-dim-value'),
  backgroundBlur: document.querySelector('#background-blur'),
  backgroundBlurValue: document.querySelector('#background-blur-value'),
  backgroundPalette: document.querySelector('#background-palette'),
  backgroundPaletteStatus: document.querySelector('#background-palette-status'),
  backgroundPaletteModes: document.querySelector('#background-palette-modes'),
  backgroundPaletteSwatches: document.querySelector('#background-palette-swatches'),
  backgroundHistory: document.querySelector('#background-history'),
  backgroundHistoryList: document.querySelector('#background-history-list'),
  backgroundStatus: document.querySelector('#background-status'),
  backgroundHistoryCount: document.querySelector('#background-history-count'),
  interfaceSettings: document.querySelector('#interface-settings'),
  interfaceReset: document.querySelector('#interface-reset'),
  contentSize: document.querySelector('#content-size'),
  contentSizeValue: document.querySelector('#content-size-value'),
  helperStatus: document.querySelector('#helper-status'),
  contextMeter: document.querySelector('#context-meter'),
  contextRing: document.querySelector('#context-ring'),
  contextPercent: document.querySelector('#context-percent'),
  contextPanel: document.querySelector('#context-panel'),
  contextPanelPercent: document.querySelector('#context-panel-percent'),
  contextPanelTotal: document.querySelector('#context-panel-total'),
  contextBarFill: document.querySelector('#context-bar-fill'),
  contextBarTrigger: document.querySelector('#context-bar-trigger'),
  contextRows: document.querySelector('#context-rows'),
  modelTrigger: document.querySelector('#model-trigger'),
  modelTriggerName: document.querySelector('#model-trigger-name'),
  modelTriggerEffort: document.querySelector('#model-trigger-effort'),
  modelMenu: document.querySelector('#model-menu'),
  menuMain: document.querySelector('#menu-main'),
  menuModels: document.querySelector('#menu-models'),
  menuEfforts: document.querySelector('#menu-efforts'),
  menuModel: document.querySelector('#menu-model'),
  menuModelValue: document.querySelector('#menu-model-value'),
  menuModelOptions: document.querySelector('#menu-model-options'),
  menuEffort: document.querySelector('#menu-effort'),
  menuEffortValue: document.querySelector('#menu-effort-value'),
  menuEffortOptions: document.querySelector('#menu-effort-options'),
  menuEffortHelp: document.querySelector('#menu-effort-help'),
  teamworkSwitch: document.querySelector('#teamwork-switch'),
  teamworkInfo: document.querySelector('#teamwork-info'),
  teamworkHelp: document.querySelector('#teamwork-help'),
  main: document.querySelector('#main'),
  sessionLoading: document.querySelector('#session-loading'),
  welcome: document.querySelector('#welcome'),
  welcomeContext: document.querySelector('#welcome-context'),
  welcomeConnect: document.querySelector('#welcome-connect'),
  treeLabel: document.querySelector('#tree-label'),
  conversation: document.querySelector('#conversation'),
  conversationTitle: document.querySelector('#conversation-title'),
  newSession: document.querySelector('#new-session'),
  newSessionMobile: document.querySelector('#new-session-mobile'),
};

const state = {
  connected: false,
  connecting: false,
  credential: '',
  config: null,
  thinkingSaving: false,
  configSaving: false,
  choices: [],
  selection: null,
  projects: null,
  projectChoices: [],
  projectSelection: null,
  projectPicking: false,
  projectSaving: false,
  sessionProjectId: null,
  collapsedProjects: new Set(),
  savedSessions: [],
  savedPersistence: false,
  historyLoading: false,
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
  inbox: null,
  askingAside: false,
  runTerminal: false,
  streaming: false,
  streamController: null,
  streamGeneration: 0,
  reconnectTimer: 0,
  retries: 0,
  lastSeq: null,
  assistantItems: new Map(),
  reasoningItems: new Map(),
  tools: new Map(),
  approvals: new Map(),
  pendingApprovalSeen: false,
  noticeKeys: new Set(),
  notices: new Map(),
  lastOutcome: null,
  lastUsage: null,
  // Latest task-request estimate for the current session (not provider usage).
  context: null,
  // Background helper jobs started by the current run, keyed by job ID.
  delegateJobs: new Map(),
  // Browser preference for new bindings; the session keeps what it was bound with.
  teamwork: true,
  sessionDelegation: null,
  sessionDelegationActive: null,
  pinned: true,
  connectGeneration: 0,
};

const pendingText = new Map();
let textFrame = 0;

/* Appearance */

const themes = new Set([
  'system',
  'graphite', 'midnight', 'forest', 'ocean', 'amethyst', 'ink', 'fjord', 'twilight', 'mocha', 'ember', 'dusk',
  'paper', 'sand', 'blossom', 'lavender', 'mint', 'sky', 'latte', 'peach', 'solar',
]);

function applyTheme(value) {
  const selected = themes.has(value) ? value : 'system';
  if (selected === 'system') delete dom.root.dataset.theme;
  else dom.root.dataset.theme = selected;
  for (const input of dom.theme.querySelectorAll('input[name="theme"]')) {
    input.checked = input.value === selected;
  }
}

try {
  applyTheme(localStorage.getItem('nexus.theme'));
} catch {
  applyTheme('system');
}

dom.theme.addEventListener('change', (event) => {
  const input = event.target;
  if (input.name !== 'theme' || !input.checked || !themes.has(input.value)) return;
  applyTheme(input.value);
  try {
    localStorage.setItem('nexus.theme', input.value);
  } catch {
    // Appearance remains usable when browser storage is unavailable.
  }
});

/* Installation */

let deferredInstallPrompt = null;

function runningStandalone() {
  return window.matchMedia?.('(display-mode: standalone)')?.matches === true
    || window.navigator.standalone === true;
}

function updateInstallDisplay(message) {
  if (!dom.installApp || !dom.installNote) return;
  if (message) {
    dom.installApp.hidden = true;
    dom.installNote.textContent = message;
    return;
  }
  if (runningStandalone()) {
    dom.installApp.hidden = true;
    dom.installNote.textContent = 'Nexus is running as an installed app. The local server must still be running; this browser remembers your connection.';
  } else if (deferredInstallPrompt) {
    dom.installApp.hidden = false;
    dom.installNote.textContent = 'Install Nexus as a standalone window. The local server must still be running; this browser remembers your connection.';
  } else {
    dom.installApp.hidden = true;
    dom.installNote.textContent = 'Use your browser\'s Install or Add to Home Screen action. The local server must still be running; this browser remembers your connection.';
  }
}

window.addEventListener('beforeinstallprompt', (event) => {
  event.preventDefault();
  deferredInstallPrompt = event;
  updateInstallDisplay();
});

window.addEventListener('appinstalled', () => {
  deferredInstallPrompt = null;
  updateInstallDisplay('Nexus is installed. Open it from your app launcher or Dock. Connect once if the installed app uses separate browser storage.');
});

dom.installApp?.addEventListener('click', async () => {
  const prompt = deferredInstallPrompt;
  if (!prompt || dom.installApp.disabled) return;
  deferredInstallPrompt = null;
  dom.installApp.disabled = true;
  try {
    prompt.prompt();
    const choice = await prompt.userChoice;
    updateInstallDisplay(choice.outcome === 'accepted'
      ? 'Installing Nexus…'
      : 'Install prompt dismissed. Use the browser menu to install later.');
  } catch {
    updateInstallDisplay('The install prompt could not be opened. Use the browser menu instead.');
  } finally {
    dom.installApp.disabled = false;
  }
});

window.matchMedia?.('(display-mode: standalone)').addEventListener?.('change', () => updateInstallDisplay());
updateInstallDisplay();

if ('serviceWorker' in navigator) {
  navigator.serviceWorker.register('/sw.js', { scope: '/' }).catch(() => {
    // The workspace remains usable when installation is unavailable.
  });
}

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
  dom.modelLabel.textContent = describeModelLabel();
  updateWelcome();
  updateModelTrigger();
}

// The empty state summarizes what the next task will use instead of
// repeating it in the top bar.
function updateWelcome() {
  dom.welcomeConnect.hidden = state.connected || state.connecting;
  if (!state.connected) {
    dom.welcomeContext.textContent = state.connecting
      ? 'Connecting to your local Nexus server…'
      : 'Connect to your local Nexus server to start.';
    return;
  }
  const parts = [];
  if (state.projects?.enabled) parts.push(state.projectSelection?.label || 'No project');
  parts.push(describeModelLabel());
  dom.welcomeContext.textContent = `Next task uses ${parts.join(' · ')}.`;
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
  return dom.main.scrollHeight - dom.main.scrollTop - dom.main.clientHeight < 48;
}

function scrollToBottom() {
  dom.main.scrollTop = dom.main.scrollHeight;
  updateJumpLatest();
}

function scrollToBottomSoon() {
  if (!state.pinned) return;
  scrollToBottom();
  requestAnimationFrame(() => {
    if (state.pinned) scrollToBottom();
  });
  setTimeout(() => {
    if (state.pinned) scrollToBottom();
  }, 120);
}

function updateJumpLatest() {
  dom.jumpLatest.hidden = state.pinned
    || dom.conversation.hidden
    || dom.main.scrollHeight <= dom.main.clientHeight + 1;
}

function flushText() {
  if (textFrame) {
    cancelAnimationFrame(textFrame);
    textFrame = 0;
  }
  if (!pendingText.size) return;
  for (const [textNode, text] of pendingText) textNode.appendData(text);
  pendingText.clear();
  for (const record of state.reasoningItems.values()) scrollReasoning(record);
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
  dom.conversation.append(element);
  dom.welcome.hidden = true;
  dom.conversation.hidden = false;
  if (state.pinned) scrollToBottom();
  updateJumpLatest();
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
  state.savedPersistence = false;
  state.savedSessions = [];
  if (readSavedCredential() === state.credential) saveCredential('');
  state.credential = '';
  setConnectionStatus('Authentication required', 'error');
  dom.connectionDetail.textContent = 'Authentication failed. Enter the current server bearer token.';
  setRunStatus('Authentication required', 'error');
  renderSidebarTree();
  updateControls();
}

/* Configuration and model choices */

function buildChoices(config) {
  const models = Array.isArray(config?.models) ? config.models : [];
  const disabledProviders = new Set(
    (Array.isArray(config?.providers) ? config.providers : [])
      .filter((provider) => provider && provider.enabled === false)
      .map((provider) => provider.id),
  );
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
    if (disabledProviders.has(entry.provider)) continue;
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

function buildProjectChoices(projects) {
  if (!projects?.enabled) return [];
  const choices = [];
  if (projects.default && typeof projects.default.id === 'string') {
    choices.push({
      demo: false,
      id: projects.default.id,
      label: projects.default.name || 'Startup workspace',
      path: typeof projects.default.path === 'string' ? projects.default.path : '',
    });
  }
  if (Array.isArray(projects.projects)) {
    for (const project of projects.projects) {
      if (!project || typeof project.id !== 'string') continue;
      choices.push({
        demo: false,
        id: project.id,
        label: typeof project.name === 'string' ? project.name : project.id,
        path: typeof project.path === 'string' ? project.path : '',
      });
    }
  }
  return choices;
}

function populateProjects() {
  const previous = state.projectSelection?.id || '';
  state.projectChoices = buildProjectChoices(state.projects);
  state.projectSelection = state.projectChoices.find((choice) => choice.id === previous)
    || state.projectChoices[0]
    || null;
  dom.project.replaceChildren();
  state.projectChoices.forEach((choice, index) => {
    const option = node('option', '', choice.path ? `${choice.label} — ${choice.path}` : choice.label);
    option.value = String(index);
    dom.project.append(option);
  });
  if (state.projectSelection) {
    dom.project.value = String(state.projectChoices.indexOf(state.projectSelection));
  }
  updateProjectDisplay();
}

function updateProjectDisplay() {
  const enabled = Boolean(state.projects?.enabled);
  const current = state.session && enabled
    ? state.projectChoices.find((choice) => choice.id === state.sessionProjectId) || null
    : null;
  const selected = state.session ? current : state.projectSelection;
  const label = selected
    ? selected.label
    : enabled ? 'No project' : 'Projects disabled';
  dom.projectLabel.textContent = state.session && enabled && !current ? 'No project' : label;
  dom.projectBadge.textContent = enabled && selected ? selected.label : 'No project';
  dom.projectBadge.hidden = !enabled;
  dom.projectBadge.title = selected?.path || '';
}

const COLLAPSED_PROJECTS_KEY = 'nexus.collapsed-projects';
const UNASSIGNED_PROJECT_KEY = 'unassigned';

function projectCollapseKey(id) {
  return id ? `project:${id}` : UNASSIGNED_PROJECT_KEY;
}

function readCollapsedProjects() {
  try {
    const value = JSON.parse(localStorage.getItem(COLLAPSED_PROJECTS_KEY) || '[]');
    return new Set(Array.isArray(value) ? value.filter((entry) => typeof entry === 'string') : []);
  } catch {
    return new Set();
  }
}

function writeCollapsedProjects() {
  try {
    localStorage.setItem(COLLAPSED_PROJECTS_KEY, JSON.stringify([...state.collapsedProjects]));
  } catch {
    // Expansion state is a convenience; rendering remains usable without it.
  }
}

function ensureProjectExpanded(id) {
  if (!state.collapsedProjects.delete(projectCollapseKey(id))) return;
  writeCollapsedProjects();
}

// Navigation labels show the words of a prompt, not its Markdown markers.
// Single `*`/`_` stay because they are common in identifiers and globs.
function plainTitle(text) {
  return String(text || '')
    .replace(/!?\[([^\]]*)\]\([^)]*\)/g, '$1')
    .replace(/(\*\*|__|~~|`)/g, '')
    .replace(/^\s*(#{1,6}|>|[-*+])\s+/, '')
    .replace(/\s+/g, ' ')
    .trim();
}

function compareSessionEntries(left, right) {
  if (left.current !== right.current) return left.current ? -1 : 1;
  const messages = (right.messages || 0) - (left.messages || 0);
  if (messages) return messages;
  return String(left.title || '').localeCompare(String(right.title || ''));
}

function buildSessionGroups() {
  const enabled = Boolean(state.projects?.enabled);
  const groups = [];
  const byId = new Map();
  if (enabled) {
    for (const choice of state.projectChoices) {
      const group = {
        id: choice.id,
        label: choice.label,
        path: choice.path || '',
        sessions: [],
      };
      groups.push(group);
      byId.set(choice.id, group);
    }
  }
  const unassigned = {
    id: '',
    label: enabled ? 'No project' : 'Sessions',
    path: '',
    sessions: [],
  };
  const savedById = new Map();
  for (const saved of state.savedSessions) {
    if (saved && typeof saved.session === 'string') savedById.set(saved.session, saved);
  }
  const entries = [];
  if (state.session) {
    const saved = savedById.get(state.session);
    savedById.delete(state.session);
    entries.push({
      session: state.session,
      title: state.sessionTitle || saved?.title || 'New conversation',
      messages: Number(saved?.messages) || 0,
      active: state.active,
      current: true,
      saved: Boolean(saved),
      project: enabled ? (state.sessionProjectId || '') : '',
    });
  }
  for (const saved of savedById.values()) {
    const meta = savedSessionMeta(saved.session) || {};
    const project = typeof meta.project === 'string' && meta.project
      ? meta.project
      : typeof saved.project === 'string' ? saved.project : '';
    entries.push({
      session: saved.session,
      title: plainTitle(saved.title || (typeof meta.title === 'string' ? meta.title : '')) || 'Saved session',
      messages: Number(saved.messages) || 0,
      active: saved.active === true,
      current: false,
      saved: true,
      project: enabled ? project : '',
    });
  }
  for (const entry of entries) {
    const group = enabled ? byId.get(entry.project) : null;
    (group || unassigned).sessions.push(entry);
  }
  for (const group of groups) group.sessions.sort(compareSessionEntries);
  unassigned.sessions.sort(compareSessionEntries);
  const selected = enabled ? (state.sessionProjectId || '') : '';
  return { groups, unassigned, currentProject: byId.has(selected) ? selected : '' };
}

function sessionTreeMeta(entry) {
  if (entry.current) return `${describeModelLabel()} · ${currentSessionStateLabel()}`;
  return `${entry.messages || 0} items · ${entry.active ? 'open' : 'saved'}`;
}

function appendSessionTreeItem(panel, entry, savedBusy) {
  const item = node('div', 'tree-session');
  item.dataset.session = entry.session;
  item.dataset.state = entry.current
    ? state.active ? 'active' : 'current'
    : entry.active ? 'open' : 'idle';
  const open = node('button', 'tree-session-open');
  open.type = 'button';
  open.disabled = savedBusy;
  const meta = sessionTreeMeta(entry);
  open.title = `${entry.title || 'Saved session'}\n${meta}`;
  open.append(node('span', 'tree-session-title', entry.title || 'Saved session'));
  // Rows stay single-line; the meta text remains available to assistive tech.
  open.append(node('span', 'tree-session-meta', meta));
  if (entry.current) open.setAttribute('aria-current', 'true');
  open.addEventListener('click', () => void openSavedSession(entry.session));
  item.append(open);
  if (entry.saved) {
    const remove = node('button', 'tree-session-delete', '×');
    remove.type = 'button';
    remove.title = 'Delete saved session';
    remove.setAttribute('aria-label', 'Delete saved session');
    remove.disabled = savedBusy;
    remove.addEventListener('click', () => void deleteSavedSession(entry.session));
    item.append(remove);
  }
  panel.append(item);
}

function renderProjectGroup(group, index, { savedBusy, projectBusy, currentProject }) {
  const expanded = !state.collapsedProjects.has(projectCollapseKey(group.id));
  const panelId = `project-sessions-${index}`;
  const section = node('section', 'tree-project');
  section.dataset.project = group.id;
  const row = node('div', 'tree-project-row');
  const current = Boolean(state.session) && group.id === currentProject;
  const selected = !state.session && group.id === (state.projectSelection?.id || '');
  row.dataset.state = current ? 'current' : selected ? 'selected' : 'idle';

  const toggle = node('button', 'tree-project-toggle');
  toggle.type = 'button';
  toggle.disabled = !state.connected || projectBusy;
  toggle.setAttribute('aria-expanded', String(expanded));
  toggle.setAttribute('aria-controls', panelId);
  if (group.path) toggle.title = group.path;
  const folder = node('span', 'tree-folder');
  folder.setAttribute('aria-hidden', 'true');
  toggle.append(folder);
  toggle.append(node('span', 'tree-project-name', group.label));
  toggle.addEventListener('click', () => toggleProjectGroup(group.id));
  row.append(toggle);

  if (group.id) {
    const add = node('button', 'tree-project-add');
    add.type = 'button';
    add.disabled = !state.connected || projectBusy;
    add.title = `New session in ${group.label}`;
    add.setAttribute('aria-label', `New session in ${group.label}`);
    add.addEventListener('click', () => startSessionInProject(group.id));
    row.append(add);
  }

  const panel = node('div', 'tree-sessions');
  panel.id = panelId;
  panel.setAttribute('role', 'group');
  panel.setAttribute('aria-label', `Sessions in ${group.label}`);
  panel.hidden = !expanded;
  if (!group.sessions.length) {
    panel.append(node('p', 'tree-empty', 'No sessions yet.'));
  } else {
    for (const entry of group.sessions) appendSessionTreeItem(panel, entry, savedBusy);
  }
  section.append(row, panel);
  return section;
}

function renderSidebarTree() {
  dom.sidebarTree.replaceChildren();
  const projectsEnabled = Boolean(state.projects?.enabled);
  dom.treeLabel.textContent = state.connected && !projectsEnabled ? 'Sessions' : 'Projects';
  if (!state.connected) {
    dom.sidebarTree.append(node('p', 'sidebar-empty', 'Connect to load projects.'));
    return;
  }
  const { groups, unassigned, currentProject } = buildSessionGroups();
  const visible = [...groups];
  if (unassigned.sessions.length) visible.push(unassigned);

  const savedBusy = state.active || state.submitting || state.connecting || state.historyLoading;
  const projectBusy = state.active || state.submitting || state.connecting;
  if (projectsEnabled) {
    visible.forEach((group, index) => {
      dom.sidebarTree.append(renderProjectGroup(group, index, { savedBusy, projectBusy, currentProject }));
    });
  } else {
    // Without projects there is only one group, so its header adds nothing.
    const list = node('div', 'tree-sessions tree-flat');
    list.setAttribute('role', 'group');
    list.setAttribute('aria-label', 'Sessions');
    for (const entry of unassigned.sessions) appendSessionTreeItem(list, entry, savedBusy);
    if (unassigned.sessions.length) dom.sidebarTree.append(list);
  }
  if (!visible.length) {
    dom.sidebarTree.append(node(
      'p',
      'sidebar-empty',
      projectsEnabled ? 'No projects available.' : 'Sessions will appear here after you start one.',
    ));
  }
  if (!state.savedPersistence) {
    const note = node('p', 'tree-note', 'History is off. Start the server with ');
    note.append(node('code', '', '--session-store DIR'), document.createTextNode(' to keep sessions.'));
    dom.sidebarTree.append(note);
  }
}

function updateSidebarProjectControls() {
  const busy = state.active || state.submitting || state.connecting;
  for (const button of dom.sidebarTree.querySelectorAll('.tree-project-toggle')) {
    button.disabled = !state.connected || busy;
  }
  for (const button of dom.sidebarTree.querySelectorAll('.tree-project-add')) {
    button.disabled = !state.connected || busy;
  }
  dom.addProjectSidebar.disabled = !state.connected || !state.projects?.enabled;
  dom.addProjectSidebar.hidden = state.connected && !state.projects?.enabled;
}

function selectProjectChoice(id) {
  const choice = state.projectChoices.find((entry) => entry.id === id);
  if (!choice) return false;
  state.projectSelection = choice;
  populateProjects();
  return true;
}

function toggleProjectGroup(id) {
  if (!state.connected || state.active || state.submitting || state.connecting) return;
  const key = projectCollapseKey(id);
  const collapsed = !state.collapsedProjects.has(key);
  if (collapsed) state.collapsedProjects.add(key);
  else state.collapsedProjects.delete(key);
  writeCollapsedProjects();
  for (const section of dom.sidebarTree.querySelectorAll('.tree-project')) {
    if ((section.dataset.project || '') !== id) continue;
    const toggle = section.querySelector('.tree-project-toggle');
    const panel = section.querySelector('.tree-sessions');
    if (toggle) toggle.setAttribute('aria-expanded', String(!collapsed));
    if (panel) panel.hidden = collapsed;
    break;
  }
  updateControls();
}

function startSessionInProject(id) {
  if (!state.connected || state.active || state.submitting || state.connecting) return;
  const choice = state.projectChoices.find((entry) => entry.id === id);
  if (!choice) return;
  if (state.session) {
    if (!window.confirm(`Start a new session in ${choice.label}? The current session remains available in saved history.`)) {
      return;
    }
    state.projectSelection = choice;
    ensureProjectExpanded(id);
    startNewSession({ force: true });
    return;
  }
  selectProjectChoice(id);
  ensureProjectExpanded(id);
  renderSidebarTree();
  updateSessionDisplay();
  updateControls();
  dom.message.focus();
}

function setFormStatus(element, text, stateName) {
  setStatus(element, text, stateName);
}

function optionalCount(input, label) {
  const raw = input.value.trim();
  if (!raw) return null;
  const value = Number(raw);
  if (!Number.isInteger(value) || value <= 0) throw new Error(`${label} must be a positive integer.`);
  return value;
}

function updateConnectionDetail() {
  if (!state.connected) return;
  const providers = Array.isArray(state.config?.providers) ? state.config.providers.length : 0;
  const models = Array.isArray(state.config?.models) ? state.config.models.length : 0;
  dom.connectionDetail.textContent = models
    ? `Connected. ${providers} provider${providers === 1 ? '' : 's'} and ${models} model${models === 1 ? '' : 's'} available.`
    : 'Connected. No selectable models are configured; the demo script remains available.';
}

function configModeLabel(mode) {
  switch (mode) {
    case 'development':
      return 'Development tools';
    case 'strict':
      return 'Strict tools';
    case 'container':
      return 'Container tools';
    case 'fakes':
      return 'Scripted demo tools';
    default:
      return 'Unknown tool mode';
  }
}

function renderProjectsList() {
  dom.projectList.replaceChildren();
  const enabled = Boolean(state.projects?.enabled);
  updateProjectFormControls();
  if (!enabled) {
    dom.projectsNote.textContent = state.projects?.desktop_project_picker === true
      ? 'Local tools are off. Open a project to enable them for that folder.'
      : state.projects?.mode === 'container'
      ? `Container tools are active. Tool network ${state.projects.network_access === false ? 'disabled' : state.projects.network_access === true ? 'enabled (experimental public-egress gateway)' : 'not reported'}. Per-project roots are not available in container mode.`
      : 'Real tools are not enabled. Start nexus-server with --tools real --tools-root PATH to bind projects.';
    if (state.projects?.desktop_project_picker === true) {
      const open = node('a', '', 'Open Project…');
      open.href = 'nexus-agent://open-project';
      dom.projectList.append(open);
    }
    dom.projectStatus.textContent = '';
    return;
  }
  const network = state.projects.network_access === false ? 'disabled'
    : state.projects.network_access === true ? 'enabled (host reachability)' : 'not reported';
  dom.projectsNote.textContent = `${configModeLabel(state.projects.mode)} · tool network ${network} · sessions use the selected root for project/temp tools.`;
  const addItem = (project, removable) => {
    const item = node('div', 'config-item');
    const text = node('div', 'config-item-text');
    text.append(node('span', 'config-item-title', project.name));
    text.append(node('span', 'config-item-meta', `${project.id} · ${project.path}`));
    item.append(text);
    if (removable) {
      const actions = node('div', 'config-item-actions');
      const remove = node('button', '', 'Remove');
      remove.type = 'button';
      remove.addEventListener('click', async () => {
        remove.disabled = true;
        try {
          const payload = await api(`/projects/${encodeURIComponent(project.id)}`, { method: 'DELETE' });
          state.projects = payload;
          populateProjects();
          renderProjectsList();
          renderSidebarTree();
          updateControls();
          updateSessionDisplay();
          setFormStatus(dom.projectStatus, `Removed ${project.name}.`, 'ok');
        } catch (error) {
          setFormStatus(dom.projectStatus, error.message, 'error');
          if (error instanceof ApiError && error.status === 401) markUnauthorized();
          remove.disabled = false;
        }
      });
      actions.append(remove);
      item.append(actions);
    }
    dom.projectList.append(item);
  };
  if (state.projects.default) {
    addItem({
      id: state.projects.default.id,
      name: state.projects.default.name,
      path: state.projects.default.path,
    }, false);
  }
  for (const project of state.projects.projects || []) addItem(project, true);
}

function updateProjectFormControls() {
  const busy = Boolean(state.projectPicking || state.projectSaving);
  const available = state.connected && state.projects?.enabled;
  dom.projectName.disabled = dom.projectPath.disabled = !available || busy;
  dom.chooseProjectFolder.disabled = !available || busy;
  dom.chooseProjectFolder.textContent = state.projectPicking ? 'Choosing folder…' : 'Choose folder…';
  dom.addProject.disabled = !available || busy || !dom.projectPath.value;
  dom.addProject.textContent = state.projectSaving ? 'Adding…' : 'Add project';
  dom.projectForm.setAttribute('aria-busy', String(busy));
}

async function chooseProjectFolder() {
  if (!state.connected || !state.projects?.enabled || state.projectPicking || state.projectSaving) return;
  const generation = state.connectGeneration;
  state.projectPicking = true;
  updateControls();
  setFormStatus(dom.projectStatus, 'Choose a folder in the dialog on the server’s desktop…', 'busy');
  try {
    const result = await api('/projects/pick-folder', { method: 'POST', body: {} });
    if (!state.connected || generation !== state.connectGeneration) return;
    if (result?.cancelled) {
      setFormStatus(dom.projectStatus, 'Folder selection cancelled.');
      return;
    }
    if (typeof result?.path !== 'string' || !result.path || typeof result?.name !== 'string' || !result.name) {
      throw new Error('The server did not return a project folder.');
    }
    dom.projectPath.value = result.path;
    dom.projectName.placeholder = result.name;
    setFormStatus(dom.projectStatus, `Selected ${result.name}. Add the project when ready.`, 'ok');
  } catch (error) {
    if (generation === state.connectGeneration) setFormStatus(dom.projectStatus, error.message, 'error');
  } finally {
    state.projectPicking = false;
    updateControls();
  }
}

function renderProviderList() {
  dom.providerList.replaceChildren();
  const providers = Array.isArray(state.config?.providers) ? state.config.providers : [];
  if (!providers.length) {
    dom.providerList.append(node('p', 'config-empty', 'No providers configured. The demo script is still available.'));
  }
  for (const provider of providers) {
    const item = node('div', 'config-item');
    item.dataset.selected = String(dom.providerForm.dataset.original === provider.id && !dom.providerForm.hidden);
    const text = node('div', 'config-item-text');
    text.append(node('span', 'config-item-title', provider.display_name || provider.id));
    const count = state.config?.models?.filter((model) => model.provider === provider.id).length || 0;
    const availability = provider.enabled === false ? 'Disabled' : 'Enabled';
    text.append(node(
      'span',
      'config-item-meta',
      `${provider.id} · ${availability} · ${count} model${count === 1 ? '' : 's'}`,
    ));
    text.append(node('span', 'config-item-meta', `${provider.protocol} · ${provider.endpoint || 'Default endpoint'}`));
    item.append(text);
    const actions = node('div', 'config-item-actions');
    const edit = node('button', '', 'Edit');
    edit.type = 'button';
    edit.setAttribute('aria-label', `Edit provider ${provider.display_name || provider.id}`);
    edit.setAttribute('aria-pressed', item.dataset.selected);
    edit.addEventListener('click', () => editProvider(provider.id));
    const addModel = node('button', '', '＋ Model');
    addModel.type = 'button';
    addModel.setAttribute('aria-label', `Add model for ${provider.display_name || provider.id}`);
    addModel.addEventListener('click', () => {
      if (!resetModelForm({ open: true })) return;
      dom.modelProvider.value = provider.id;
      selectSettingsSection('models-block');
      dom.modelName.focus();
    });
    edit.disabled = addModel.disabled = Boolean(state.configSaving || state.thinkingSaving || !state.connected);
    actions.append(edit, addModel);
    item.append(actions);
    dom.providerList.append(item);
  }
}

function renderModelList() {
  dom.modelList.replaceChildren();
  const models = Array.isArray(state.config?.models) ? state.config.models : [];
  const providers = Array.isArray(state.config?.providers) ? state.config.providers : [];
  if (!models.length) {
    dom.modelList.append(node('p', 'config-empty', 'No selectable models configured.'));
  }
  for (const model of models) {
    const item = node('div', 'config-item');
    item.dataset.selected = String(dom.modelForm.dataset.original === model.id && !dom.modelForm.hidden);
    const text = node('div', 'config-item-text');
    text.append(node('span', 'config-item-title', model.name || model.id));
    const provider = providers.find((entry) => entry.id === model.provider);
    text.append(node('span', 'config-item-meta', `${model.id} · ${provider?.display_name || model.provider}${provider?.enabled === false ? ' · Provider disabled' : ''}`));
    item.append(text);
    const actions = node('div', 'config-item-actions');
    const edit = node('button', '', 'Edit');
    edit.type = 'button';
    edit.setAttribute('aria-label', `Edit model ${model.name || model.id}`);
    edit.setAttribute('aria-pressed', item.dataset.selected);
    edit.disabled = Boolean(state.configSaving || state.thinkingSaving || !state.connected);
    edit.addEventListener('click', () => editModel(model.id));
    actions.append(edit);
    item.append(actions);
    dom.modelList.append(item);
  }
  const previous = dom.modelProvider.value;
  dom.modelProvider.replaceChildren();
  for (const provider of providers) {
    const option = node('option', '', `${provider.display_name || provider.id}${provider.enabled === false ? ' (disabled)' : ''}`);
    option.value = provider.id;
    dom.modelProvider.append(option);
  }
  if (providers.some((provider) => provider.id === previous)) {
    dom.modelProvider.value = previous;
  } else if (providers.length) {
    dom.modelProvider.value = providers[0].id;
  }
  dom.modelProviderGuide.hidden = Boolean(providers.length);
  dom.saveModel.disabled = !providers.length || !state.connected || Boolean(state.configSaving);
  dom.newModel.disabled = !providers.length || !state.connected || Boolean(state.configSaving || state.thinkingSaving);
}

function updateProviderCredentialFields() {
  const stored = dom.providerCredentialKind.value === 'stored';
  dom.providerTokenField.hidden = !stored;
  dom.providerCredentialName.placeholder = stored ? 'stored-name' : 'OPENAI_API_KEY';
  dom.providerCredentialLabel.textContent = stored ? 'Local key reference' : 'Environment variable name';
  dom.providerCredentialHint.textContent = stored
    ? 'The API key is saved privately on this server, never returned to the browser. The reference is a local name, not the key itself.'
    : "Set this variable in the server's environment, not in your browser.";
  if (stored && !dom.providerCredentialName.value) {
    dom.providerCredentialName.value = dom.providerId.value.trim();
  }
  if (!stored) dom.providerApiToken.value = '';
}

function canLeaveEditor(form) {
  if (state.configSaving || state.thinkingSaving) return false;
  return form.dataset.dirty !== 'true' || globalThis.confirm('Discard unsaved changes in this editor?');
}

function updateConfigBusy() {
  const busy = Boolean(state.configSaving || state.thinkingSaving || !state.connected);
  dom.providerFields.disabled = dom.modelFields.disabled = busy;
  dom.newProvider.disabled = dom.cancelProvider.disabled = dom.cancelModel.disabled = busy;
  dom.newModel.disabled = busy || !state.config?.providers?.length;
  for (const list of [dom.providerList, dom.modelList]) {
    for (const button of list.querySelectorAll('button')) button.disabled = busy;
  }
  dom.providerForm.setAttribute('aria-busy', String(state.configSaving === 'provider'));
  dom.modelForm.setAttribute('aria-busy', String(state.configSaving === 'model'));
  dom.saveProvider.textContent = state.configSaving === 'provider' ? 'Saving…'
    : dom.providerForm.dataset.original ? 'Save changes' : 'Create provider';
  dom.saveModel.textContent = state.configSaving === 'model' ? 'Saving…'
    : dom.modelForm.dataset.original ? 'Save changes' : 'Create model';
}

function resetProviderForm({ open = false, force = false } = {}) {
  if (!force && !canLeaveEditor(dom.providerForm)) return false;
  dom.providerForm.hidden = !open;
  dom.providerList.hidden = open;
  dom.providerForm.dataset.dirty = 'false';
  dom.providerEditorMode.textContent = 'NEW PROVIDER';
  dom.providerEditorTitle.textContent = 'Add provider';
  dom.saveProvider.textContent = 'Create provider';
  dom.providerAdvanced.open = false;
  dom.providerForm.dataset.original = '';
  dom.providerId.readOnly = false;
  dom.providerId.value = '';
  dom.providerDisplay.value = '';
  dom.providerAdapter.value = 'direct';
  dom.providerProtocol.value = 'chat_completions';
  dom.providerEndpoint.value = '';
  dom.providerEnabled.value = 'true';
  dom.providerCredentialKind.value = 'env';
  dom.providerCredentialName.value = '';
  dom.providerApiToken.value = '';
  dom.deleteProvider.hidden = true;
  dom.providerDeleteHint.hidden = true;
  updateProviderCredentialFields();
  dom.providerDefaultModel.value = '';
  dom.providerContextWindow.value = '';
  dom.providerTaskThinking.value = 'inherit';
  setFormStatus(dom.providerStatus, '');
  renderProviderList();
  if (open) dom.providerId.focus();
  return true;
}

function editProvider(id, { force = false } = {}) {
  const provider = state.config?.providers?.find((entry) => entry.id === id);
  if (!provider) return;
  if (!force && !canLeaveEditor(dom.providerForm)) return;
  dom.providerForm.hidden = false;
  dom.providerList.hidden = true;
  dom.providerForm.dataset.dirty = 'false';
  dom.providerEditorMode.textContent = 'EDIT PROVIDER';
  dom.providerEditorTitle.textContent = provider.display_name || provider.id;
  dom.saveProvider.textContent = 'Save changes';
  dom.providerAdvanced.open = provider.task_reasoning_effort != null;
  dom.providerForm.dataset.original = id;
  dom.providerId.readOnly = true;
  dom.providerId.value = provider.id;
  dom.providerDisplay.value = provider.display_name || '';
  dom.providerAdapter.value = provider.adapter || 'direct';
  dom.providerProtocol.value = provider.protocol || 'chat_completions';
  dom.providerEndpoint.value = provider.endpoint || '';
  dom.providerEnabled.value = provider.enabled === false ? 'false' : 'true';
  const credential = provider.credential && typeof provider.credential === 'object'
    ? provider.credential
    : {};
  dom.providerCredentialKind.value = credential.stored ? 'stored' : 'env';
  dom.providerCredentialName.value = credential.stored || credential.env || '';
  dom.providerApiToken.value = '';
  dom.deleteProvider.hidden = false;
  const usedBy = state.config?.models?.filter((model) => model.provider === id) || [];
  dom.deleteProvider.disabled = Boolean(usedBy.length);
  dom.deleteProvider.title = usedBy.length ? `Used by: ${usedBy.map((model) => model.id).join(', ')}. Reassign these models before deleting this provider.` : 'Delete this provider';
  dom.providerDeleteHint.hidden = !usedBy.length;
  dom.providerDeleteHint.textContent = usedBy.length
    ? `Used by ${usedBy.map((model) => model.id).join(', ')}. Reassign these models in Models before deleting this provider.` : '';
  updateProviderCredentialFields();
  dom.providerDefaultModel.value = provider.default_model || '';
  dom.providerContextWindow.value = provider.context_window_tokens ?? '';
  dom.providerTaskThinking.value = provider.task_reasoning_effort ?? 'inherit';
  setFormStatus(dom.providerStatus, `Editing ${provider.display_name || provider.id}.`);
  renderProviderList();
  dom.providerDisplay.focus();
}

async function deleteProvider(id) {
  if (!id || !state.connected || state.configSaving || state.thinkingSaving) return;
  if (state.config?.models?.some((model) => model.provider === id)) {
    setFormStatus(dom.providerStatus, 'Reassign this provider’s models before deleting it.', 'error');
    return;
  }
  if (!globalThis.confirm(`Delete provider ${id}? Models that still reference it must be removed first.`)) return;
  dom.deleteProvider.disabled = true;
  state.configSaving = 'provider';
  updateControls();
  setFormStatus(dom.providerStatus, `Deleting ${id}…`, 'busy');
  try {
    const summary = await api(`/config/providers/${encodeURIComponent(id)}`, { method: 'DELETE' });
    state.config = summary;
    updateConnectionDetail();
    populateModels();
    renderConfigEditors();
    resetProviderForm({ force: true });
    setFormStatus(dom.providerStatus, `Deleted ${id}.`, 'ok');
    updateSessionDisplay();
    updateControls();
  } catch (error) {
    setFormStatus(dom.providerStatus, error.message, 'error');
    if (error instanceof ApiError && error.status === 401) markUnauthorized();
  } finally {
    state.configSaving = false;
    updateControls();
    dom.deleteProvider.disabled = false;
  }
}

function resetModelForm({ open = false, force = false } = {}) {
  if (!force && !canLeaveEditor(dom.modelForm)) return false;
  dom.modelForm.hidden = !open;
  dom.modelList.hidden = open;
  dom.modelForm.dataset.dirty = 'false';
  dom.modelEditorMode.textContent = 'NEW MODEL';
  dom.modelEditorTitle.textContent = 'Add model';
  dom.saveModel.textContent = 'Create model';
  dom.modelAdvanced.open = false;
  dom.modelForm.dataset.original = '';
  dom.modelId.readOnly = false;
  dom.modelId.value = '';
  dom.modelName.value = '';
  dom.modelContextWindow.value = '';
  dom.modelMaxOutput.value = '';
  dom.modelTaskThinking.value = 'inherit';
  if (dom.modelProvider.options.length) dom.modelProvider.selectedIndex = 0;
  setFormStatus(dom.modelStatus, '');
  renderModelList();
  if (open) dom.modelId.focus();
  return true;
}

function editModel(id, { force = false } = {}) {
  const model = state.config?.models?.find((entry) => entry.id === id);
  if (!model) return;
  if (!force && !canLeaveEditor(dom.modelForm)) return;
  dom.modelForm.hidden = false;
  dom.modelList.hidden = true;
  dom.modelForm.dataset.dirty = 'false';
  dom.modelEditorMode.textContent = 'EDIT MODEL';
  dom.modelEditorTitle.textContent = model.name || model.id;
  dom.saveModel.textContent = 'Save changes';
  dom.modelAdvanced.open = model.context_window_tokens != null || model.max_output_tokens != null || model.task_reasoning_effort != null;
  dom.modelForm.dataset.original = id;
  dom.modelId.readOnly = true;
  dom.modelId.value = model.id;
  if (state.config?.providers?.some((provider) => provider.id === model.provider)) {
    dom.modelProvider.value = model.provider;
  }
  dom.modelName.value = model.name || '';
  dom.modelContextWindow.value = model.context_window_tokens ?? '';
  dom.modelMaxOutput.value = model.max_output_tokens ?? '';
  dom.modelTaskThinking.value = model.task_reasoning_effort ?? 'inherit';
  setFormStatus(dom.modelStatus, `Editing ${model.name || model.id}.`);
  renderModelList();
  dom.modelName.focus();
}

function renderConfigEditors({ hydrateThinking = false } = {}) {
  if (hydrateThinking) dom.taskThinking.value = state.config?.task_reasoning_effort ?? 'inherit';
  renderProjectsList();
  renderSidebarTree();
  renderProviderList();
  renderModelList();
  updateProjectDisplay();
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
  let selected = state.choices.find((choice) => choiceKey(choice) === previous) || sessionChoice;
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

/* Saved connection */

function readSavedCredential() {
  try {
    const value = localStorage.getItem('nexus.server-token') || '';
    if (!value || TOKEN_PATTERN.test(value)) return value;
    localStorage.removeItem('nexus.server-token');
  } catch {
    // A blocked storage backend must not prevent an in-memory connection.
  }
  return '';
}

function saveCredential(value) {
  try {
    if (value) localStorage.setItem('nexus.server-token', value);
    else localStorage.removeItem('nexus.server-token');
    return true;
  } catch {
    return false;
  }
}

/* Saved session references */

const SESSION_INDEX_KEY = 'nexus.sessions';
const CURRENT_SESSION_KEY = 'nexus.current-session';

function readSessionIndex() {
  try {
    const value = JSON.parse(localStorage.getItem(SESSION_INDEX_KEY) || '{}');
    return value && typeof value === 'object' && !Array.isArray(value) ? value : {};
  } catch {
    return {};
  }
}

function writeSessionIndex(index) {
  try {
    localStorage.setItem(SESSION_INDEX_KEY, JSON.stringify(index));
  } catch {
    // The server remains authoritative when browser storage is unavailable.
  }
}

function savedSessionMeta(id) {
  const meta = readSessionIndex()[id];
  return meta && typeof meta === 'object' ? meta : null;
}

function rememberSession(id, changes = {}) {
  if (!id) return;
  const index = readSessionIndex();
  index[id] = { ...(index[id] || {}), ...changes };
  writeSessionIndex(index);
}

function rememberCurrentSession(id) {
  try {
    if (id) localStorage.setItem(CURRENT_SESSION_KEY, id);
    else localStorage.removeItem(CURRENT_SESSION_KEY);
  } catch {
    // Auto-open is optional; the saved session list remains usable.
  }
}

function lastCurrentSession() {
  try {
    return localStorage.getItem(CURRENT_SESSION_KEY) || '';
  } catch {
    return '';
  }
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
    let projects;
    try {
      projects = await api('/projects', { credential: candidate, signal: controller.signal });
    } catch (error) {
      if (error instanceof ApiError && error.status === 401) throw error;
      projects = { enabled: false, mode: 'unavailable', projects: [] };
    }
    if (generation !== state.connectGeneration) return false;

    state.connected = true;
    state.credential = candidate;
    state.config = config;
    dom.serverVersion.textContent = typeof config.version === 'string' ? config.version : 'Not reported';
    state.projects = projects;
    populateModels();
    populateProjects();
    renderSidebarTree();
    renderConfigEditors({ hydrateThinking: true });
    dom.token.value = '';
    setConnectionStatus('Connected', 'ok');
    updateConnectionDetail();
    if (!saveCredential(candidate)) {
      dom.connectionDetail.textContent += ' Browser storage is unavailable; this connection will not be remembered.';
    }

    updateSessionDisplay();
    updateControls();
    await refreshSavedSessions();
    await resumeIfNeeded();
    const last = lastCurrentSession();
    if (!state.session && last && state.savedSessions.some((saved) => saved.session === last)) {
      // Let connect() finish clearing `state.connecting` before restoring.
      setTimeout(() => {
        if (!state.session) void openSavedSession(last);
      }, 0);
    }
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
      if (readSavedCredential() === candidate) saveCredential('');
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
  state.savedPersistence = false;
  state.savedSessions = [];
  const forgotten = saveCredential('');
  setConnectionStatus('Not connected', '');
  setRunStatus('Not connected', '');
  dom.connectionDetail.textContent = forgotten
    ? 'Disconnected. The saved server token has been removed from this browser.'
    : 'Disconnected. Browser storage could not be cleared; clear this site\'s data in your browser to forget the token.';
  renderSidebarTree();
  updateControls();
}

/* Session and controls */

function currentSessionStateLabel() {
  return state.inFlightUnknown
    ? 'Unknown'
    : state.active
      ? state.cancelling ? 'Cancelling' : state.pendingApprovalSeen ? 'Awaiting approval' : 'Running'
      : state.runTerminal ? selectedOutcome(state.lastOutcome) : 'Ready';
}

function syncCurrentSessionTree() {
  for (const item of dom.sidebarTree.querySelectorAll('.tree-session')) {
    if (item.dataset.session !== state.session) continue;
    item.dataset.state = state.active ? 'active' : 'current';
    const [title] = item.querySelectorAll('.tree-session-title');
    if (title) title.textContent = state.sessionTitle || 'New conversation';
    const [meta] = item.querySelectorAll('.tree-session-meta');
    if (meta) meta.textContent = `${describeModelLabel()} · ${currentSessionStateLabel()}`;
  }
}

function updateProjectTreeState() {
  let projectId = state.session ? (state.sessionProjectId || '') : null;
  if (projectId && !state.projectChoices.some((choice) => choice.id === projectId)) projectId = '';
  for (const section of dom.sidebarTree.querySelectorAll('.tree-project')) {
    const [row] = section.querySelectorAll('.tree-project-row');
    if (!row) continue;
    const id = section.dataset.project || '';
    row.dataset.state = state.session
      ? id === projectId ? 'current' : 'idle'
      : id === (state.projectSelection?.id || '') ? 'selected' : 'idle';
  }
}

function updateSessionDisplay() {
  syncCurrentSessionTree();
  updateProjectTreeState();
  dom.conversationTitle.textContent = state.sessionTitle || 'New conversation';
}

function updateComposerActivity() {
  const running = state.connected && !state.inFlightUnknown
    && (state.submitting || (state.active && !state.runTerminal));
  dom.composer.dataset.activity = running ? 'running' : 'idle';
  dom.composerActivity.hidden = !running;
}

async function refreshSavedSessions() {
  if (!state.connected) {
    state.savedPersistence = false;
    state.savedSessions = [];
    renderSidebarTree();
    return;
  }
  try {
    const reply = await api('/sessions');
    state.savedPersistence = reply?.enabled === true;
    state.savedSessions = Array.isArray(reply?.sessions) ? reply.sessions : [];
  } catch (error) {
    state.savedPersistence = false;
    state.savedSessions = [];
    if (error instanceof ApiError && error.status === 401) markUnauthorized();
  }
  renderSidebarTree();
}

async function deleteSavedSession(id) {
  if (!state.connected || state.active || state.submitting || state.connecting || state.historyLoading) return;
  if (!window.confirm('Delete this saved session? This cannot be undone.')) return;
  try {
    await api(`/sessions/${encodeURIComponent(id)}`, { method: 'DELETE' });
    const index = readSessionIndex();
    delete index[id];
    writeSessionIndex(index);
    if (state.session === id) startNewSession({ force: true });
    await refreshSavedSessions();
  } catch (error) {
    if (error instanceof ApiError && error.status === 401) markUnauthorized();
    else addNotice('error', `Could not delete the saved session: ${error.message}`, 'delete-session-failed');
  }
}

async function loadSessionHistory(id) {
  let after = 0;
  let complete = false;
  let pages = 0;
  const items = [];
  while (!complete && pages < 5) {
    const reply = await api(
      `/sessions/${encodeURIComponent(id)}/history?after=${after}&limit=100`,
    );
    const page = Array.isArray(reply?.items) ? reply.items : [];
    items.push(...page);
    const next = Number(reply?.next_after);
    complete = reply?.complete === true || !Number.isInteger(next) || next <= after;
    if (!complete) after = next;
    pages += 1;
  }
  let inbox = null;
  try {
    inbox = await api(`/sessions/${encodeURIComponent(id)}/inbox`);
  } catch (error) {
    if (error?.status !== 404) addNotice('warning', 'Could not recover queued prompts.', 'inbox-refresh');
  }
  if (state.session !== id) return;
  renderPromptInbox(inbox);
  const active = inbox?.recent?.find(item => item.run === inbox.active_run);
  renderHistory(active ? items.filter(item => item.run !== active.run) : items);
  if (active) startDisplayingRun(active.run, active.input);
  if (!complete) {
    addNotice('warning', 'Only the first 500 saved history items were loaded.', 'history-truncated');
  }
}

function renderHistoricalAssistant(text) {
  const article = node('article', 'message assistant');
  if (continuesAssistantReply()) article.dataset.continued = 'true';
  const head = node('div', 'message-head');
  head.append(node('span', '', 'NEXUS'));
  const body = node('div', 'message-body');
  const prose = node('div', 'prose');
  renderMarkdown(prose, text, state.session);
  body.append(prose);
  article.append(head, body);
  appendNode(article);
}

// A helper result delivered at a model boundary or as a follow-up. The text is
// untrusted model output, rendered only through the DOM-building Markdown path.
function renderChildCompletion(item) {
  const status = typeof item.status === 'string' ? item.status : 'unknown';
  const job = typeof item.job_id === 'string' ? item.job_id : '';
  const known = job ? state.delegateJobs.get(job) : null;
  // An adopted follow-up and its own event can both report the same job.
  if (known?.shown) return;
  if (job) {
    state.delegateJobs.set(job, { ...known, state: status, shown: true });
    if (known?.statusElement) {
      known.statusElement.textContent = `Result ${delegateStatusLabel(status).toLowerCase()}`;
    }
  }
  const result = node('details', 'helper-card');
  result.dataset.status = status;
  const summary = node('summary');
  summary.append(node('span', 'helper-card-icon'));
  summary.append(node('span', 'helper-card-title', known?.task
    ? `${delegateRoleLabel(known.role)} · ${known.task}`
    : `Helper result${job ? ` · ${shortJob(job)}` : ''}`));
  summary.append(node('span', 'helper-card-status', delegateStatusLabel(status)));
  result.append(summary);
  const body = node('div', 'helper-card-body');
  if (typeof item.text === 'string' && item.text) {
    const prose = node('div', 'prose');
    renderMarkdown(prose, item.text);
    body.append(prose);
  } else {
    body.append(node('p', 'tool-note', 'The result was delivered to the agent. Its text is available in saved history.'));
  }
  const facts = [];
  const usage = delegateUsageText(item.input_tokens, item.output_tokens);
  if (usage) facts.push(usage);
  if (item.truncated === true) facts.push('Result truncated');
  if (facts.length) body.append(node('div', 'tool-meta', facts.join(' · ')));
  result.append(body);
  appendNode(result);
  renderHelperStatus();
}

function renderHistory(items) {
  flushText();
  for (const job of state.delegateJobs.values()) job.shown = false;
  projectImageObserver?.disconnect();
  dom.conversation.replaceChildren();
  dom.conversation.hidden = true;
  dom.welcome.hidden = false;
  state.pinned = true;
  for (const item of items) {
    if (!item || typeof item.kind !== 'string') continue;
    if (item.kind === 'user' && typeof item.text === 'string') {
      appendUserMessage(item.text);
    } else if (item.kind === 'assistant' && typeof item.text === 'string') {
      renderHistoricalAssistant(item.text);
    } else if (item.kind === 'child-completion' && typeof item.text === 'string') {
      renderChildCompletion(item);
    } else if (item.kind === 'tool-call' && typeof item.call === 'string') {
      startTool({
        call: item.call,
        tool: typeof item.tool === 'string' ? item.tool : 'tool',
        args_preview: `${item.tool} ${typeof item.args === 'string' ? item.args : ''}`,
      });
    } else if (item.kind === 'tool-result' && typeof item.call === 'string') {
      if (!state.tools.has(item.call)) {
        startTool({ call: item.call, tool: typeof item.tool === 'string' ? item.tool : 'tool' });
      }
      finishTool({
        call: item.call,
        outcome: {
          status: item.status,
          content: item.content,
          effect: item.effect,
          evidence: item.evidence,
          truncated: item.truncated === true,
        },
      });
    } else if (item.kind === 'tool-denied' && typeof item.call === 'string') {
      startTool({ call: item.call, tool: typeof item.tool === 'string' ? item.tool : 'tool' });
      finishTool({
        call: item.call,
        outcome: { status: 'Denied', content: 'Denied by policy', effect: 'NotStarted', evidence: 'HostObserved' },
      });
    }
  }
  markUnfinishedTools();
  if (dom.conversation.childElementCount) {
    dom.conversation.hidden = false;
    dom.welcome.hidden = true;
  }
  scrollToBottomSoon();
  updateJumpLatest();
}

async function openSavedSession(id) {
  if (!state.connected || !id || state.active || state.submitting || state.connecting || state.historyLoading) return;
  if (state.session === id) return;
  const meta = savedSessionMeta(id) || {};
  let choice = state.selection;
  if (meta.provider && meta.model) {
    choice = state.choices.find((entry) => !entry.demo
      && entry.provider === meta.provider && entry.model === meta.model) || choice;
  }
  const hasProjectMeta = Object.prototype.hasOwnProperty.call(meta, 'project');
  let project = null;
  if (state.projects?.enabled) {
    if (hasProjectMeta) {
      project = typeof meta.project === 'string' && meta.project
        ? state.projectChoices.find((entry) => entry.id === meta.project) || null
        : null;
    } else {
      project = state.projectSelection;
    }
  }
  const body = {};
  if (choice && !choice.demo) {
    body.provider = choice.provider;
    body.model = choice.model;
  }
  if (state.projects?.enabled && project) body.project = project.id;
  if (state.teamwork === false) body.delegation = false;
  state.historyLoading = true;
  dom.main.scrollTop = 0;
  setRunStatus('Restoring session…', 'busy');
  updateControls();
  try {
    const reply = await api(`/sessions/${encodeURIComponent(id)}/restore`, { method: 'POST', body });
    stopStream();
    resetRunState();
    state.session = reply?.session || id;
    state.sessionProvider = body.provider || null;
    state.sessionModel = body.model || null;
    state.sessionProjectId = project?.id || null;
    state.sessionDelegation = state.teamwork !== false;
    state.sessionDelegationActive = typeof reply?.delegation === 'boolean' ? reply.delegation : null;
    state.context = null;
    state.selection = choice || state.selection;
    populateModels();
    if (project) state.projectSelection = project;
    ensureProjectExpanded(state.sessionProjectId || '');
    const listed = state.savedSessions.find((saved) => saved.session === state.session);
    state.sessionTitle = plainTitle(typeof meta.title === 'string' ? meta.title : listed?.title) || 'Saved session';
    rememberSession(state.session, {
      title: state.sessionTitle,
      provider: state.sessionProvider,
      model: state.sessionModel,
      project: state.sessionProjectId,
    });
    rememberCurrentSession(state.session);
    renderSidebarTree();
    updateSessionDisplay();
    await loadSessionHistory(state.session);
    if (!state.active) setRunStatus('Ready', 'ok');
    await refreshSavedSessions();
  } catch (error) {
    if (error instanceof ApiError && error.status === 401) markUnauthorized();
    else {
      setRunStatus('Restore failed', 'error');
      addNotice('error', `Could not restore the saved session: ${error.message}`, 'restore-session-failed');
    }
  } finally {
    state.historyLoading = false;
    updateControls();
  }
}

function updateControls() {
  updateComposerActivity();
  updateConfigBusy();
  updateProjectFormControls();
  dom.sessionLoading.hidden = !state.historyLoading;
  dom.main.setAttribute('aria-busy', String(state.historyLoading));
  dom.main.dataset.loading = String(state.historyLoading);
  const connected = state.connected;
  const busy = state.active || state.submitting || state.connecting || state.historyLoading;
  const blocked = state.inFlightUnknown;
  const canNewSession = connected && !busy && !blocked;

  dom.newSession.disabled = !canNewSession;
  dom.newSessionMobile.disabled = !canNewSession;
  dom.message.disabled = !connected || blocked || state.historyLoading;
  dom.send.disabled = !connected || state.submitting || state.connecting || state.historyLoading
    || blocked || (!state.session && !state.selection) || !dom.message.value.trim();
  const sendLabel = state.active || state.inbox?.items?.length ? 'Queue' : 'Send';
  dom.send.setAttribute('aria-label', sendLabel);
  dom.send.title = state.active ? 'Queue for after the current run' : sendLabel;
  if (dom.steerRun) {
    dom.steerRun.hidden = !state.active;
    dom.steerRun.disabled = dom.send.disabled || !state.run || state.cancelRequested;
  }
  const projectEnabled = Boolean(state.projects?.enabled);
  dom.model.disabled = !connected || state.connecting || state.historyLoading || state.submitting || blocked;
  // The native select is the data source; the composer shows the model menu trigger.
  dom.model.hidden = true;
  dom.model.title = 'Model for the next task; active runs keep their current model';
  dom.modelTrigger.disabled = !connected || state.connecting || state.historyLoading;
  dom.modelLabel.hidden = true;
  dom.project.disabled = !connected || busy || Boolean(state.session) || !projectEnabled;
  dom.project.hidden = Boolean(state.session) || !projectEnabled;
  dom.projectLabel.hidden = !state.session || !projectEnabled;
  dom.composerProject.hidden = !projectEnabled;
  dom.cancel.hidden = !state.active;
  dom.cancel.disabled = !connected || !state.active || state.cancelling || state.cancelRequested || !state.run;
  dom.cancel.title = state.cancelling || state.cancelRequested ? 'Cancelling…' : 'Cancel run';
  dom.connect.disabled = state.connecting;
  dom.disconnect.disabled = !connected || busy || state.projectPicking || state.projectSaving;
  dom.taskThinking.disabled = !connected || state.thinkingSaving || Boolean(state.configSaving);
  dom.saveThinking.disabled = !connected || state.thinkingSaving || Boolean(state.configSaving);
  dom.message.placeholder = !connected
    ? 'Connect to start a task…'
    : blocked
      ? 'Resolve the previous submission first…'
      : state.active
        ? 'A run is active. Send to queue the next task, or Steer to redirect it…'
        : 'Message or describe a task · /btw asks a side question';
  const savedBusy = state.active || state.submitting || state.connecting || state.historyLoading;
  for (const button of dom.sidebarTree.querySelectorAll('.tree-session-open')) {
    button.disabled = savedBusy;
  }
  for (const button of dom.sidebarTree.querySelectorAll('.tree-session-delete')) {
    button.disabled = savedBusy;
  }
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
  updateProjectDisplay();
  updateSidebarProjectControls();
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
  state.lastUsage = null;
  state.pendingApprovalSeen = false;
  for (const record of state.assistantItems.values()) {
    if (record.renderTimer) clearTimeout(record.renderTimer);
  }
  state.assistantItems.clear();
  state.reasoningItems.clear();
  state.tools.clear();
  state.approvals.clear();
  state.noticeKeys.clear();
  state.notices.clear();
  for (const [id, job] of state.delegateJobs) {
    if (job.state !== 'running') state.delegateJobs.delete(id);
  }
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
  if (!state.connected || state.active || state.submitting || state.connecting || state.historyLoading) return;
  if (state.inFlightUnknown && !force) {
    addNotice(
      'error',
      'The previous submission has an unknown outcome. Starting a new session does not cancel any run that may still be executing.',
      `unknown-session-${Date.now()}`,
    );
    return;
  }
  if (!force && dom.conversation.childElementCount > 0) {
    if (!window.confirm('Start a new session? The current session remains available in saved history.')) return;
  }
  flushText();
  stopStream();
  resetRunState();
  state.session = null;
  renderPromptInbox(null);
  state.sessionProvider = null;
  state.sessionModel = null;
  state.sessionProjectId = null;
  state.sessionDelegation = null;
  state.sessionDelegationActive = null;
  state.context = null;
  state.delegateJobs.clear();
  state.sessionTitle = '';
  rememberCurrentSession('');
  if (state.connected) {
    populateModels();
    populateProjects();
  }
  projectImageObserver?.disconnect();
  dom.conversation.replaceChildren();
  dom.conversation.hidden = true;
  dom.welcome.hidden = false;
  dom.main.scrollTop = 0;
  state.pinned = true;
  updateJumpLatest();
  dom.message.value = '';
  updateSessionDisplay();
  renderSidebarTree();
  updateControls();
  setRunStatus('Ready', 'ok');
  dom.message.focus();
}

function setSessionTitleFromInput(text) {
  if (state.sessionTitle) return;
  const compact = plainTitle(text);
  state.sessionTitle = compact.length > MAX_TITLE_CHARS
    ? `${compact.slice(0, MAX_TITLE_CHARS).trimEnd()}…`
    : compact;
  if (state.session) rememberSession(state.session, { title: state.sessionTitle });
}

async function ensureSession({ restore = false } = {}) {
  if (!state.selection) throw new ApiError(400, 'Select a model before sending.');
  if (state.session) {
    const provider = state.selection.demo ? null : state.selection.provider;
    const model = state.selection.demo ? null : state.selection.model;
    const delegation = state.teamwork !== false;
    if (!restore && provider === state.sessionProvider && model === state.sessionModel
        && delegation === (state.sessionDelegation ?? true)) return;
    if (!state.savedPersistence) {
      if (restore) throw new ApiError(409, 'This session has expired and was not saved. Copy your draft before starting a new session.');
      throw new ApiError(409, 'Switching models or teamwork in an existing session requires --session-store. Start a new session to apply it.');
    }
    const body = {};
    if (provider) Object.assign(body, { provider, model });
    if (state.sessionProjectId) body.project = state.sessionProjectId;
    if (!delegation) body.delegation = false;
    const restored = await api(`/sessions/${encodeURIComponent(state.session)}/restore`, { method: 'POST', body });
    state.sessionProvider = provider;
    state.sessionModel = model;
    state.sessionDelegation = delegation;
    state.sessionDelegationActive = typeof restored?.delegation === 'boolean' ? restored.delegation : null;
    rememberSession(state.session, { provider, model, delegation });
    return;
  }
  const body = {};
  if (!state.selection.demo) {
    body.provider = state.selection.provider;
    body.model = state.selection.model;
  }
  if (state.projects?.enabled && state.projectSelection) {
    body.project = state.projectSelection.id;
  }
  if (state.teamwork === false) body.delegation = false;
  const reply = await api('/sessions', { method: 'POST', body });
  if (!reply || typeof reply.session !== 'string' || !reply.session) {
    throw new ApiError(200, 'Server did not return a session identity.');
  }
  state.session = reply.session;
  state.sessionProvider = state.selection.demo ? null : state.selection.provider;
  state.sessionModel = state.selection.demo ? null : state.selection.model;
  state.sessionProjectId = state.projects?.enabled && state.projectSelection
    ? state.projectSelection.id
    : null;
  state.sessionDelegation = state.teamwork !== false;
  state.sessionDelegationActive = typeof reply.delegation === 'boolean' ? reply.delegation : null;
  ensureProjectExpanded(state.sessionProjectId || '');
  rememberSession(state.session, {
    title: state.sessionTitle || 'New conversation',
    provider: state.sessionProvider,
    model: state.sessionModel,
    project: state.sessionProjectId,
  });
  rememberCurrentSession(state.session);
  renderSidebarTree();
  updateSessionDisplay();
  void refreshSavedSessions();
}

/* Sending */

function renderPromptInbox(inbox) {
  state.inbox = inbox;
  renderHelperStatus();
  const items = inbox?.items || [];
  const expired = inbox?.expired_child_results || 0;
  dom.promptInbox.replaceChildren();
  dom.promptInbox.hidden = !items.length && !expired;
  if (expired) dom.promptInbox.append(node('div', '', `${expired} helper result${expired === 1 ? '' : 's'} expired before delivery.`));
  if (!items.length) return;
  dom.promptInbox.append(node('div', '', inbox.paused || 'Pending prompts'));
  const session = state.session;
  for (const [index, item] of items.entries()) {
    const row = node('div', 'prompt-inbox-row');
    const label = item.kind === 'child-completion' ? `Helper result · ${delegateStatusLabel(item.status)}`
      : item.delivery === 'steer' ? `Steer: ${item.input}` : item.input;
    row.append(node('span', '', label));
    const actions = node('span');
    if (index === 0 && inbox.paused) {
      const resume = node('button', '', 'Resume');
      resume.type = 'button';
      resume.addEventListener('click', () => void changePromptInbox(session, item.id, true));
      actions.append(resume);
    }
    const remove = node('button', '', 'Remove');
    remove.type = 'button';
    remove.addEventListener('click', () => void changePromptInbox(session, item.id, false));
    actions.append(remove);
    row.append(actions);
    dom.promptInbox.append(row);
  }
}

async function changePromptInbox(session, id, resume) {
  if (!session || session !== state.session || state.submitting) return;
  state.submitting = true;
  updateControls();
  try {
    const path = `/sessions/${encodeURIComponent(session)}/inbox/${encodeURIComponent(id)}`;
    await api(resume ? `${path}/start` : path, { method: resume ? 'POST' : 'DELETE' });
    await refreshPromptInbox(session, { adopt: resume });
  } catch (error) {
    addNotice('error', `Could not update queued prompts: ${error.message}`, 'inbox-update');
  } finally {
    state.submitting = false;
    updateControls();
  }
}

async function refreshPromptInbox(session = state.session, { adopt = false, follow = false } = {}) {
  if (!session || !state.connected) return;
  const previous = state.run;
  try {
    const until = Date.now() + 60_000;
    for (let attempt = 0; ; attempt += 1) {
      const inbox = await api(`/sessions/${encodeURIComponent(session)}/inbox`);
      if (state.session !== session || state.run !== previous) return;
      renderPromptInbox(inbox);
      updateControls();
      if (adopt && !state.active) {
        const recent = inbox.recent || [];
        const previousIndex = recent.findIndex(item => item.run === previous);
        const next = previousIndex >= 0 ? recent[previousIndex + 1]
          : previous === null ? recent.find(item => item.run === inbox.active_run)
            : recent.find(item => item.kind === 'child-completion' && item.parent_run === previous);
        if (next) {
          startDisplayingRun(next.run, next.input);
          if (next.kind === 'child-completion') renderChildCompletion(next);
          return;
        }
      }
      const waitingForChild = inbox.pending_child_results > 0;
      if (!follow || inbox.paused || Date.now() >= until
          || (!waitingForChild && (!inbox.items?.length || attempt >= 11))
          || document.hidden) return;
      await new Promise(resolve => setTimeout(resolve, waitingForChild ? 2000 : 150));
      if (state.session !== session || state.run !== previous || document.hidden) return;
    }
  } catch (error) {
    if (state.session === session && error?.status !== 404) {
      addNotice('warning', 'Could not refresh queued prompts.', 'inbox-refresh');
    }
  }
}

function startDisplayingRun(run, text) {
  stopStream();
  resetRunState();
  if (typeof text === 'string') {
    setSessionTitleFromInput(text);
    appendUserMessage(text);
  }
  state.run = run;
  state.active = true;
  updateSessionDisplay();
  setRunStatus('Starting…', 'busy');
  updateControls();
  void attachStream();
}

async function askSideQuestion(draft, question) {
  if (!question) {
    addNotice('warning', 'Enter a question after /btw.', 'side-question-input');
    return;
  }
  if (!state.session || state.askingAside) return;
  const session = state.session;
  state.askingAside = true;
  dom.sideQuestionAnswer.textContent = 'Asking the model…';
  dom.sideQuestionUsage.textContent = '';
  if (!dom.sideQuestion.open) dom.sideQuestion.showModal();
  try {
    const reply = await api(`/sessions/${encodeURIComponent(session)}/side-question`, {
      method: 'POST', body: { question },
    });
    if (state.session !== session) return;
    renderMarkdown(dom.sideQuestionAnswer, reply.answer || '');
    const usage = [reply.usage?.input, reply.usage?.output];
    dom.sideQuestionUsage.textContent = usage.every(value => !Number.isFinite(value))
      ? 'Usage not reported'
      : `Usage: ${usage.map(value => Number.isFinite(value) ? formatTokenCount(value) : '—').join(' / ')} (in / out)`;
    if (dom.message.value === draft) dom.message.value = '';
  } catch (error) {
    if (state.session === session) dom.sideQuestionAnswer.textContent = error.message;
  } finally {
    state.askingAside = false;
    updateControls();
  }
}

async function sendMessage({ steer = false } = {}) {
  const draft = dom.message.value;
  const text = draft.trim();
  if (!text || !state.connected || state.submitting || state.connecting || state.historyLoading || state.inFlightUnknown) return;
  if (steer && (!state.active || !state.run || state.cancelRequested)) return;
  if (/^\/btw(?:\s|$)/.test(text)) {
    await askSideQuestion(draft, text.slice(4).trim());
    return;
  }
  state.submitting = true;
  state.pinned = true;
  let submittingRun = false;
  if (!state.active) setRunStatus(state.session ? 'Starting run…' : 'Creating session…', 'busy');
  updateControls();

  try {
    await ensureSession();
    const path = `/sessions/${encodeURIComponent(state.session)}/runs`;
    const options = {
      method: 'POST',
      body: { input: text, delivery: steer ? 'steer' : 'queue', submission_id: crypto.randomUUID(),
        ...(steer ? { run: state.run } : {}) },
    };
    if (!state.active) setRunStatus('Starting run…', 'busy');
    submittingRun = true;
    let reply;
    try {
      reply = await api(path, options);
    } catch (error) {
      if (steer || state.active || !(error instanceof ApiError)
          || error.status !== 404 || error.message !== 'unknown session') throw error;
      // Session lookup rejected the request before a run could start.
      submittingRun = false;
      setRunStatus('Restoring session…', 'busy');
      try {
        await ensureSession({ restore: true });
      } catch (restoreError) {
        if (restoreError instanceof ApiError) throw restoreError;
        throw new ApiError(0, 'Could not restore the session. Check the server connection and try again.');
      }
      setRunStatus('Starting run…', 'busy');
      submittingRun = true;
      reply = await api(path, options);
    }
    if (reply?.reply === 'queued' || reply?.reply === 'steering') {
      if (dom.message.value === draft) dom.message.value = '';
      await refreshPromptInbox(state.session, { adopt: !state.active });
      return;
    }
    if (!reply || reply.reply !== 'accepted' || typeof reply.run !== 'string') {
      throw new ApiError(200, reply?.reply || 'Server did not accept the run.');
    }

    if (dom.message.value === draft) dom.message.value = '';
    startDisplayingRun(reply.run, text);
  } catch (error) {
    if (error instanceof ApiError) {
      addNotice('error', `Request failed: ${error.message}`, null);
      if (!state.active) setRunStatus(error.status === 401 ? 'Authentication required' : 'Request failed', 'error');
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

// True when an assistant message already follows the latest user message,
// so a new assistant item continues the same reply visually.
function continuesAssistantReply() {
  for (let element = dom.conversation.lastElementChild; element; element = element.previousElementSibling) {
    if (element.classList.contains('user')) return false;
    if (element.classList.contains('assistant')) return true;
  }
  return false;
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
  if (state.runTerminal) {
    void refreshPromptInbox(session, { adopt: true, follow: true });
    return;
  }
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
    case 'user-steered':
      if (typeof detail.text === 'string') {
        markStreamingDone();
        appendUserMessage(detail.text);
        void refreshPromptInbox();
      }
      break;
    case 'child-completion':
      if (typeof detail.text === 'string') {
        markStreamingDone();
        renderChildCompletion(detail);
      }
      break;
    case 'run-started':
      setRunStatus('Running', 'busy');
      break;
    case 'assistant-text':
      appendAssistantText(detail);
      break;
    case 'assistant-reasoning':
      appendReasoning(detail);
      setRunStatus('Thinking…', 'busy');
      break;
    case 'reasoning-redacted':
      setRunStatus('Reasoning content is hidden', 'busy');
      break;
    case 'model-request-started':
      setRunStatus(detail.purpose === 'summary' ? 'Preparing context…' : 'Waiting for model…', 'busy');
      if (detail.purpose !== 'summary') updateContextEstimate(detail);
      break;
    case 'model-request-finished':
      setRunStatus('Model response received', 'busy');
      markStreamingDone();
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

/* Markdown */

const LIVE_MARKDOWN_LIMIT = 40_000;
const MARKDOWN_RENDER_DELAY_MS = 90;
const HIDDEN_TOOL_NAMES = new Set(['host_read', 'host_list']);

function isWordChar(character) {
  return Boolean(character) && /[A-Za-z0-9_]/.test(character);
}

function safeMarkdownHref(raw) {
  let text = raw.trim();
  if (text.startsWith('<') && text.endsWith('>')) text = text.slice(1, -1);
  try {
    const parsed = new URL(text, location.href);
    if (parsed.protocol === 'http:' || parsed.protocol === 'https:' || parsed.protocol === 'mailto:') {
      return parsed.href;
    }
  } catch {
    // Invalid or unsafe targets stay as plain label text.
  }
  return null;
}

function appendMarkdownLink(parent, label, url, depth, inLink = false) {
  const href = safeMarkdownHref(url);
  if (!href) {
    appendInlineMarkdown(parent, label || url, depth + 1, inLink);
    return;
  }
  const link = node('a');
  link.href = href;
  link.target = '_blank';
  link.rel = 'noopener noreferrer';
  appendInlineMarkdown(link, label || url, depth + 1, true);
  parent.append(link);
}

const MARKDOWN_IMAGE = /!\[((?:\\[\[\]\\]|[^[\]\\\n])*)\]\([ \t]*(?:<([^<>\n]+)>|((?:[^\s()<>]|\([^\s()<>]*\))+))(?:[ \t]+(?:"([^"\n]*)"|'([^'\n]*)'))?[ \t]*\)/y;
const PROJECT_IMAGE_TYPES = new Map([
  ['png', 'image/png'],
  ['jpg', 'image/jpeg'],
  ['jpeg', 'image/jpeg'],
  ['webp', 'image/webp'],
  ['svg', 'image/svg+xml'],
]);
let projectImageObserver;
let projectImageQueue = Promise.resolve();

function projectImageTarget(target) {
  if (/^(?:[a-z][a-z0-9+.-]*:|\/\/)/i.test(target)
    || /^\/assets\/icon-(?:192|512)\.png(?:[?#]|$)/.test(target)) return null;
  let path;
  try {
    path = decodeURIComponent(target);
  } catch {
    path = target;
  }
  const type = PROJECT_IMAGE_TYPES.get(path.split('.').at(-1)?.toLowerCase());
  return type && path.includes('.') ? { path, type } : null;
}

async function loadProjectImage(placeholder, session, target) {
  const credential = state.credential;
  if (!placeholder.isConnected || state.session !== session || !credential) return;
  const { path, type } = projectImageTarget(target);
  let url;
  try {
    const response = await fetch(`/sessions/${encodeURIComponent(session)}/project-image`, {
      method: 'POST',
      headers: { Authorization: `Bearer ${credential}`, 'Content-Type': 'application/json' },
      body: JSON.stringify({ path }),
      signal: AbortSignal.timeout(10000),
      cache: 'no-store',
      credentials: 'omit',
      redirect: 'error',
    });
    if (!response.ok || response.headers.get('content-type') !== 'application/octet-stream') {
      throw new Error('Image unavailable');
    }
    const bytes = await response.blob();
    if (!placeholder.isConnected || state.session !== session || state.credential !== credential) return;
    url = URL.createObjectURL(bytes.slice(0, bytes.size, type));
    const image = document.createElement('img');
    image.src = url;
    await image.decode();
    if (!placeholder.isConnected || state.session !== session || state.credential !== credential) return;
    // Keep the blob URL private to the image decoder, especially for untrusted SVG.
    const canvas = node('canvas', 'md-image');
    const scale = Math.min(1, 1600 / Math.max(image.naturalWidth, image.naturalHeight));
    canvas.width = Math.max(1, Math.round(image.naturalWidth * scale));
    canvas.height = Math.max(1, Math.round(image.naturalHeight * scale));
    canvas.getContext('2d').drawImage(image, 0, 0, canvas.width, canvas.height);
    canvas.tabIndex = 0;
    canvas.setAttribute('role', 'button');
    canvas.setAttribute('aria-label', `Enlarge ${placeholder.textContent || 'project image'}`);
    placeholder.replaceWith(canvas);
  } catch {
    placeholder.append(document.createTextNode(' (project image unavailable)'));
  } finally {
    if (url) URL.revokeObjectURL(url);
  }
}

function observeProjectImages(container, session) {
  const previews = Array.from(container.querySelectorAll('[data-project-image]')).slice(0, 4);
  if (!previews.length) return;
  projectImageObserver ??= new IntersectionObserver((entries, observer) => {
    for (const { target, isIntersecting } of entries) {
      if (!isIntersecting) continue;
      observer.unobserve(target);
      const { session, projectImage } = target.dataset;
      projectImageQueue = projectImageQueue.then(() => loadProjectImage(target, session, projectImage));
    }
  });
  for (const preview of previews) {
    preview.dataset.session = session;
    projectImageObserver.observe(preview);
  }
}

function appendMarkdownImage(parent, label, target, title, inLink) {
  if (projectImageTarget(target)) {
    const placeholder = node('span', 'md-project-image', label || 'Project image');
    placeholder.dataset.projectImage = target;
    parent.append(placeholder);
    return;
  }
  let url;
  try {
    url = new URL(target, location.href);
  } catch {
    // Keep the description when the URL is invalid.
  }
  const sameOrigin = url?.origin === location.origin;
  if (!url || url.username || url.password
    || !(url.protocol === 'https:' || (sameOrigin && url.protocol === 'http:'))) {
    parent.append(document.createTextNode(label || 'Image'));
    return;
  }
  const image = node('img', 'md-image');
  image.alt = label;
  if (title) image.title = title;
  image.tabIndex = 0;
  image.setAttribute('role', 'button');
  image.setAttribute('aria-label', label ? `Enlarge image: ${label}` : 'Enlarge image');
  image.loading = 'lazy';
  image.decoding = 'async';
  image.referrerPolicy = 'no-referrer';
  if (sameOrigin) {
    image.src = url.href;
    parent.append(image);
  } else if (!inLink) {
    const load = node('button', 'md-image-load', label ? `Load image: ${label}` : 'Load image');
    load.type = 'button';
    load.title = url.hostname;
    load.addEventListener('click', () => {
      image.src = url.href;
      load.replaceWith(image);
    }, { once: true });
    parent.append(load);
  } else {
    parent.append(document.createTextNode(label || 'Image'));
  }
}

function imagePreviewSource(source) {
  if (!source) return null;
  if (source.tagName === 'CANVAS') {
    try {
      return {
        src: source.toDataURL('image/png'),
        alt: source.getAttribute('aria-label') || 'Project image',
      };
    } catch {
      return null;
    }
  }
  if (source.tagName === 'IMG') {
    const src = source.currentSrc || source.src;
    return src ? { src, alt: source.alt || source.getAttribute('aria-label') || 'Image' } : null;
  }
  return null;
}

let imageLightboxReturnFocus = null;

function enlargeImage(source) {
  const preview = imagePreviewSource(source);
  if (!preview || !dom.imageLightbox || !dom.imageLightboxImage) return false;
  imageLightboxReturnFocus = source?.focus ? source : null;
  dom.imageLightboxImage.src = preview.src;
  dom.imageLightboxImage.alt = preview.alt;
  dom.imageLightbox.hidden = false;
  document.body.classList.add('image-lightbox-open');
  dom.imageLightbox.focus?.();
  return true;
}

function closeImageLightbox() {
  if (!dom.imageLightbox || dom.imageLightbox.hidden) return;
  dom.imageLightbox.hidden = true;
  dom.imageLightboxImage?.removeAttribute('src');
  document.body.classList.remove('image-lightbox-open');
  const target = imageLightboxReturnFocus;
  imageLightboxReturnFocus = null;
  if (target?.isConnected !== false) target?.focus?.();
}

function imageFromEvent(event) {
  return event.target?.closest?.('img.md-image, canvas.md-image') || null;
}

const MATH_SOURCE_LIMIT = 4096;
const MATH_NAMESPACE = 'http://www.w3.org/1998/Math/MathML';
const MATH_IDENTIFIERS = new Map(Array.from(
  'alpha beta gamma delta epsilon zeta eta theta iota kappa lambda mu nu xi pi rho sigma tau upsilon phi chi psi omega Gamma Delta Theta Lambda Xi Pi Sigma Upsilon Phi Psi Omega'.split(' '),
  (name, index) => [name, 'αβγδεζηθικλμνξπρστυφχψωΓΔΘΛΞΠΣΥΦΨΩ'[index]],
));
for (const [name, symbol] of Object.entries({
  epsilon: 'ϵ', varepsilon: 'ε', vartheta: 'ϑ', varpi: 'ϖ', varrho: 'ϱ', varsigma: 'ς',
  phi: 'ϕ', varphi: 'φ', varkappa: 'ϰ', ell: 'ℓ', hbar: 'ℏ', imath: 'ı', jmath: 'ȷ',
  Re: 'ℜ', Im: 'ℑ', aleph: 'ℵ', wp: '℘', emptyset: '∅', varnothing: '∅',
})) MATH_IDENTIFIERS.set(name, symbol);
const MATH_OPERATORS = new Map(Object.entries({
  times: '×', cdot: '⋅', div: '÷', pm: '±', mp: '∓', le: '≤', leq: '≤',
  ge: '≥', geq: '≥', ne: '≠', neq: '≠', approx: '≈', equiv: '≡', sim: '∼',
  simeq: '≃', cong: '≅', asymp: '≍', propto: '∝', ll: '≪', gg: '≫',
  prec: '≺', succ: '≻', preceq: '≼', succeq: '≽',
  in: '∈', ni: '∋', notin: '∉', subset: '⊂', subseteq: '⊆', supset: '⊃',
  supseteq: '⊇', subsetneq: '⊊', cup: '∪', cap: '∩', setminus: '∖',
  wedge: '∧', land: '∧', vee: '∨', lor: '∨', neg: '¬', lnot: '¬',
  forall: '∀', exists: '∃', nexists: '∄', top: '⊤', bot: '⊥', perp: '⊥',
  parallel: '∥', mid: '∣', nmid: '∤', colon: ':',
  to: '→', gets: '←', rightarrow: '→', leftarrow: '←', leftrightarrow: '↔',
  Rightarrow: '⇒', Leftarrow: '⇐', Leftrightarrow: '⇔', implies: '⟹', iff: '⟺',
  longrightarrow: '⟶', longleftarrow: '⟵', longleftrightarrow: '⟷',
  Longrightarrow: '⟹', Longleftarrow: '⟸', Longleftrightarrow: '⟺',
  mapsto: '↦', longmapsto: '⟼', uparrow: '↑', downarrow: '↓', updownarrow: '↕',
  hookrightarrow: '↪', hookleftarrow: '↩', rightleftharpoons: '⇌',
  sum: '∑', prod: '∏', coprod: '∐', bigcup: '⋃', bigcap: '⋂',
  bigvee: '⋁', bigwedge: '⋀', bigoplus: '⨁', bigotimes: '⨂', bigodot: '⨀',
  int: '∫', iint: '∬', iiint: '∭', oint: '∮', oiint: '∯',
  infty: '∞', partial: '∂', nabla: '∇', prime: '′', degree: '°',
  dots: '…', ldots: '…', cdots: '⋯', vdots: '⋮', ddots: '⋱',
  ast: '∗', star: '⋆', circ: '∘', bullet: '•', oplus: '⊕', ominus: '⊖',
  otimes: '⊗', oslash: '⊘', odot: '⊙', triangle: '△', angle: '∠',
  triangleq: '≜', square: '□', Box: '□', diamond: '⋄', therefore: '∴',
  because: '∵', opencorner: '⌜', checkmark: '✓',
}));
const MATH_FUNCTIONS = new Set(
  'sin cos tan cot sec csc arcsin arccos arctan sinh cosh tanh coth log ln lg exp lim liminf limsup min max sup inf det gcd arg deg dim ker Pr hom'.split(' '),
);
const MATH_LIMITS = new Set('sum prod coprod bigcup bigcap bigvee bigwedge bigoplus bigotimes bigodot lim liminf limsup min max sup inf'.split(' '));
const MATH_SPACES = new Map([
  ['\\,', '0.167em'], ['\\:', '0.222em'], ['\\;', '0.278em'],
  ['\\ ', '0.333em'], ['\\enspace', '0.5em'], ['\\quad', '1em'], ['\\qquad', '2em'],
  ['\\thinspace', '0.167em'], ['\\medspace', '0.222em'], ['\\thickspace', '0.278em'],
]);
const MATH_FIXED_DELIMITERS = new Set('()[]{}|');
const MATH_DELIMITERS = new Map(Object.entries({
  lbrace: '{', rbrace: '}', lbrack: '[', rbrack: ']', langle: '⟨', rangle: '⟩',
  lvert: '|', rvert: '|', vert: '|', lVert: '‖', rVert: '‖', Vert: '‖',
  lfloor: '⌊', rfloor: '⌋', lceil: '⌈', rceil: '⌉', backslash: '∖',
  uparrow: '↑', downarrow: '↓', updownarrow: '↕',
}));
for (const value of MATH_DELIMITERS.values()) MATH_FIXED_DELIMITERS.add(value);
MATH_FIXED_DELIMITERS.add('∥');
const MATH_ACCENTS = new Map(Object.entries({
  hat: '^', widehat: '^', tilde: '~', widetilde: '~', bar: '¯', overline: '¯',
  vec: '→', overrightarrow: '→', overleftarrow: '←', overleftrightarrow: '↔',
  dot: '˙', ddot: '¨', dddot: '⃛', breve: '˘', check: 'ˇ', acute: '´', grave: '`',
  underleftarrow: '←', underrightarrow: '→', underline: '_', overbrace: '⏞', underbrace: '⏟',
}));
const MATH_ENVIRONMENTS = new Map(Object.entries({
  matrix: '', pmatrix: '()', bmatrix: '[]', Bmatrix: '{}', vmatrix: '||',
  Vmatrix: '‖‖', smallmatrix: '', cases: '{', dcases: '{',
  aligned: '', align: '', 'align*': '', split: '', alignedat: '', alignat: '', 'alignat*': '',
  gathered: '', gather: '', 'gather*': '', equation: '', 'equation*': '', array: '',
}));
const MATH_ALIGNMENTS = new Set(['aligned', 'align', 'align*', 'split', 'alignedat', 'alignat', 'alignat*']);
const MATH_ALPHABETS = new Map([
  ['mathbf', [0x1d400, 0x1d41a, 0x1d7ce, 0x1d6a8, 0x1d6c2]],
  ['boldsymbol', [0x1d468, 0x1d482, 0x1d7ce, 0x1d71c, 0x1d736]],
  ['mathit', [0x1d434, 0x1d44e]], ['mathbb', [0x1d538, 0x1d552, 0x1d7d8]],
  ['mathcal', [0x1d49c, 0x1d4b6]], ['mathscr', [0x1d49c, 0x1d4b6]],
  ['mathfrak', [0x1d504, 0x1d51e]], ['mathsf', [0x1d5a0, 0x1d5ba, 0x1d7e2]],
  ['mathtt', [0x1d670, 0x1d68a, 0x1d7f6]], ['mathrm', []],
]);
const MATH_LETTER_EXCEPTIONS = new Map([
  [0x1d455, 'ℎ'], [0x1d49d, 'ℬ'], [0x1d4a0, 'ℰ'], [0x1d4a1, 'ℱ'],
  [0x1d4a3, 'ℋ'], [0x1d4a4, 'ℐ'], [0x1d4a7, 'ℒ'], [0x1d4a8, 'ℳ'],
  [0x1d4ad, 'ℛ'], [0x1d4ba, 'ℯ'], [0x1d4bc, 'ℊ'], [0x1d4c4, 'ℴ'],
  [0x1d506, 'ℭ'], [0x1d50b, 'ℌ'], [0x1d50c, 'ℑ'], [0x1d515, 'ℜ'],
  [0x1d51d, 'ℨ'], [0x1d53a, 'ℂ'], [0x1d53f, 'ℍ'], [0x1d545, 'ℕ'],
  [0x1d547, 'ℙ'], [0x1d548, 'ℚ'], [0x1d549, 'ℝ'], [0x1d551, 'ℤ'],
]);

function mathElement(name, ...children) {
  const element = document.createElementNS(MATH_NAMESPACE, name);
  element.append(...children);
  return element;
}

function mathOperator(value, stretchy = false) {
  const operator = mathElement('mo', value);
  if (stretchy || MATH_FIXED_DELIMITERS.has(value)) {
    operator.setAttribute('stretchy', String(stretchy));
  }
  return operator;
}

function buildMarkdownMath(source, display) {
  if (!source.trim() || source.length > MATH_SOURCE_LIMIT) return null;
  const token = /(\\(?:[A-Za-z]+|.)|\d+(?:\.\d+)?|.)/syu;
  const textGroup = /\{((?:\\[{}\\%$#_&]|[^{}\\])*)\}/y;
  const input = source.trim();
  let index = 0;
  let alphabet;
  const operatorLimits = new WeakSet();
  const invalid = new Error('Unsupported math');

  function peek() {
    while (index < input.length && /\s/.test(input[index])) index += 1;
    return input[index];
  }

  function nextToken() {
    peek();
    token.lastIndex = index;
    return token.exec(input)?.[0];
  }

  function take() {
    const value = nextToken();
    if (!value) throw invalid;
    index += value.length;
    return value;
  }

  function rawGroup(pattern) {
    if (take() !== '{') throw invalid;
    const end = input.indexOf('}', index);
    if (end < 0) throw invalid;
    const value = input.slice(index, end);
    if (!pattern.test(value)) throw invalid;
    index = end + 1;
    return value;
  }

  function styled(child, style, level) {
    const wrapper = mathElement('mstyle', child);
    if (style !== undefined) wrapper.setAttribute('displaystyle', String(style));
    if (level !== undefined) wrapper.setAttribute('scriptlevel', level);
    return wrapper;
  }

  function letter(value) {
    let text = value;
    if (alphabet) {
      const ascii = value.charCodeAt(0);
      const group = /^[A-Z]$/.test(value) ? [0, 65] : /^[a-z]$/.test(value) ? [1, 97]
        : /^[Α-Ω]$/.test(value) ? [3, 0x391] : /^[α-ω]$/.test(value) ? [4, 0x3b1] : null;
      if (group && alphabet[group[0]]) {
        const transformed = alphabet[group[0]] + ascii - group[1];
        text = MATH_LETTER_EXCEPTIONS.get(transformed) || String.fromCodePoint(transformed);
      }
    }
    const element = mathElement('mi', text);
    if ((alphabet && text === value) || text !== value) element.setAttribute('mathvariant', 'normal');
    return element;
  }

  function delimiter() {
    const value = take();
    if (value === '.') return '';
    if (value === '\\|') return '‖';
    if (/^\\[{}]$/.test(value)) return value[1];
    if (/^[()[\]|⟨⟩‖⌊⌋⌈⌉]$/.test(value)) return value;
    const mapped = value.startsWith('\\') && MATH_DELIMITERS.get(value.slice(1));
    if (!mapped) throw invalid;
    return mapped;
  }

  function fenced(content, left, right) {
    if (!left && !right) return content;
    const row = mathElement('mrow');
    if (left) row.append(mathOperator(left, true));
    row.append(content);
    if (right) row.append(mathOperator(right, true));
    return row;
  }

  function table(depth, displayStyle) {
    const name = rawGroup(/^[a-zA-Z]+\*?$/);
    if (!MATH_ENVIRONMENTS.has(name)) throw invalid;
    const columns = name === 'array' ? rawGroup(/^[lcr|]{1,24}$/).replace(/\|/g, '') : '';
    if (name === 'array' && !columns) throw invalid;
    if (name.startsWith('alignat') || name === 'alignedat') rawGroup(/^[1-9]\d?$/);
    const layout = mathElement('mtable');
    const isCases = name === 'cases' || name === 'dcases';
    const aligned = MATH_ALIGNMENTS.has(name);
    const singleColumn = /^(?:gather|equation)/.test(name);
    const maxColumns = singleColumn ? 1 : isCases ? 2 : columns.length || 24;
    for (let rowIndex = 0; rowIndex < 64; rowIndex += 1) {
      const row = mathElement('mtr');
      for (let column = 0; column < maxColumns; column += 1) {
        const wrap = sequence(depth + 1, value => value === '&' || value === '\\\\' || value === '\\end',
          name === 'dcases' || aligned || (singleColumn && displayStyle));
        const cell = mathElement('mtd', wrap);
        const alignment = columns[column] || (isCases ? 'l' : aligned ? (column % 2 ? 'l' : 'r') : 'c');
        cell.setAttribute('class', `md-math-${alignment}`);
        row.append(cell);
        if (nextToken() !== '&') break;
        take();
        if (column + 1 === maxColumns) throw invalid;
      }
      layout.append(row);
      if (nextToken() === '\\\\') {
        take();
        const gap = input.slice(index).match(/^\s*\[\s*-?\d+(?:\.\d+)?(?:em|ex|pt|px|mm|cm)\s*\]/);
        if (gap) index += gap[0].length;
        if (nextToken() !== '\\end') continue;
      }
      if (take() !== '\\end' || rawGroup(/^[a-zA-Z]+\*?$/) !== name) throw invalid;
      const [left, right] = MATH_ENVIRONMENTS.get(name);
      return fenced(name === 'smallmatrix' ? styled(layout, false, '+1')
        : (name === 'dcases' || aligned) ? styled(layout, true) : layout, left, right);
    }
    throw invalid;
  }

  function atom(depth, single = false, displayStyle = false) {
    if (depth > 24) throw invalid;
    const value = single && /^\d$/.test(peek() || '') ? input[index++] : take();
    if (value === '{') {
      const row = sequence(depth + 1, undefined, displayStyle);
      if (take() !== '}') throw invalid;
      return row;
    }
    if (value === '\\left') {
      const left = delimiter();
      const content = mathElement('mrow');
      const end = command => command === '\\middle' || command === '\\right';
      if (left) content.append(mathOperator(left, true));
      content.append(sequence(depth + 1, end, displayStyle));
      while (nextToken() === '\\middle') {
        take();
        const middle = delimiter();
        if (middle) content.append(mathOperator(middle, true));
        content.append(sequence(depth + 1, end, displayStyle));
      }
      if (take() !== '\\right') throw invalid;
      const right = delimiter();
      // Keep middle delimiters in the same row as the outer fences.
      if (right) content.append(mathOperator(right, true));
      return content;
    }
    if (value === '\\begin') return table(depth, displayStyle);
    if (value === '\\sqrt') {
      if (peek() !== '[') return mathElement('msqrt', atom(depth + 1, true));
      take();
      const degree = sequence(depth + 1, part => part === ']');
      if (take() !== ']') throw invalid;
      return mathElement('mroot', atom(depth + 1, true), degree);
    }
    if (['\\frac', '\\dfrac', '\\tfrac', '\\binom', '\\dbinom', '\\tbinom'].includes(value)) {
      let fraction = mathElement('mfrac', atom(depth + 1, true), atom(depth + 1, true));
      if (value.endsWith('binom')) {
        fraction.setAttribute('linethickness', '0');
        fraction = fenced(fraction, '(', ')');
      }
      return value[1] === 'd' ? styled(fraction, true) : value[1] === 't' ? styled(fraction, false) : fraction;
    }
    if (['\\text', '\\mbox', '\\textnormal', '\\operatorname', '\\tag'].includes(value)) {
      const starred = (value === '\\operatorname' || value === '\\tag') && peek() === '*';
      if (starred) take();
      peek();
      textGroup.lastIndex = index;
      const text = textGroup.exec(input);
      if (!text) throw invalid;
      index = textGroup.lastIndex;
      const label = text[1].replace(/\\([{}\\%$#_&])/g, '$1');
      const element = mathElement(value === '\\operatorname' ? 'mo' : 'mtext',
        value === '\\tag' && !starred ? ` (${label})` : label);
      if (value === '\\operatorname' && starred) operatorLimits.add(element);
      if (value === '\\tag') element.setAttribute('class', 'md-math-tag');
      return element;
    }
    if (MATH_SPACES.has(value)) {
      const space = mathElement('mspace');
      space.setAttribute('width', MATH_SPACES.get(value));
      return space;
    }
    const name = value.slice(1);
    if (value.startsWith('\\')) {
      if (MATH_ALPHABETS.has(name) || name === 'bm') {
        const previous = alphabet;
        alphabet = MATH_ALPHABETS.get(name === 'bm' ? 'boldsymbol' : name);
        const content = atom(depth + 1, true, displayStyle);
        alphabet = previous;
        return content;
      }
      if (MATH_ACCENTS.has(name)) {
        const below = name.startsWith('under');
        const accent = mathElement('mo', MATH_ACCENTS.get(name));
        const wide = /^(?:over|under|wide)/.test(name);
        accent.setAttribute('stretchy', String(wide));
        const accented = mathElement(below ? 'munder' : 'mover', atom(depth + 1, true), accent);
        accented.setAttribute(below ? 'accentunder' : 'accent', 'true');
        if (name.endsWith('brace')) operatorLimits.add(accented);
        return accented;
      }
      if (['overset', 'stackrel', 'underset'].includes(name)) {
        const script = atom(depth + 1, true);
        return mathElement(name === 'underset' ? 'munder' : 'mover', atom(depth + 1, true), script);
      }
      if (name === 'not') {
        const relation = take();
        const symbol = relation.startsWith('\\') ? MATH_OPERATORS.get(relation.slice(1)) : relation;
        if (!symbol || !/^[\p{Sm}=<>]$/u.test(symbol)) throw invalid;
        const negations = { '=': '≠', '∈': '∉', '∋': '∌', '≡': '≢', '⊂': '⊄', '⊆': '⊈', '∥': '∦', '∣': '∤' };
        return mathOperator(negations[symbol] || `${symbol}\u0338`);
      }
      if (name === 'boxed' || name === 'phantom') {
        const child = atom(depth + 1, true, displayStyle);
        const wrapper = mathElement(name === 'boxed' ? 'mrow' : 'mphantom', child);
        if (name === 'boxed') wrapper.setAttribute('class', 'md-math-box');
        return wrapper;
      }
      if (name === 'substack') {
        if (take() !== '{') throw invalid;
        const layout = mathElement('mtable');
        for (let rowIndex = 0; rowIndex < 64; rowIndex += 1) {
          const line = sequence(depth + 1, part => part === '\\\\', false);
          layout.append(mathElement('mtr', mathElement('mtd', line)));
          const separator = take();
          if (separator === '}') return styled(layout, false, '1');
          if (separator !== '\\\\') throw invalid;
        }
        throw invalid;
      }
      if (name === 'xrightarrow' || name === 'xleftarrow') {
        let below;
        if (peek() === '[') {
          take();
          below = sequence(depth + 1, part => part === ']');
          if (take() !== ']') throw invalid;
        }
        const arrow = mathOperator(name === 'xrightarrow' ? '→' : '←', true);
        const above = atom(depth + 1, true);
        return below ? mathElement('munderover', arrow, below, above) : mathElement('mover', arrow, above);
      }
      if (/^(?:big|Big|bigg|Bigg)[lmr]?$/.test(name)) {
        const sizes = { big: '1.2em', Big: '1.5em', bigg: '1.8em', Bigg: '2.2em' };
        const content = delimiter();
        if (!content) return mathElement('mrow');
        const operator = mathOperator(content, true);
        const size = sizes[name.replace(/[lmr]$/, '')];
        operator.setAttribute('minsize', size);
        operator.setAttribute('maxsize', size);
        return operator;
      }
      if (MATH_IDENTIFIERS.has(name)) return letter(MATH_IDENTIFIERS.get(name));
      if (MATH_OPERATORS.has(name)) return mathOperator(MATH_OPERATORS.get(name));
      if (MATH_FUNCTIONS.has(name)) return mathElement('mo', name);
      if (MATH_DELIMITERS.has(name)) return mathOperator(MATH_DELIMITERS.get(name));
      if (name === 'notag' || name === 'nonumber') return mathElement('mrow');
      if (name === '|') return mathOperator('‖');
      if (/^[{}%$#&_!]$/.test(name)) return name === '!' ? mathElement('mrow') : mathOperator(name);
      throw invalid;
    }
    if (/^\d/.test(value)) {
      const digits = alphabet?.[2];
      return mathElement('mn', digits ? value.replace(/\d/g, digit => String.fromCodePoint(digits + Number(digit))) : value);
    }
    if (/^\p{L}$/u.test(value)) return letter(value);
    if (/^[+\-−*=<>()[\]|!.,:;/\p{Sm}]$/u.test(value)) return mathOperator(value === '-' ? '−' : value);
    throw invalid;
  }

  function sequence(depth, stop = () => false, displayStyle = display, allowInfix = true) {
    if (depth > 24) throw invalid;
    const row = mathElement('mrow');
    let command;
    while ((command = nextToken()) && command !== '}' && !stop(command)) {
      if (['\\over', '\\atop', '\\choose'].includes(command)) {
        if (!allowInfix) throw invalid;
        take();
        const fraction = mathElement('mfrac', row, sequence(depth + 1, stop, false, false));
        if (command !== '\\over') fraction.setAttribute('linethickness', '0');
        return command === '\\choose' ? fenced(fraction, '(', ')') : fraction;
      }
      if (['\\displaystyle', '\\textstyle', '\\scriptstyle', '\\scriptscriptstyle'].includes(command)) {
        take();
        const large = command === '\\displaystyle';
        const level = command === '\\scriptstyle' ? '1' : command === '\\scriptscriptstyle' ? '2' : '0';
        row.append(styled(sequence(depth + 1, stop, large, allowInfix), large, level));
        break;
      }
      const base = atom(depth, false, displayStyle);
      let sub;
      let sup;
      let primes = '';
      let limits = operatorLimits.has(base) && /brace$/.test(command)
        || (displayStyle && (operatorLimits.has(base) || MATH_LIMITS.has(command.slice(1))));
      let modifier = nextToken();
      if (['\\limits', '\\nolimits', '\\displaylimits'].includes(modifier)) {
        take();
        limits = modifier === '\\limits' || (modifier === '\\displaylimits' && displayStyle);
      }
      while ((modifier = nextToken()) && ['_', '^', "'", '′', '″', '‴'].includes(modifier)) {
        take();
        if (modifier === '_' || modifier === '^') {
          if ((modifier === '_' && sub) || (modifier === '^' && sup)) throw invalid;
          if (modifier === '_') sub = atom(depth + 1, true);
          else sup = atom(depth + 1, true);
        } else {
          primes += modifier === "'" ? '′' : modifier;
        }
      }
      if (primes) {
        const prime = mathOperator(primes);
        sup = sup ? mathElement('mrow', prime, sup) : prime;
      }
      row.append(sub && sup ? mathElement(limits ? 'munderover' : 'msubsup', base, sub, sup)
        : sub ? mathElement(limits ? 'munder' : 'msub', base, sub)
          : sup ? mathElement(limits ? 'mover' : 'msup', base, sup) : base);
    }
    return row;
  }

  try {
    const row = sequence(0);
    if (peek()) return null;
    const math = mathElement('math', row);
    if (display) math.setAttribute('display', 'block');
    return math;
  } catch (error) {
    if (error !== invalid) throw error;
    return null;
  }
}

function matchInlineMath(text, start) {
  const dollar = text[start] === '$';
  if (dollar ? (text[start - 1] === '$' || !/[^\s$]/.test(text[start + 1] || ''))
    : !text.startsWith('\\(', start)) return null;
  const body = start + (dollar ? 1 : 2);
  for (let index = body; index < text.length && index - body <= MATH_SOURCE_LIMIT; index += 1) {
    if (text[index] === '`') return null;
    if (text[index] === '\\') {
      if (!dollar && text[index + 1] === ')') return { body, end: index, after: index + 2 };
      if (!dollar && text[index + 1] === '(') return null;
      index += 1;
    } else if (dollar && text[index] === '$') {
      if (index > body && !/\s/.test(text[index - 1]) && !/[\d$]/.test(text[index + 1] || '')) {
        return { body, end: index, after: index + 1 };
      }
      return null;
    }
  }
  return null;
}

function appendInlineMarkdown(parent, text, depth = 0, inLink = false) {
  if (!text) return;
  if (depth > 12) {
    parent.append(document.createTextNode(text));
    return;
  }
  let index = 0;
  let buffer = '';
  const flush = () => {
    if (buffer) {
      parent.append(document.createTextNode(buffer));
      buffer = '';
    }
  };

  while (index < text.length) {
    const character = text[index];

    if (character === '$' || character === '\\') {
      const match = matchInlineMath(text, index);
      if (match) {
        flush();
        parent.append(buildMarkdownMath(text.slice(match.body, match.end), false)
          || document.createTextNode(text.slice(index, match.after)));
        index = match.after;
        continue;
      }
      if (text.startsWith('\\(', index) || text.startsWith('\\[', index)) {
        buffer += text.slice(index, index + 2);
        index += 2;
        continue;
      }
    }

    if (character === '\\' && index + 1 < text.length && /[\\`*{}\[\]()#+\-.!_>~|$]/.test(text[index + 1])) {
      buffer += text[index + 1];
      index += 2;
      continue;
    }

    if (character === '!' && text[index + 1] === '[') {
      MARKDOWN_IMAGE.lastIndex = index;
      const image = MARKDOWN_IMAGE.exec(text);
      if (image) {
        flush();
        appendMarkdownImage(parent, image[1].replace(/\\([\[\]\\])/g, '$1'),
          image[2] ?? image[3], image[4] ?? image[5], inLink);
        index += image[0].length;
        continue;
      }
    }

    if (character === '`') {
      let run = 1;
      while (text[index + run] === '`') run += 1;
      const marker = '`'.repeat(run);
      const end = text.indexOf(marker, index + run);
      if (end > index + run) {
        const content = text.slice(index + run, end).replace(/\n/g, ' ');
        flush();
        const code = node('code', '', content);
        parent.append(code);
        index = end + run;
        continue;
      }
    }

    if (character === '[' && !inLink) {
      const close = text.indexOf('](', index + 1);
      if (close > index) {
        const end = text.indexOf(')', close + 2);
        if (end > close) {
          const label = text.slice(index + 1, close);
          const target = text.slice(close + 2, end).trim().split(/\s+/)[0];
          flush();
          appendMarkdownLink(parent, label, target, depth);
          index = end + 1;
          continue;
        }
      }
    }

    if (text.startsWith('**', index) || text.startsWith('__', index)) {
      const delimiter = text.slice(index, index + 2);
      const end = text.indexOf(delimiter, index + 2);
      if (end > index + 2) {
        flush();
        const strong = node('strong');
        appendInlineMarkdown(strong, text.slice(index + 2, end), depth + 1, inLink);
        parent.append(strong);
        index = end + 2;
        continue;
      }
    }

    if (text.startsWith('~~', index)) {
      const end = text.indexOf('~~', index + 2);
      if (end > index + 2) {
        flush();
        const deleted = node('del');
        appendInlineMarkdown(deleted, text.slice(index + 2, end), depth + 1, inLink);
        parent.append(deleted);
        index = end + 2;
        continue;
      }
    }

    if (character === '_') {
      const previous = index > 0 ? text[index - 1] : '';
      if (previous && isWordChar(previous)) {
        buffer += character;
        index += 1;
        continue;
      }
      let end = -1;
      for (let cursor = index + 1; cursor < text.length; cursor += 1) {
        if (text[cursor] === '_'
          && !/\s/.test(text[cursor - 1])
          && (cursor + 1 >= text.length || !isWordChar(text[cursor + 1]))) {
          end = cursor;
          break;
        }
      }
      if (end > index + 1 && !/\s/.test(text[index + 1])) {
        flush();
        const emphasis = node('em');
        appendInlineMarkdown(emphasis, text.slice(index + 1, end), depth + 1, inLink);
        parent.append(emphasis);
        index = end + 1;
        continue;
      }
    }

    if (character === '*' && text[index + 1] !== '*') {
      const end = text.indexOf('*', index + 1);
      if (end > index + 1 && !/\s/.test(text[index + 1]) && !/\s/.test(text[end - 1])) {
        flush();
        const emphasis = node('em');
        appendInlineMarkdown(emphasis, text.slice(index + 1, end), depth + 1, inLink);
        parent.append(emphasis);
        index = end + 1;
        continue;
      }
    }

    if (!inLink
      && (text.startsWith('http://', index) || text.startsWith('https://', index))
      && (index === 0 || !isWordChar(text[index - 1]))) {
      const match = text.slice(index).match(/^https?:\/\/[^\s<>()]+/);
      if (match) {
        let url = match[0];
        const trailing = url.match(/[.,;:!?]+$/);
        if (trailing) url = url.slice(0, -trailing[0].length);
        if (url.length > 8) {
          flush();
          appendMarkdownLink(parent, url, url, depth);
          index += url.length;
          continue;
        }
      }
    }

    buffer += character;
    index += 1;
  }
  flush();
}

function leadingIndent(line) {
  let width = 0;
  for (const character of line) {
    if (character === ' ') width += 1;
    else if (character === '\t') width += 4;
    else break;
  }
  return width;
}

function stripIndent(line, amount) {
  let index = 0;
  let removed = 0;
  while (index < line.length && removed < amount) {
    if (line[index] === ' ') {
      removed += 1;
      index += 1;
    } else if (line[index] === '\t') {
      removed += 4;
      index += 1;
    } else {
      break;
    }
  }
  return line.slice(index);
}

function matchFence(line) {
  const match = line.match(/^\s{0,3}(`{3,}|~{3,})(.*)$/);
  if (!match) return null;
  if (match[1][0] === '`' && match[2].includes('`')) return null;
  return { marker: match[1], info: match[2].trim() };
}

function matchHeading(line) {
  return line.match(/^\s{0,3}(#{1,6})\s+(.*)$/);
}

function isHorizontalRule(line) {
  return /^\s{0,3}(?:(?:\*\s*){3,}|(?:-\s*){3,}|(?:_\s*){3,})$/.test(line);
}

function matchBlockquote(line) {
  return /^\s{0,3}>\s?/.test(line);
}

function matchListItem(line) {
  const match = line.match(/^(\s*)([-+*]|\d+[.)])\s+(.*)$/);
  if (!match) return null;
  return {
    indent: leadingIndent(line),
    ordered: /\d/.test(match[2]),
    start: Number.parseInt(match[2], 10) || 1,
    text: match[3],
  };
}

function splitTableRow(line) {
  let text = line.trim();
  if (text.startsWith('|')) text = text.slice(1);
  if (text.endsWith('|')) text = text.slice(0, -1);
  const cells = [];
  let current = '';
  for (let index = 0; index < text.length; index += 1) {
    const character = text[index];
    const math = (character === '$' || character === '\\') && matchInlineMath(text, index);
    if (math) {
      current += text.slice(index, math.after);
      index = math.after - 1;
    } else if (character === '\\' && index + 1 < text.length) {
      current += text.slice(index, index + 2);
      index += 1;
    } else if (character === '|') {
      cells.push(current);
      current = '';
    } else {
      current += character;
    }
  }
  cells.push(current);
  return cells;
}

function tableSeparator(line) {
  if (!line.includes('|')) return null;
  const cells = splitTableRow(line).map((cell) => cell.trim());
  if (!cells.length) return null;
  const alignments = [];
  for (const cell of cells) {
    const match = cell.match(/^(:?)-{3,}(:?)$/);
    if (!match) return null;
    alignments.push(match[1] && match[2] ? 'center' : match[1] ? 'left' : match[2] ? 'right' : '');
  }
  return alignments;
}

function startsMarkdownBlock(lines, index) {
  const line = lines[index];
  if (!line || !line.trim()) return true;
  if (matchFence(line) || matchHeading(line) || isHorizontalRule(line) || matchBlockquote(line) || matchListItem(line)
    || matchDisplayMath(lines, index)) {
    return true;
  }
  return line.includes('|') && index + 1 < lines.length && tableSeparator(lines[index + 1]) !== null;
}

function matchDisplayMath(lines, start) {
  const first = lines[start].trim();
  const name = first.match(/^\\begin\{([a-zA-Z]+\*?)\}/)?.[1];
  const environment = MATH_ENVIRONMENTS.has(name) ? name : null;
  const open = first.startsWith('$$') ? '$$' : first.startsWith('\\[') ? '\\[' : null;
  if (!open && !environment) return null;
  if (first.length > MATH_SOURCE_LIMIT + 4) return null;
  if (environment && !open) {
    let depth = 0;
    let size = 0;
    for (let index = start; index < lines.length; index += 1) {
      const line = lines[index].trim();
      size += lines[index].length + 1;
      if (size > MATH_SOURCE_LIMIT || !line || matchFence(line)) return null;
      for (const match of line.matchAll(/\\(begin|end)\{([a-zA-Z]+\*?)\}/g)) {
        if (match[2] !== environment) continue;
        depth += match[1] === 'begin' ? 1 : -1;
        if (depth === 0) {
          if (match.index + match[0].length !== line.length) return null;
          return { source: lines.slice(start, index + 1).join('\n'), end: index };
        }
      }
    }
    return null;
  }
  const close = open === '$$' ? '$$' : '\\]';
  if (first.length >= 4 && first.endsWith(close)) {
    return { source: first.slice(2, -2), end: start };
  }
  const source = [first.slice(2)];
  let size = source[0].length;
  for (let index = start + 1; index < lines.length; index += 1) {
    const line = lines[index].trimEnd();
    size += line.length + 1;
    if (size > MATH_SOURCE_LIMIT || !line.trim() || matchFence(line)) return null;
    if (line.endsWith(close)) {
      source.push(line.slice(0, -2));
      return { source: source.join('\n'), end: index };
    }
    source.push(line);
  }
  return null;
}

const CODE_KEYWORDS = new Set((
  'abstract and as assert async await begin break case catch chan class const continue crate def default defer delete do done elif else elseif end enum esac except export extends extern false fi finally fn for foreach from func function global go if impl implements import in include instanceof interface is let local loop map match mod module mut namespace new nil none not null of on or package pass private protected pub public raise range ref require return select self set static struct super switch then this throw throws trait true try type typeof unless unsafe until use using var void when where while with yield'
).split(' '));
const CODE_HIGHLIGHT_CHARS = 20_000;
const CODE_HIGHLIGHT_TOKENS = 4_000;
const CODE_LINE_HASH = /^(?:py|py3|python3?|rb|ruby|sh|bash|zsh|shell|fish|yaml|yml|toml|perl|r|makefile|dockerfile|ini|conf|coffee|elixir|exs?)$/;
const CODE_LINE_DASH = /^(?:sql|lua|hs|haskell|elm|ada)$/;
const CODE_MARKUP = /^(?:html?|xml|svg|vue|svelte|jsx|tsx)$/;
const CODE_PLAIN = /^(?:text|txt|plain|plaintext|none|markdown|md|diff|patch|log)$/;
const CODE_CSS = /^(?:css|scss|less|sass|styl)$/;

function codeLineComment(language) {
  if (CODE_LINE_HASH.test(language)) return '#';
  if (CODE_LINE_DASH.test(language)) return '--';
  if (CODE_MARKUP.test(language) && !/^(?:jsx|tsx)$/.test(language)) return '';
  return '//';
}

function codeTokens(source, language) {
  const lang = String(language || '').toLowerCase().replace(/^language-/, '');
  if (!lang || CODE_PLAIN.test(lang) || source.length > CODE_HIGHLIGHT_CHARS) return null;
  const line = codeLineComment(lang);
  const markup = CODE_MARKUP.test(lang);
  const css = CODE_CSS.test(lang);
  const parts = [
    '/\\*[\\s\\S]*?\\*/',
    '<!--[\\s\\S]*?-->',
    line ? `${line}[^\\n]*` : '',
    '"""[\\s\\S]*?"""',
    "'''[\\s\\S]*?'''",
    '"(?:\\\\[\\s\\S]|[^"\\\\\\n])*"',
    "'(?:\\\\[\\s\\S]|[^'\\\\\\n])*'",
    '`(?:\\\\[\\s\\S]|[^`\\\\])*`',
    '@[A-Za-z_$][\\w$]*',
    '\\$[A-Za-z_][\\w]*',
    css ? '#[0-9a-fA-F]{3,8}\\b' : '',
    '\\b(?:0[xX][\\da-fA-F]+|\\d+(?:\\.\\d+)?(?:[eE][+-]?\\d+)?)\\b',
    markup ? '</?[A-Za-z][\\w:-]*' : '',
    '[A-Za-z_$][\\w$]*',
  ].filter(Boolean);
  const pattern = new RegExp(parts.join('|'), 'g');
  const tokens = [];
  let last = 0;
  let match;
  while ((match = pattern.exec(source))) {
    if (tokens.length >= CODE_HIGHLIGHT_TOKENS) return null;
    if (match.index > last) tokens.push(['', source.slice(last, match.index)]);
    const text = match[0];
    let kind = '';
    if (text.startsWith('/*') || text.startsWith('<!--') || (line && text.startsWith(line))) {
      kind = 'cmt';
    } else if (/^["'`]/.test(text)) {
      kind = 'str';
    } else if (text.startsWith('@') || text.startsWith('$')) {
      kind = 'fn';
    } else if ((css && text.startsWith('#')) || /^(?:0[xX]|\d)/.test(text)) {
      kind = 'num';
    } else if (text.startsWith('<')) {
      kind = 'tag';
    } else if (CODE_KEYWORDS.has(text.toLowerCase())) {
      kind = 'kw';
    } else if (/^\s*\(/.test(source.slice(pattern.lastIndex, pattern.lastIndex + 8))) {
      kind = 'fn';
    }
    tokens.push([kind, text]);
    last = pattern.lastIndex;
  }
  if (last < source.length) tokens.push(['', source.slice(last)]);
  return tokens;
}

function appendHighlightedCode(code, source, language) {
  const tokens = codeTokens(source, language);
  if (!tokens) {
    code.textContent = source;
    return;
  }
  for (const [kind, text] of tokens) {
    if (kind) code.append(node('span', `md-${kind}`, text));
    else code.append(document.createTextNode(text));
  }
}

function appendMarkdownBlocks(parent, lines) {
  let index = 0;
  while (index < lines.length) {
    const line = lines[index];
    if (!line.trim()) {
      index += 1;
      continue;
    }

    const fence = matchFence(line);
    if (fence) {
      const markerCharacter = fence.marker[0];
      let end = index + 1;
      let closed = false;
      while (end < lines.length) {
        const close = lines[end].match(/^\s{0,3}(`{3,}|~{3,})\s*$/);
        if (close && close[1][0] === markerCharacter && close[1].length >= fence.marker.length) {
          closed = true;
          break;
        }
        end += 1;
      }
      const codeBlock = node('div', 'md-code-block');
      const language = fence.info.split(/\s+/)[0];
      if (language) codeBlock.append(node('div', 'md-code-lang', language));
      const pre = node('pre');
      const code = node('code');
      appendHighlightedCode(code, lines.slice(index + 1, end).join('\n'), language);
      pre.append(code);
      codeBlock.append(pre);
      parent.append(codeBlock);
      index = closed ? end + 1 : lines.length;
      continue;
    }

    const displayMath = matchDisplayMath(lines, index);
    if (displayMath) {
      const math = buildMarkdownMath(displayMath.source, true);
      if (math) {
        const wrapper = node('div', 'md-math-display');
        wrapper.append(math);
        parent.append(wrapper);
      } else {
        parent.append(node('p', 'md-math-source', lines.slice(index, displayMath.end + 1).join('\n')));
      }
      index = displayMath.end + 1;
      continue;
    }

    const heading = matchHeading(line);
    if (heading) {
      const level = Math.min(6, heading[1].length);
      const title = heading[2].replace(/\s+#+\s*$/, '');
      const element = node(`h${level}`);
      appendInlineMarkdown(element, title);
      parent.append(element);
      index += 1;
      continue;
    }

    if (isHorizontalRule(line)) {
      parent.append(node('hr'));
      index += 1;
      continue;
    }

    if (matchBlockquote(line)) {
      const inner = [];
      while (index < lines.length && matchBlockquote(lines[index])) {
        inner.push(lines[index].replace(/^\s{0,3}>\s?/, ''));
        index += 1;
      }
      const quote = node('blockquote');
      appendMarkdownBlocks(quote, inner);
      parent.append(quote);
      continue;
    }

    if (line.includes('|') && index + 1 < lines.length) {
      const alignments = tableSeparator(lines[index + 1]);
      const headerCells = splitTableRow(line);
      if (alignments && alignments.length === headerCells.length) {
        const wrap = node('div', 'md-table-wrap');
        const table = node('table');
        const head = node('thead');
        const headRow = node('tr');
        headerCells.forEach((cell, column) => {
          const th = node('th', alignments[column] ? `md-align-${alignments[column]}` : '');
          appendInlineMarkdown(th, cell.trim());
          headRow.append(th);
        });
        head.append(headRow);
        table.append(head);
        index += 2;
        const body = node('tbody');
        while (index < lines.length && lines[index].trim() && lines[index].includes('|')) {
          const cells = splitTableRow(lines[index]);
          const row = node('tr');
          for (let column = 0; column < headerCells.length; column += 1) {
            const td = node('td', alignments[column] ? `md-align-${alignments[column]}` : '');
            appendInlineMarkdown(td, (cells[column] || '').trim());
            row.append(td);
          }
          body.append(row);
          index += 1;
        }
        if (body.childElementCount) table.append(body);
        wrap.append(table);
        parent.append(wrap);
        continue;
      }
    }

    const item = matchListItem(line);
    if (item) {
      const baseIndent = item.indent;
      const ordered = item.ordered;
      const list = node(ordered ? 'ol' : 'ul');
      if (ordered && item.start !== 1) list.start = item.start;
      while (index < lines.length) {
        const current = matchListItem(lines[index]);
        if (!current || current.indent !== baseIndent || current.ordered !== ordered) break;
        const content = [current.text];
        index += 1;
        while (index < lines.length) {
          if (!lines[index].trim()) {
            if (index + 1 < lines.length && leadingIndent(lines[index + 1]) > baseIndent) {
              content.push('');
              index += 1;
              continue;
            }
            break;
          }
          const next = matchListItem(lines[index]);
          if (next && leadingIndent(lines[index]) === baseIndent) break;
          if (leadingIndent(lines[index]) > baseIndent) {
            content.push(stripIndent(lines[index], baseIndent + 2));
            index += 1;
            continue;
          }
          break;
        }
        const listItem = node('li');
        let itemText = content.join('\n');
        const task = itemText.match(/^\[([ xX])\]\s+([\s\S]*)$/);
        if (task) {
          listItem.classList.add('md-task');
          const checkbox = document.createElement('input');
          checkbox.type = 'checkbox';
          checkbox.disabled = true;
          checkbox.checked = task[1].toLowerCase() === 'x';
          listItem.append(checkbox);
          itemText = task[2];
        }
        itemText.split('\n').forEach((part, lineIndex) => {
          if (lineIndex) listItem.append(document.createElement('br'));
          appendInlineMarkdown(listItem, part);
        });
        list.append(listItem);
      }
      parent.append(list);
      continue;
    }

    const paragraph = node('p');
    const start = index;
    index += 1;
    while (index < lines.length && !startsMarkdownBlock(lines, index)) index += 1;
    for (let lineIndex = start; lineIndex < index; lineIndex += 1) {
      if (lineIndex > start) paragraph.append(document.createElement('br'));
      appendInlineMarkdown(paragraph, lines[lineIndex].trim());
    }
    parent.append(paragraph);
  }
}

function renderMarkdown(container, text, projectSession = null) {
  flushText();
  container.replaceChildren();
  appendMarkdownBlocks(container, text.replace(/\r\n?/g, '\n').split('\n'));
  if (projectSession && state.credential) observeProjectImages(container, projectSession);
}

function assistantWarning(record) {
  if (!record.truncated) return;
  record.prose.append(node('span', 'inline-warning', 'Message truncated in this view.'));
}

function renderAssistant(record, final) {
  if (!record) return;
  if (record.renderTimer) {
    clearTimeout(record.renderTimer);
    record.renderTimer = 0;
  }
  const useMarkdown = final || record.text.length <= LIVE_MARKDOWN_LIMIT;
  if (record.renderedLength === record.text.length
    && record.finalized === final
    && record.mdRendered === useMarkdown) {
    return;
  }
  flushText();
  record.textNode = null;
  if (useMarkdown) {
    record.mdRendered = true;
    record.prose.classList.remove('plain-text');
    renderMarkdown(record.prose, record.text, final ? state.session : null);
  } else {
    record.mdRendered = false;
    record.prose.classList.add('plain-text');
    record.textNode = document.createTextNode(record.text);
    record.prose.replaceChildren(record.textNode);
  }
  assistantWarning(record);
  record.renderedLength = record.text.length;
  record.finalized = final;
  if (state.pinned) {
    if (final) scrollToBottomSoon();
    else scrollToBottom();
  }
  updateJumpLatest();
}

function scheduleAssistantRender(record) {
  if (record.renderTimer) return;
  record.renderTimer = setTimeout(() => {
    record.renderTimer = 0;
    renderAssistant(record, false);
  }, MARKDOWN_RENDER_DELAY_MS);
}

/* Assistant text */

function markStreamingDone() {
  for (const record of state.reasoningItems.values()) {
    record.card.dataset.streaming = 'false';
    record.label.textContent = 'Thoughts';
  }
  for (const record of state.assistantItems.values()) {
    record.article.dataset.streaming = 'false';
    renderAssistant(record, true);
  }
}

function appendAssistantText(detail) {
  const text = typeof detail.text === 'string' ? detail.text : '';
  if (!text) return;
  const key = `${detail.turn ?? ''}:${detail.item ?? ''}`;
  let record = state.assistantItems.get(key);
  if (!record) {
    markStreamingDone();
    const article = node('article', 'message assistant');
    if (continuesAssistantReply()) article.dataset.continued = 'true';
    article.dataset.streaming = 'true';
    const head = node('div', 'message-head');
    head.append(node('span', '', 'NEXUS'));
    const body = node('div', 'message-body');
    const prose = node('div', 'prose plain-text');
    const textNode = document.createTextNode('');
    prose.append(textNode);
    body.append(prose);
    article.append(head, body);
    appendNode(article);
    record = {
      article,
      prose,
      textNode,
      text: '',
      chars: 0,
      truncated: false,
      mdRendered: false,
      renderTimer: 0,
      renderedLength: -1,
      finalized: false,
    };
    state.assistantItems.set(key, record);
  }
  record.article.dataset.streaming = 'true';
  record.finalized = false;
  if (record.chars >= MAX_ASSISTANT_CHARS) {
    if (!record.truncated) {
      flushText();
      record.truncated = true;
      assistantWarning(record);
    }
    return;
  }
  const remaining = MAX_ASSISTANT_CHARS - record.chars;
  const piece = text.length > remaining ? clip(text, remaining) : text;
  record.text += piece;
  record.chars += piece.length;
  if (piece.length < text.length) record.truncated = true;

  if (!record.mdRendered) {
    queueText(record.textNode, piece);
    if (record.text.length <= LIVE_MARKDOWN_LIMIT) scheduleAssistantRender(record);
  } else if (record.text.length <= LIVE_MARKDOWN_LIMIT) {
    scheduleAssistantRender(record);
  } else {
    if (record.renderTimer) {
      clearTimeout(record.renderTimer);
      record.renderTimer = 0;
    }
    flushText();
    record.mdRendered = false;
    record.prose.classList.add('plain-text');
    record.textNode = document.createTextNode(record.text);
    record.prose.replaceChildren(record.textNode);
    assistantWarning(record);
    if (state.pinned) scrollToBottom();
  }
}

/* Live reasoning (historical reasoning remains hidden). */

function scrollReasoning(record) {
  record.preview.scrollTop = record.preview.scrollHeight;
  if (record.card.open && record.follow) record.body.scrollTop = record.body.scrollHeight;
}

function appendReasoning(detail) {
  const text = typeof detail.text === 'string' ? detail.text : '';
  if (!text) return;
  const key = `${detail.turn ?? ''}:${detail.item ?? ''}`;
  let record = state.reasoningItems.get(key);
  if (!record) {
    markStreamingDone();
    const card = node('details', 'tool-card reasoning-card');
    card.open = false;
    const summary = node('summary', 'reasoning-summary');
    const label = node('span', 'tool-name reasoning-label', 'Thinking…');
    const preview = node('span', 'reasoning-preview');
    preview.setAttribute('aria-hidden', 'true');
    const previewText = document.createTextNode('');
    preview.append(previewText);
    summary.append(label, preview);
    const body = node('div', 'reasoning-body');
    body.tabIndex = 0;
    body.setAttribute('role', 'region');
    body.setAttribute('aria-label', 'Live thinking');
    const textNode = document.createTextNode('');
    body.append(textNode);
    const warning = node('p', 'reasoning-limit', 'Thinking display limit reached; further text is not shown.');
    warning.hidden = true;
    card.append(summary, body, warning);
    record = { card, label, preview, previewText, body, textNode, warning, chars: 0, follow: true };
    card.addEventListener('toggle', () => {
      record.follow = true;
      requestAnimationFrame(() => {
        scrollReasoning(record);
        if (state.pinned) scrollToBottom();
      });
    });
    body.addEventListener('scroll', () => {
      record.follow = body.scrollHeight - body.scrollTop - body.clientHeight < 48;
    });
    appendNode(card);
    state.reasoningItems.set(key, record);
  }
  record.card.dataset.streaming = 'true';
  record.label.textContent = 'Thinking…';
  const remaining = MAX_ASSISTANT_CHARS - record.chars;
  const piece = text.slice(0, remaining);
  record.chars += piece.length;
  if (piece.length < text.length) record.warning.hidden = false;
  if (piece) {
    queueText(record.previewText, piece);
    queueText(record.textNode, piece);
  }
}

function appendContextNotice(detail) {
  const removed = Number.isFinite(detail.removed_items) ? detail.removed_items : 'some';
  const method = typeof detail.method === 'string' ? detail.method : 'unknown method';
  const retainedItems = Number.isFinite(detail.retained_items) ? detail.retained_items : null;
  const itemLimit = Number.isFinite(detail.item_limit) ? detail.item_limit : null;
  const retainedBytes = Number.isFinite(detail.retained_bytes) ? detail.retained_bytes : null;
  const byteLimit = Number.isFinite(detail.byte_limit) ? detail.byte_limit : null;
  const itemText = retainedItems !== null && itemLimit !== null
    ? ` · ${retainedItems}/${itemLimit} items retained`
    : '';
  const byteText = retainedBytes !== null && byteLimit !== null
    ? ` · ${retainedBytes}/${byteLimit} bytes retained`
    : '';
  const reduction = method === 'bounded-history' && removed === 0
    ? 'archived tool output shortened for the local limit'
    : `${removed} item${removed === 1 ? '' : 's'} removed using ${method}`;
  addNotice(
    'info',
    `Context reduced: ${reduction}${itemText}${byteText}.`,
    `context-${detail.removed_items ?? 'unknown'}`,
  );
}

/* Tools */

function toolPresentation(tool, preview) {
  let args;
  try {
    const text = typeof preview === 'string' ? preview.trim() : '';
    args = JSON.parse(text.startsWith(`${tool} `) ? text.slice(tool.length + 1) : text);
  } catch {
    // Previews may be truncated; keep the original text available below.
  }
  const kind = tool === 'host_exec' ? 'exec' : tool === 'host_write' ? 'write'
    : tool === 'host_patch' ? 'patch' : 'generic';
  const result = {
    label: kind === 'exec' ? 'RUN' : kind === 'write' ? 'WRITE' : kind === 'patch' ? 'EDIT' : tool,
    target: kind === 'exec' ? 'Command preview unavailable' : kind !== 'generic' ? 'Preview unavailable' : '',
    targetKnown: false,
    kind, sections: [],
  };
  if (tool === 'delegate_start' || tool === 'delegate_await') {
    return delegatePresentation(tool, args && typeof args === 'object' && !Array.isArray(args) ? args : {});
  }
  if (!args || typeof args !== 'object' || Array.isArray(args)) return result;
  if (tool === 'host_exec' && Array.isArray(args.argv)
    && args.argv.length && args.argv.every((arg) => typeof arg === 'string')) {
    const command = args.argv.map((arg) => /^[\w./:=@%+,-]+$/.test(arg)
      ? arg : `'${arg.replaceAll("'", "'\\''")}'`).join(' ');
    return {
      label: 'RUN', target: command, kind: 'exec', targetKnown: true,
      sections: [{ label: 'Command', text: command }],
      directory: typeof args.write_dir === 'string' ? args.write_dir : '',
    };
  }
  if ((tool === 'host_write' || tool === 'host_patch') && typeof args.path === 'string') {
    result.target = args.path;
    result.targetKnown = true;
    if (tool === 'host_write' && typeof args.content === 'string') {
      result.sections.push({ label: 'Proposed file content', text: args.content, style: 'content' });
    } else if (tool === 'host_patch') {
      if (typeof args.old_text === 'string' && typeof args.new_text === 'string') {
        result.sections.push(toolPatchDiff(args.old_text, args.new_text));
      }
    }
  }
  return result;
}

const DELEGATE_ROLES = { explore: 'Explore', propose: 'Propose' };

function delegateRoleLabel(role) {
  return DELEGATE_ROLES[role] || 'Helper';
}

function shortJob(id) {
  const text = String(id || '');
  return text.length > 14 ? `${text.slice(0, 12)}…` : text;
}

// Helper tools are shown as the task they delegate, not as raw JSON calls.
function delegatePresentation(tool, args) {
  if (tool === 'delegate_start') {
    const task = typeof args.task === 'string' ? args.task : '';
    const role = typeof args.role === 'string' ? args.role : '';
    const facts = [];
    if (typeof args.model === 'string') facts.push(`Model: ${args.model}`);
    if (typeof args.thinking === 'string') facts.push(`Thinking: ${args.thinking}`);
    facts.push(`Delivery: ${typeof args.delivery === 'string' ? args.delivery : 'default'}`);
    return {
      label: 'HELPER', kind: 'delegate', targetKnown: Boolean(task),
      target: task ? `${delegateRoleLabel(role)} · ${task}` : `${delegateRoleLabel(role)} task`,
      sections: task ? [{ label: 'Task', text: task }] : [],
      facts,
      delegate: { start: true, role, task },
    };
  }
  const job = typeof args.job_id === 'string' ? args.job_id : '';
  const known = job ? state.delegateJobs.get(job) : null;
  return {
    label: 'COLLECT', kind: 'delegate', targetKnown: Boolean(job),
    target: known?.task ? `${delegateRoleLabel(known.role)} · ${known.task}` : job ? `Result of ${shortJob(job)}` : 'Helper result',
    sections: [], facts: [],
    delegate: { start: false, job },
  };
}

function delegateStatusLabel(status) {
  switch (status) {
    case 'accepted': return 'Started';
    case 'completed': return 'Completed';
    case 'collected': return 'Already collected';
    case 'timed-out': return 'Timed out';
    case 'limit-reached': return 'Limit reached';
    case 'cancelled': return 'Cancelled';
    case 'failed': return 'Failed';
    default: return status ? String(status) : 'Unknown';
  }
}

function delegateUsageText(input, output) {
  if (!Number.isFinite(input) && !Number.isFinite(output)) return '';
  const part = (value) => Number.isFinite(value) ? formatCompactTokens(value) : '—';
  return `Helper usage ${part(input)} in · ${part(output)} out`;
}

// Replaces the raw JSON output of a helper tool with its task state, result
// text, or verified proposal. Unparseable output keeps the raw view.
function finishDelegateTool(record, content, toolSucceeded) {
  let value;
  try {
    value = JSON.parse(content);
  } catch {
    return null;
  }
  if (!value || typeof value !== 'object') return null;
  const status = typeof value.status === 'string' ? value.status : '';
  const job = typeof value.job_id === 'string' ? value.job_id : record.delegate.job;
  if (record.delegate.start) {
    if (toolSucceeded && status === 'accepted' && job) {
      state.delegateJobs.set(job, {
        role: record.delegate.role, task: record.delegate.task, state: 'running', run: state.run,
        statusElement: record.status,
      });
    }
  } else if (job) {
    const known = state.delegateJobs.get(job);
    if (known) known.state = status || 'unknown';
    if (typeof value.text === 'string' && value.text) {
      appendToolSection(record.resultSlot, { label: 'Result', text: value.text });
    }
    const proposal = value.proposal && typeof value.proposal === 'object' ? value.proposal : null;
    if (proposal && typeof proposal.old_text === 'string' && typeof proposal.new_text === 'string') {
      const diff = toolPatchDiff(proposal.old_text, proposal.new_text);
      diff.label = `Proposed edit · ${typeof proposal.path === 'string' ? proposal.path : 'file'}`;
      appendToolSection(record.resultSlot, diff);
      record.resultSlot.append(node('p', 'tool-note', 'A proposal is data only; the agent applies it with its normal edit tool if it chooses to.'));
    }
    if (value.truncated === true) record.resultSlot.append(node('span', 'inline-warning', 'The helper result was truncated.'));
  }
  record.outputSection.hidden = true;
  renderHelperStatus();
  return {
    label: delegateStatusLabel(status),
    tone: !toolSucceeded || ['failed', 'cancelled', 'timed-out', 'limit-reached'].includes(status)
      ? status === 'failed' || !toolSucceeded ? 'error' : 'busy'
      : 'ok',
    usage: delegateUsageText(value.input_tokens, value.output_tokens),
    helperUsage: Number.isFinite(value.input_tokens) || Number.isFinite(value.output_tokens)
      ? { input: value.input_tokens, output: value.output_tokens }
      : null,
  };
}

function toolPatchDiff(oldText, newText) {
  const lines = (text) => text ? text.split('\n') : [];
  const before = lines(clip(oldText, MAX_TOOL_CHARS));
  const after = lines(clip(newText, MAX_TOOL_CHARS));
  let start = 0;
  while (start < before.length && start < after.length && before[start] === after[start]) start += 1;
  let end = 0;
  while (end < before.length - start && end < after.length - start
    && before[before.length - end - 1] === after[after.length - end - 1]) end += 1;
  const rows = [];
  // Keep comparison linear and bounded: one replacement region, not a minimal edit script.
  for (let index = Math.max(0, start - 3); index < start; index += 1) {
    rows.push({ style: 'context', text: `  ${before[index]}` });
  }
  for (let index = start; index < before.length - end; index += 1) {
    rows.push({ style: 'removed', text: `- ${before[index]}` });
  }
  for (let index = start; index < after.length - end; index += 1) {
    rows.push({ style: 'added', text: `+ ${after[index]}` });
  }
  for (let index = 0; index < Math.min(end, 3); index += 1) {
    rows.push({ style: 'context', text: `  ${after[after.length - end + index]}` });
  }
  return {
    label: 'Proposed diff · replacement excerpt', rows,
    truncated: oldText.length > MAX_TOOL_CHARS || newText.length > MAX_TOOL_CHARS,
    unchanged: oldText === newText,
  };
}

function appendToolSection(body, section) {
  const root = node('div', 'tool-section');
  root.append(node('span', 'tool-label', section.label));
  if (section.rows) {
    const diff = node('pre', 'tool-diff');
    for (const row of section.rows) diff.append(node('span', `tool-diff-line ${row.style}`, row.text));
    if (section.unchanged) diff.textContent = 'No changes';
    root.append(diff);
    if (section.truncated) root.append(node('span', 'inline-warning', 'Diff preview truncated in this view'));
    body.append(root);
    return;
  }
  const pre = node('pre', section.style ? `tool-code ${section.style}` : 'tool-command');
  const text = clip(section.text, MAX_TOOL_CHARS);
  if (section.style) {
    for (const line of text.split('\n')) pre.append(node('span', 'tool-code-line', line));
  } else {
    pre.textContent = text;
  }
  root.append(pre);
  if (text.length < section.text.length) root.append(node('span', 'inline-warning', 'Preview truncated in this view'));
  body.append(root);
}

function startTool(detail) {
  const call = String(detail.call ?? '');
  if (!call || state.tools.has(call)) return;
  flushText();
  markStreamingDone();
  const toolName = typeof detail.tool === 'string' ? detail.tool : 'tool';
  if (HIDDEN_TOOL_NAMES.has(toolName)) {
    setRunStatus(toolName === 'host_list' ? 'Listing project files…' : 'Reading project file…', 'busy');
    return;
  }
  setRunStatus(`Running tool: ${toolName}`, 'busy');

  const root = node('details', 'tool-card');
  root.dataset.state = 'running';
  const presentation = toolPresentation(toolName, detail.args_preview);
  root.open = false;
  root.dataset.kind = presentation.kind;
  const summary = node('summary');
  summary.append(node('span', 'tool-name', presentation.label));
  let target = null;
  if (presentation.target) {
    target = node('span', 'tool-target', clip(presentation.target, 500));
    target.title = presentation.targetKnown
      ? clip(presentation.target, MAX_TOOL_CHARS)
      : 'The server did not include this call\'s argument preview.';
    if (!presentation.targetKnown) target.dataset.unavailable = 'true';
    summary.append(target);
  }
  const status = node('span', 'tool-status', 'Running');
  status.dataset.state = 'running';
  summary.append(status);
  root.append(summary);

  const body = node('div', 'tool-body');
  for (const section of presentation.sections) appendToolSection(body, section);
  if (presentation.directory) body.append(node('div', 'tool-meta', `Write directory: ${presentation.directory}`));
  if (presentation.facts?.length) body.append(node('div', 'tool-meta', presentation.facts.join(' · ')));
  // Parsed helper results go above the raw argument and output views.
  const resultSlot = node('div', 'tool-result-slot');
  body.append(resultSlot);
  if (!presentation.targetKnown && presentation.kind !== 'generic') {
    body.append(node(
      'p',
      'tool-note',
      'The server did not provide an argument preview for this call. The tool still ran; its result is shown below.',
    ));
  }
  if (typeof detail.args_preview === 'string' && detail.args_preview) {
    const section = node('details', 'tool-arguments');
    section.append(node('summary', '', `Raw arguments · ${toolName}`));
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

  const record = {
    root,
    status,
    target,
    targetKnown: presentation.targetKnown,
    kind: presentation.kind,
    delegate: presentation.delegate || null,
    resultSlot,
    outputSection,
    outputText,
    meta,
    chars: 0,
    truncated: false,
    finished: false,
    userToggled: false,
  };
  summary.addEventListener('click', () => { record.userToggled = true; });
  state.tools.set(call, record);
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
  let tone = status === 'Succeeded' ? 'ok'
    : status === 'Cancelled' || status === 'Interrupted' || status === 'TimedOut' ? 'busy'
      : 'error';
  let label = status === 'Succeeded' ? 'Completed'
    : status === 'TimedOut' ? 'Timed out'
      : status;
  const delegated = record.delegate && content ? finishDelegateTool(record, content, status === 'Succeeded') : null;
  if (delegated) ({ label, tone } = delegated);
  setStatus(record.status, label, tone);
  record.root.dataset.state = tone;
  if (!record.targetKnown && record.target && (record.kind === 'write' || record.kind === 'patch')) {
    const result = typeof outcome.content === 'string' ? outcome.content.trim() : '';
    if (result) {
      record.target.textContent = clip(result, 240);
      record.target.title = clip(result, MAX_TOOL_CHARS);
      delete record.target.dataset.unavailable;
      record.targetKnown = true;
    }
  }
  setRunStatus(`Tool finished: ${label}`, tone === 'error' ? 'error' : 'busy');
  if (!record.userToggled || tone !== 'ok') record.root.open = tone !== 'ok';

  record.meta.replaceChildren();
  if (typeof outcome.effect === 'string') record.meta.append(node('span', '', `Effect: ${outcome.effect}`));
  if (typeof outcome.evidence === 'string') record.meta.append(node('span', '', `Evidence: ${outcome.evidence}`));
  if (outcome.truncated) record.meta.append(node('span', '', 'Server truncated output'));
  if (record.truncated) record.meta.append(node('span', '', 'View truncated output'));
  if (delegated?.usage) record.meta.append(node('span', '', delegated.usage));
  if (delegated?.helperUsage) record.helperUsage = delegated.helperUsage;
}

function markUnfinishedTools() {
  for (const record of state.tools.values()) {
    if (!record.finished) {
      setStatus(record.status, 'No final result received', 'error');
      record.root.dataset.state = 'error';
      record.root.open = true;
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

// Token counts are displayed in thousands with at most two decimal places;
// trailing zeros are dropped so 1_200 renders as "1.2k" rather than "1.20k".
function formatTokenCount(value) {
  return `${Math.round((value / 1000) * 100) / 100}k`;
}

function usageParts() {
  const parts = [];
  if (state.lastUsage?.input !== null && state.lastUsage?.input !== undefined) {
    parts.push(`${formatTokenCount(state.lastUsage.input)} in`);
  }
  if (state.lastUsage?.output !== null && state.lastUsage?.output !== undefined) {
    parts.push(`${formatTokenCount(state.lastUsage.output)} out`);
  }
  if (state.lastUsage?.cacheRead !== null && state.lastUsage?.cacheRead !== undefined) {
    parts.push(`${formatTokenCount(state.lastUsage.cacheRead)} cache read`);
  }
  if (state.lastUsage?.cacheWrite !== null && state.lastUsage?.cacheWrite !== undefined) {
    parts.push(`${formatTokenCount(state.lastUsage.cacheWrite)} cache write`);
  }
  return parts;
}

function updateUsage(detail) {
  state.lastUsage = {
    input: Number.isFinite(detail.input) ? detail.input : null,
    output: Number.isFinite(detail.output) ? detail.output : null,
    cacheRead: Number.isFinite(detail.cache_read) ? detail.cache_read : null,
    cacheWrite: Number.isFinite(detail.cache_write) ? detail.cache_write : null,
    finality: typeof detail.finality === 'string' ? detail.finality : '',
  };
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
  for (const job of state.delegateJobs.values()) {
    if (job.state === 'running' && detail.outcome !== 'completed') job.state = 'cancelled';
  }
  const outcome = selectedOutcome(detail.outcome);
  setRunStatus(outcome, outcomeTone(detail.outcome));
  appendRunSummary(detail, fromSnapshot);
  renderHelperStatus();
  updateSessionDisplay();
  updateControls();
  scrollToBottomSoon();
  updateJumpLatest();
  void refreshSavedSessions();
  void verifyTerminalSnapshot(session, run, lastSeq, fromSnapshot);
  if (fromSnapshot) void refreshPromptInbox(session, { adopt: true, follow: true });
}

function persistenceLabel(value) {
  switch (value) {
    case 'Saved':
      return 'Saved';
    case 'Ephemeral':
      return 'Not saved';
    case 'SaveFailed':
      return 'Save failed';
    default:
      return `Persistence: ${value}`;
  }
}

function appendRunSummary(detail, fromSnapshot) {
  const card = node('div', 'run-card');
  card.dataset.outcome = detail.outcome || 'unknown';
  const actions = node('div', 'run-actions');
  const text = [...state.assistantItems.values()].map((record) => record.text).filter(Boolean).join('\n\n');
  if (text) actions.append(copyButton(text));
  actions.append(usageChip(detail));
  if (detail.outcome !== 'completed') {
    const outcome = node('span', 'run-outcome', selectedOutcome(detail.outcome));
    outcome.dataset.outcome = detail.outcome || 'unknown';
    actions.append(outcome);
  }
  if (detail.persistence === 'SaveFailed') {
    const persistence = node('span', 'run-outcome', persistenceLabel(detail.persistence));
    persistence.dataset.outcome = 'failed';
    actions.append(persistence);
  }
  if (fromSnapshot) actions.append(node('span', 'run-fact', 'Confirmed by snapshot'));
  const time = node('time', 'run-time', new Date().toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' }));
  actions.append(time);
  card.append(actions);
  if (detail.error && typeof detail.error === 'object') {
    const errorBlock = node('div', 'run-detail');
    errorBlock.append(node('div', '', `${detail.error.category || 'Error'} · ${retryLabel(detail.error.retry)}`));
    if (detail.error.message) {
      const message = String(detail.error.message);
      if (detail.error.category === 'ResourceLimit' && message.startsWith('context budget exhausted:')) {
        errorBlock.append(node('div', '', 'Nexus reached its local context limit, not the model token window.'));
      }
      errorBlock.append(node('pre', '', message));
    }
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

/* Interface settings */

// Text size, message width, spacing, and motion are browser preferences.
// Interface size uses CSS zoom on the page body, so popover coordinates and
// viewport-based heights are corrected by the same factor.
const INTERFACE_KEY = 'nexus.interface';
const INTERFACE_DEFAULTS = { uiScale: 100, contentSize: 15, contentWidth: 'default', lineSpacing: 'default', motion: 'system' };
const UI_SCALES = [90, 100, 110, 125];
const CONTENT_WIDTHS = { narrow: '720px', default: '860px', wide: '1080px', full: '100%' };
const LINE_SPACINGS = { compact: '1.55', default: '1.75', relaxed: '1.95' };

function parseInterfaceSettings(raw) {
  let value = {};
  try {
    value = JSON.parse(raw || '{}') || {};
  } catch {
    value = {};
  }
  const pick = (allowed, candidate, fallback) => Object.hasOwn(allowed, candidate) ? candidate : fallback;
  const scale = Number(value.uiScale);
  return {
    uiScale: UI_SCALES.includes(scale) ? scale : INTERFACE_DEFAULTS.uiScale,
    contentSize: clampStep(value.contentSize, 13, 20, 1, INTERFACE_DEFAULTS.contentSize),
    contentWidth: pick(CONTENT_WIDTHS, value.contentWidth, INTERFACE_DEFAULTS.contentWidth),
    lineSpacing: pick(LINE_SPACINGS, value.lineSpacing, INTERFACE_DEFAULTS.lineSpacing),
    motion: value.motion === 'reduce' ? 'reduce' : 'system',
  };
}

function interfaceZoom() {
  return Number(getComputedStyle(dom.root).getPropertyValue('--ui-scale')) || 1;
}

function applyInterfaceSettings(settings) {
  const style = dom.root.style;
  style.setProperty('--ui-scale', String(settings.uiScale / 100));
  style.setProperty('--content-font-size', `${settings.contentSize}px`);
  style.setProperty('--content-width', CONTENT_WIDTHS[settings.contentWidth]);
  style.setProperty('--content-line-height', LINE_SPACINGS[settings.lineSpacing]);
  if (settings.motion === 'reduce') dom.root.dataset.motion = 'reduce';
  else delete dom.root.dataset.motion;
  const values = { 'ui-scale': String(settings.uiScale), 'content-width': settings.contentWidth,
    'line-spacing': settings.lineSpacing, motion: settings.motion };
  for (const input of dom.interfaceSettings.querySelectorAll('input[type="radio"]')) {
    input.checked = values[input.name] === input.value;
  }
  dom.contentSize.value = String(settings.contentSize);
  dom.contentSizeValue.textContent = `${settings.contentSize} px`;
}

function currentInterfaceSettings() {
  const checked = (name) => dom.interfaceSettings.querySelector(`input[name="${name}"]:checked`)?.value;
  return parseInterfaceSettings(JSON.stringify({
    uiScale: checked('ui-scale'), contentSize: dom.contentSize.value,
    contentWidth: checked('content-width'), lineSpacing: checked('line-spacing'), motion: checked('motion'),
  }));
}

function saveInterfaceSettings(settings) {
  applyInterfaceSettings(settings);
  try {
    localStorage.setItem(INTERFACE_KEY, JSON.stringify(settings));
  } catch {
    // The values still apply to this page.
  }
}

try {
  applyInterfaceSettings(parseInterfaceSettings(localStorage.getItem(INTERFACE_KEY)));
} catch {
  applyInterfaceSettings({ ...INTERFACE_DEFAULTS });
}
dom.interfaceSettings.addEventListener('change', () => saveInterfaceSettings(currentInterfaceSettings()));
dom.contentSize.addEventListener('input', () => saveInterfaceSettings(currentInterfaceSettings()));
dom.interfaceReset.addEventListener('click', () => saveInterfaceSettings({ ...INTERFACE_DEFAULTS }));

/* Custom background */

// The image never leaves the browser: it is decoded, resized, and re-encoded
// locally (dropping metadata), then kept as a Blob in IndexedDB. Dim and blur
// are small settings in localStorage.
const BACKGROUND_SETTINGS_KEY = 'nexus.background';
const BACKGROUND_PALETTE_KEY = 'nexus.backgroundPalette';
const BACKGROUND_PALETTE_MODE_KEY = 'nexus.backgroundPaletteMode';
const BACKGROUND_DB = 'nexus-appearance';
const BACKGROUND_STORE = 'images';
const BACKGROUND_HISTORY_LIMIT = 5;
const BACKGROUND_MAX_EDGE = 2560;
const BACKGROUND_MAX_INPUT_BYTES = 40 * 1024 * 1024;
const BACKGROUND_TYPES = new Set(['image/png', 'image/jpeg', 'image/webp', 'image/gif', 'image/avif']);
const BACKGROUND_DEFAULTS = { dim: 55, blur: 0 };
let backgroundUrl = '';
let backgroundBlob = null;
let backgroundPaletteRevision = 0;
let backgroundPaletteMode = 'auto';
let recentBackgrounds = [];
let recentBackgroundUrls = [];
let currentBackgroundId = null;
let backgroundBusy = true;

function clampStep(value, min, max, step, fallback) {
  const number = Number(value);
  if (!Number.isFinite(number)) return fallback;
  return Math.min(max, Math.max(min, Math.round(number / step) * step));
}

function parseBackgroundSettings(raw) {
  let value = {};
  try {
    value = JSON.parse(raw || '{}') || {};
  } catch {
    value = {};
  }
  return {
    dim: clampStep(value.dim, 0, 90, 5, BACKGROUND_DEFAULTS.dim),
    blur: clampStep(value.blur, 0, 24, 2, BACKGROUND_DEFAULTS.blur),
  };
}

// Scales dimensions down so the longest edge fits; never scales up.
function fitWithin(width, height, maxEdge) {
  const longest = Math.max(width, height);
  if (!(longest > maxEdge)) return { width: Math.round(width), height: Math.round(height) };
  const scale = maxEdge / longest;
  return { width: Math.max(1, Math.round(width * scale)), height: Math.max(1, Math.round(height * scale)) };
}

// Color helpers for image palettes. Channels are 0–255; hue is degrees,
// saturation and lightness are 0–1.
function hexToRgb(hex) {
  const value = Number.parseInt(hex.slice(1), 16);
  return [(value >> 16) & 255, (value >> 8) & 255, value & 255];
}

function rgbToHex([r, g, b]) {
  return `#${[r, g, b].map((channel) => Math.round(Math.min(255, Math.max(0, channel))).toString(16).padStart(2, '0')).join('')}`;
}

function rgbToHsl([r, g, b]) {
  const [red, green, blue] = [r / 255, g / 255, b / 255];
  const max = Math.max(red, green, blue);
  const min = Math.min(red, green, blue);
  const lightness = (max + min) / 2;
  if (max === min) return [0, 0, lightness];
  const delta = max - min;
  const saturation = delta / (1 - Math.abs(2 * lightness - 1));
  const hue = max === red ? ((green - blue) / delta + 6) % 6 : max === green ? (blue - red) / delta + 2 : (red - green) / delta + 4;
  return [hue * 60, Math.min(1, saturation), lightness];
}

function hslToHex(hue, saturation, lightness) {
  const chroma = (1 - Math.abs(2 * lightness - 1)) * saturation;
  const x = chroma * (1 - Math.abs(((hue / 60) % 2) - 1));
  const m = lightness - chroma / 2;
  const [r, g, b] = hue < 60 ? [chroma, x, 0] : hue < 120 ? [x, chroma, 0] : hue < 180 ? [0, chroma, x]
    : hue < 240 ? [0, x, chroma] : hue < 300 ? [x, 0, chroma] : [chroma, 0, x];
  return rgbToHex([(r + m) * 255, (g + m) * 255, (b + m) * 255]);
}

function relativeLuminance(hex) {
  const linear = hexToRgb(hex).map((channel) => {
    const value = channel / 255;
    return value <= 0.03928 ? value / 12.92 : ((value + 0.055) / 1.055) ** 2.4;
  });
  return 0.2126 * linear[0] + 0.7152 * linear[1] + 0.0722 * linear[2];
}

function contrastRatio(a, b) {
  const [high, low] = [relativeLuminance(a), relativeLuminance(b)].sort((x, y) => y - x);
  return (high + 0.05) / (low + 0.05);
}

// Moves lightness away from the surfaces until the color reaches `minimum`
// contrast against all of them, keeping hue and saturation.
function readableColor(hue, saturation, lightness, surfaces, minimum, lighter) {
  let value = lightness;
  for (let step = 0; step < 100; step += 1) {
    const color = hslToHex(hue, saturation, value);
    if (surfaces.every((surface) => contrastRatio(color, surface) >= minimum)) return color;
    value = lighter ? Math.min(1, value + 0.01) : Math.max(0, value - 0.01);
  }
  return lighter ? '#ffffff' : '#000000';
}

// Summarizes an image as its largest color region (base), its most vivid
// well-represented color (accent), and its average lightness.
function extractImageColors(pixels) {
  const groups = new Map();
  let visible = 0;
  let lightness = 0;
  for (let i = 0; i + 3 < pixels.length; i += 4) {
    if (pixels[i + 3] < 128) continue;
    const r = pixels[i];
    const g = pixels[i + 1];
    const b = pixels[i + 2];
    const key = ((r >> 5) << 6) | ((g >> 5) << 3) | (b >> 5);
    const group = groups.get(key) ?? { r: 0, g: 0, b: 0, count: 0 };
    group.r += r;
    group.g += g;
    group.b += b;
    group.count += 1;
    groups.set(key, group);
    visible += 1;
    lightness += relativeLuminance(rgbToHex([r, g, b]));
  }
  if (!visible) return null;
  const average = (group) => rgbToHex([group.r / group.count, group.g / group.count, group.b / group.count]);
  const ranked = [...groups.values()].sort((a, b) => b.count - a.count);
  const base = average(ranked[0]);
  let accent = null;
  let best = -1;
  for (const group of ranked) {
    // Ignore specks: an accent must cover at least 2% of the visible image.
    if (group.count / visible < 0.02) continue;
    const [, saturation, light] = rgbToHsl(hexToRgb(average(group)));
    const vividness = saturation * (1 - Math.abs(2 * light - 1)) * Math.sqrt(group.count / visible);
    if (vividness > best) {
      best = vividness;
      accent = average(group);
    }
  }
  return { base, accent: accent || base, lightness: lightness / visible };
}

// Builds a full theme from extracted colors. Every text color is checked
// against the surfaces it appears on so the result meets WCAG AA (4.5:1).
function buildImagePalette(colors, mode = 'auto') {
  const scheme = mode === 'light' || mode === 'dark' ? mode : colors.lightness > 0.45 ? 'light' : 'dark';
  const [baseHue, baseSaturation] = rgbToHsl(hexToRgb(colors.base));
  let [accentHue, accentSaturation] = rgbToHsl(hexToRgb(colors.accent));
  // Near-gray images get a calm accent from the base hue rather than gray.
  if (accentSaturation < 0.18) [accentHue, accentSaturation] = [baseHue, Math.max(baseSaturation, 0.35)];
  const tint = Math.min(baseSaturation, scheme === 'dark' ? 0.3 : 0.35);
  const vivid = Math.min(0.85, Math.max(0.45, accentSaturation));
  const surface = (light, extra = 0) => hslToHex(baseHue, Math.min(1, tint + extra), light);
  const dark = scheme === 'dark';
  const palette = dark ? {
    bg: surface(0.08), panel: surface(0.11), raised: surface(0.15),
    line: surface(0.22), 'line-strong': surface(0.33), 'code-bg': surface(0.06),
    'user-bg': hslToHex(accentHue, Math.min(vivid, 0.3), 0.19),
    'accent-soft': hslToHex(accentHue, Math.min(vivid, 0.35), 0.2),
  } : {
    bg: surface(0.97), panel: surface(0.94), raised: surface(0.99),
    line: surface(0.86), 'line-strong': surface(0.73), 'code-bg': surface(0.93),
    'user-bg': hslToHex(accentHue, Math.min(vivid, 0.5), 0.91),
    'accent-soft': hslToHex(accentHue, Math.min(vivid, 0.5), 0.9),
  };
  const surfaces = [palette.bg, palette.panel, palette.raised, palette['user-bg'], palette['accent-soft']];
  palette.fg = readableColor(baseHue, Math.min(tint, 0.15), dark ? 0.92 : 0.15, surfaces, 7, dark);
  palette.muted = readableColor(baseHue, Math.min(tint, 0.18), dark ? 0.68 : 0.42, [palette.bg, palette.panel, palette.raised], 4.5, dark);
  palette.accent = readableColor(accentHue, vivid, dark ? 0.7 : 0.4, [palette.bg, palette.panel, palette['accent-soft']], 4.5, dark);
  palette['accent-ink'] = readableColor(accentHue, Math.min(vivid, 0.6), dark ? 0.12 : 0.98, [palette.accent], 4.5, !dark);
  palette.danger = readableColor(4, 0.7, dark ? 0.75 : 0.4, [palette.bg, palette.panel], 4.5, dark);
  palette.warning = readableColor(40, 0.75, dark ? 0.72 : 0.3, [palette.bg, palette.panel], 4.5, dark);
  palette['danger-soft'] = hslToHex(4, 0.45, dark ? 0.16 : 0.94);
  palette['warning-soft'] = hslToHex(40, 0.5, dark ? 0.15 : 0.92);
  return { scheme, palette };
}

const IMAGE_PALETTE_VARIABLES = ['bg', 'panel', 'raised', 'fg', 'muted', 'line', 'line-strong', 'accent',
  'accent-ink', 'accent-soft', 'user-bg', 'code-bg', 'danger', 'danger-soft', 'warning', 'warning-soft'];

async function sampleBackgroundPixels(blob) {
  const bitmap = await createImageBitmap(blob);
  try {
    const { width, height } = fitWithin(bitmap.width, bitmap.height, 48);
    const canvas = document.createElement('canvas');
    canvas.width = width;
    canvas.height = height;
    const context = canvas.getContext('2d');
    context.drawImage(bitmap, 0, 0, width, height);
    return context.getImageData(0, 0, width, height).data;
  } finally {
    bitmap.close();
  }
}

function clearImagePalette() {
  delete dom.root.dataset.backgroundPalette;
  dom.root.style.removeProperty('--background-image-color');
  dom.root.style.removeProperty('color-scheme');
  for (const name of IMAGE_PALETTE_VARIABLES) dom.root.style.removeProperty(`--${name}`);
}

// Inline custom properties override the selected theme until cleared, so
// turning the option off restores the theme without reloading.
function applyImagePalette(colors) {
  const { scheme, palette } = buildImagePalette(colors, backgroundPaletteMode);
  dom.root.style.setProperty('--background-image-color', colors.base);
  dom.root.style.setProperty('color-scheme', scheme);
  for (const name of IMAGE_PALETTE_VARIABLES) dom.root.style.setProperty(`--${name}`, palette[name]);
  dom.root.dataset.backgroundPalette = 'image';
  return { scheme, palette };
}

function renderImagePaletteSwatches(result) {
  if (!dom.backgroundPaletteSwatches) return;
  dom.backgroundPaletteSwatches.replaceChildren();
  dom.backgroundPaletteSwatches.hidden = !result;
  if (!result) return;
  for (const [name, label] of [['bg', 'Background'], ['panel', 'Panels'], ['accent', 'Accent'], ['fg', 'Text']]) {
    const swatch = document.createElement('span');
    swatch.className = 'palette-swatch';
    swatch.style.background = result.palette[name];
    swatch.title = `${label} ${result.palette[name]}`;
    dom.backgroundPaletteSwatches.append(swatch);
  }
  const detail = document.createElement('span');
  detail.className = 'palette-swatch-label';
  detail.textContent = `${result.scheme === 'dark' ? 'Dark' : 'Light'} theme from image`;
  dom.backgroundPaletteSwatches.append(detail);
}

async function refreshBackgroundPalette() {
  const revision = ++backgroundPaletteRevision;
  clearImagePalette();
  renderImagePaletteSwatches(null);
  setFormStatus(dom.backgroundPaletteStatus, '');
  if (dom.backgroundPaletteModes) dom.backgroundPaletteModes.hidden = !dom.backgroundPalette.checked;
  if (!backgroundBlob || !dom.backgroundPalette.checked) return;
  try {
    const colors = extractImageColors(await sampleBackgroundPixels(backgroundBlob));
    if (revision !== backgroundPaletteRevision) return;
    if (!colors) throw new Error('No visible colors');
    renderImagePaletteSwatches(applyImagePalette(colors));
  } catch {
    if (revision !== backgroundPaletteRevision) return;
    setFormStatus(dom.backgroundPaletteStatus, 'Could not extract image colors. The selected theme is still in use.', 'error');
  }
}

function setBackgroundPalette(enabled) {
  dom.backgroundPalette.checked = enabled;
  try {
    if (enabled) localStorage.setItem(BACKGROUND_PALETTE_KEY, 'image');
    else localStorage.removeItem(BACKGROUND_PALETTE_KEY);
  } catch {
    // The selection still applies to this page.
  }
  void refreshBackgroundPalette();
}

function setBackgroundPaletteMode(mode) {
  backgroundPaletteMode = mode === 'light' || mode === 'dark' ? mode : 'auto';
  for (const input of dom.backgroundPaletteModes?.querySelectorAll('input') ?? []) {
    input.checked = input.value === backgroundPaletteMode;
  }
  try {
    localStorage.setItem(BACKGROUND_PALETTE_MODE_KEY, backgroundPaletteMode);
  } catch {
    // The selection still applies to this page.
  }
}

function openBackgroundStore() {
  return new Promise((resolve, reject) => {
    if (!window.indexedDB) {
      reject(new Error('IndexedDB is unavailable'));
      return;
    }
    const request = indexedDB.open(BACKGROUND_DB, 1);
    request.onupgradeneeded = () => request.result.createObjectStore(BACKGROUND_STORE);
    request.onsuccess = () => resolve(request.result);
    request.onerror = () => reject(request.error || new Error('IndexedDB could not be opened'));
  });
}

async function backgroundTransaction(mode, action) {
  const db = await openBackgroundStore();
  try {
    return await new Promise((resolve, reject) => {
      const transaction = db.transaction(BACKGROUND_STORE, mode);
      const request = action(transaction.objectStore(BACKGROUND_STORE));
      transaction.oncomplete = () => resolve(request?.result);
      transaction.onerror = () => reject(transaction.error || new Error('Background storage failed'));
      transaction.onabort = () => reject(transaction.error || new Error('Background storage was aborted'));
    });
  } finally {
    db.close();
  }
}

async function encodeBackground(file) {
  if (!BACKGROUND_TYPES.has(file.type)) throw new Error('Choose a PNG, JPEG, WebP, GIF, or AVIF image.');
  if (file.size > BACKGROUND_MAX_INPUT_BYTES) throw new Error('Choose an image smaller than 40 MB.');
  let bitmap;
  try {
    bitmap = await createImageBitmap(file);
  } catch {
    throw new Error('This image could not be decoded.');
  }
  const size = fitWithin(bitmap.width, bitmap.height, BACKGROUND_MAX_EDGE);
  const canvas = document.createElement('canvas');
  canvas.width = size.width;
  canvas.height = size.height;
  canvas.getContext('2d').drawImage(bitmap, 0, 0, size.width, size.height);
  bitmap.close?.();
  const blob = await new Promise((resolve) => canvas.toBlob(resolve, 'image/jpeg', 0.86));
  if (!blob) throw new Error('This image could not be converted.');
  return blob;
}

function applyBackgroundSettings(settings) {
  dom.root.style.setProperty('--backdrop-dim', String(settings.dim / 100));
  dom.root.style.setProperty('--backdrop-blur', `${settings.blur}px`);
  dom.backgroundDim.value = String(settings.dim);
  dom.backgroundBlur.value = String(settings.blur);
  dom.backgroundDimValue.textContent = `${settings.dim}%`;
  dom.backgroundBlurValue.textContent = settings.blur ? `${settings.blur} px` : 'Off';
}

function showBackground(blob) {
  if (backgroundUrl) URL.revokeObjectURL(backgroundUrl);
  backgroundUrl = blob ? URL.createObjectURL(blob) : '';
  const image = backgroundUrl ? `url("${backgroundUrl}")` : '';
  dom.appBackdrop.style.backgroundImage = image;
  dom.backgroundPreview.style.backgroundImage = image;
  dom.appBackdrop.hidden = !backgroundUrl;
  dom.backgroundPreview.dataset.empty = String(!backgroundUrl);
  dom.backgroundRemove.disabled = backgroundBusy || !blob;
  backgroundBlob = blob;
  dom.backgroundPalette.disabled = !blob;
  if (blob) void refreshBackgroundPalette();
  else setBackgroundPalette(false);
  if (backgroundUrl) document.body.dataset.backdrop = 'image';
  else delete document.body.dataset.backdrop;
}

function saveBackgroundSettings() {
  const settings = parseBackgroundSettings(JSON.stringify({
    dim: dom.backgroundDim.value, blur: dom.backgroundBlur.value,
  }));
  applyBackgroundSettings(settings);
  try {
    localStorage.setItem(BACKGROUND_SETTINGS_KEY, JSON.stringify(settings));
  } catch {
    // The values still apply to this page.
  }
}

function rememberBackground(entries, entry) {
  return [entry, ...entries.filter(({ id }) => id !== entry.id)].slice(0, BACKGROUND_HISTORY_LIMIT);
}

function updateBackgroundControls() {
  dom.backgroundChoose.disabled = backgroundBusy;
  dom.backgroundRemove.disabled = backgroundBusy || !backgroundBlob;
  for (const button of dom.backgroundHistoryList.querySelectorAll('button')) {
    button.disabled = backgroundBusy;
  }
}

function renderBackgroundHistory() {
  dom.backgroundHistoryList.replaceChildren();
  for (const url of recentBackgroundUrls) URL.revokeObjectURL(url);
  recentBackgroundUrls = [];
  recentBackgrounds.forEach((entry, index) => {
    const active = entry.id === currentBackgroundId && Boolean(backgroundBlob);
    const item = document.createElement('li');
    item.className = 'background-tile';
    if (active) item.dataset.active = 'true';
    const apply = document.createElement('button');
    apply.type = 'button';
    apply.className = 'background-tile-apply';
    apply.setAttribute('aria-pressed', String(active));
    apply.setAttribute('aria-label', `${active ? 'Current background' : 'Use background'} ${index + 1}: ${entry.name}`);
    apply.title = entry.name;
    const thumbnail = document.createElement('img');
    thumbnail.alt = '';
    const url = URL.createObjectURL(entry.blob);
    recentBackgroundUrls.push(url);
    thumbnail.src = url;
    const name = document.createElement('span');
    name.className = 'background-tile-name';
    name.textContent = entry.name;
    apply.append(thumbnail, name);
    apply.addEventListener('click', () => {
      if (!active) void runBackgroundChange(() => useRecentBackground(entry));
    });
    const remove = document.createElement('button');
    remove.type = 'button';
    remove.className = 'background-tile-delete';
    remove.textContent = '×';
    remove.title = 'Delete from history';
    remove.setAttribute('aria-label', `Delete background ${index + 1} from history: ${entry.name}`);
    remove.addEventListener('click', () => void runBackgroundChange(() => deleteRecentBackground(entry.id)));
    item.append(apply, remove);
    dom.backgroundHistoryList.append(item);
  });
  if (recentBackgrounds.length < BACKGROUND_HISTORY_LIMIT) {
    const item = document.createElement('li');
    item.className = 'background-tile background-tile-add';
    const add = document.createElement('button');
    add.type = 'button';
    add.className = 'background-tile-apply';
    add.setAttribute('aria-label', 'Add a background image');
    add.textContent = '+';
    add.addEventListener('click', () => dom.backgroundFile.click());
    item.append(add);
    dom.backgroundHistoryList.append(item);
  }
  dom.backgroundHistoryCount.textContent = `${recentBackgrounds.length} / ${BACKGROUND_HISTORY_LIMIT}`;
  dom.backgroundHistory.hidden = recentBackgrounds.length === 0;
  updateBackgroundControls();
}

async function runBackgroundChange(action) {
  if (backgroundBusy) return;
  backgroundBusy = true;
  updateBackgroundControls();
  try {
    await action();
  } catch {
    setFormStatus(dom.backgroundStatus, 'Could not update background history in this browser.', 'error');
  } finally {
    backgroundBusy = false;
    updateBackgroundControls();
  }
}

async function useRecentBackground(entry) {
  const next = rememberBackground(recentBackgrounds, entry);
  showBackground(entry.blob);
  currentBackgroundId = entry.id;
  try {
    await backgroundTransaction('readwrite', (store) => {
      store.put(entry.blob, 'current');
      store.put(entry.id, 'current-id');
      return store.put(next, 'recent');
    });
    recentBackgrounds = next;
    renderBackgroundHistory();
    setFormStatus(dom.backgroundStatus, 'Background saved in this browser.', 'ok');
  } catch {
    setFormStatus(dom.backgroundStatus, 'Background applied, but this browser could not store it; it resets on reload.', 'error');
  }
}

async function deleteRecentBackground(id) {
  const next = recentBackgrounds.filter((entry) => entry.id !== id);
  await backgroundTransaction('readwrite', (store) => store.put(next, 'recent'));
  recentBackgrounds = next;
  renderBackgroundHistory();
  setFormStatus(dom.backgroundStatus, 'Background removed from history.', 'ok');
}

async function chooseBackground(file) {
  if (!file) return;
  setFormStatus(dom.backgroundStatus, 'Preparing image…', 'busy');
  try {
    const blob = await encodeBackground(file);
    await useRecentBackground({ id: crypto.randomUUID(), name: file.name.slice(0, 120), blob });
  } catch (error) {
    setFormStatus(dom.backgroundStatus, error.message, 'error');
  } finally {
    dom.backgroundFile.value = '';
  }
}

async function removeBackground() {
  showBackground(null);
  currentBackgroundId = null;
  renderBackgroundHistory();
  try {
    await backgroundTransaction('readwrite', (store) => {
      store.delete('current-id');
      return store.delete('current');
    });
    setFormStatus(dom.backgroundStatus, 'Background removed.', 'ok');
  } catch {
    setFormStatus(dom.backgroundStatus, 'Background removed for this page; stored data could not be cleared.', 'error');
  }
}

async function restoreBackground() {
  updateBackgroundControls();
  try {
    applyBackgroundSettings(parseBackgroundSettings(localStorage.getItem(BACKGROUND_SETTINGS_KEY)));
  } catch {
    applyBackgroundSettings({ ...BACKGROUND_DEFAULTS });
  }
  try {
    const blob = await backgroundTransaction('readonly', (store) => store.get('current'));
    if (blob instanceof Blob) {
      try {
        dom.backgroundPalette.checked = localStorage.getItem(BACKGROUND_PALETTE_KEY) === 'image';
      } catch {
        // Keep the image without applying a saved palette preference.
      }
      showBackground(blob);
    }
    const storedId = await backgroundTransaction('readonly', (store) => store.get('current-id'));
    const recent = await backgroundTransaction('readonly', (store) => store.get('recent'));
    if (Array.isArray(recent)) {
      recentBackgrounds = recent.slice(0, BACKGROUND_HISTORY_LIMIT).filter((entry) =>
        entry && typeof entry.id === 'string' && typeof entry.name === 'string' && entry.blob instanceof Blob);
      // Older stores have no current-id; the front entry was the last one applied.
      if (blob instanceof Blob) currentBackgroundId = typeof storedId === 'string' ? storedId : recentBackgrounds[0]?.id ?? null;
    } else if (recent === undefined && blob instanceof Blob) {
      const initial = [{ id: 'previous-background', name: 'Previous background', blob }];
      await backgroundTransaction('readwrite', (store) => store.put(initial, 'recent'));
      recentBackgrounds = initial;
      currentBackgroundId = 'previous-background';
    }
  } catch {
    // No stored background, or storage is blocked: keep the theme background.
  } finally {
    backgroundBusy = false;
    renderBackgroundHistory();
  }
}

dom.backgroundChoose.addEventListener('click', () => dom.backgroundFile.click());
dom.backgroundFile.addEventListener('change', () => void runBackgroundChange(() => chooseBackground(dom.backgroundFile.files?.[0])));
dom.backgroundRemove.addEventListener('click', () => void runBackgroundChange(removeBackground));
dom.backgroundDim.addEventListener('input', saveBackgroundSettings);
dom.backgroundBlur.addEventListener('input', saveBackgroundSettings);
dom.backgroundPalette.addEventListener('change', () => setBackgroundPalette(dom.backgroundPalette.checked));
dom.backgroundPaletteModes.addEventListener('change', ({ target }) => {
  if (target.name !== 'background-palette-mode') return;
  setBackgroundPaletteMode(target.value);
  void refreshBackgroundPalette();
});
try {
  setBackgroundPaletteMode(localStorage.getItem(BACKGROUND_PALETTE_MODE_KEY));
} catch {
  setBackgroundPaletteMode('auto');
}
dom.theme.addEventListener('change', ({ target }) => {
  if (target.name === 'theme' && target.checked && themes.has(target.value)) {
    setBackgroundPalette(false);
  }
});
void restoreBackground();

/* Composer status, menus, and reply usage */

const TEAMWORK_KEY = 'nexus.teamwork';
const EFFORT_LABELS = { inherit: 'Default', low: 'Low', none: 'Off' };
const SVG_NS = 'http://www.w3.org/2000/svg';

// Compact token counts for chips: 57285 -> "57.3K", 1000000 -> "1M".
function formatCompactTokens(value) {
  if (!Number.isFinite(value)) return '—';
  if (value < 1000) return String(Math.round(value));
  if (value < 1_000_000) return `${Math.round(value / 100) / 10}K`;
  return `${Math.round(value / 100_000) / 10}M`;
}

function icon(paths, size = 15) {
  const svg = document.createElementNS(SVG_NS, 'svg');
  svg.setAttribute('width', String(size));
  svg.setAttribute('height', String(size));
  svg.setAttribute('viewBox', '0 0 24 24');
  svg.setAttribute('fill', 'none');
  svg.setAttribute('stroke', 'currentColor');
  svg.setAttribute('stroke-width', '1.8');
  svg.setAttribute('stroke-linecap', 'round');
  svg.setAttribute('stroke-linejoin', 'round');
  svg.setAttribute('aria-hidden', 'true');
  for (const d of paths) {
    const path = document.createElementNS(SVG_NS, 'path');
    path.setAttribute('d', d);
    svg.append(path);
  }
  return svg;
}

const ICONS = {
  copy: ['M9 9h10v10H9z', 'M5 15V5h10'],
  check: ['M5 12.5 10 17l9-10'],
  usage: ['M4 7c0-1.7 3.6-3 8-3s8 1.3 8 3-3.6 3-8 3-8-1.3-8-3Z', 'M4 7v5c0 1.7 3.6 3 8 3s8-1.3 8-3V7', 'M4 12v5c0 1.7 3.6 3 8 3s8-1.3 8-3v-5'],
  shield: ['M12 3 5 6v5c0 4.5 3 8.3 7 10 4-1.7 7-5.5 7-10V6Z'],
};

function readTeamworkPreference() {
  try {
    return localStorage.getItem(TEAMWORK_KEY) !== 'off';
  } catch {
    return true;
  }
}

function selectedModelEntry() {
  const id = state.session ? state.sessionModel : state.selection?.model;
  return state.config?.models?.find((entry) => entry.id === id) || null;
}

function sessionProviderEntry() {
  const id = state.session ? state.sessionProvider : state.selection?.provider;
  return state.config?.providers?.find((entry) => entry.id === id) || null;
}

// Effective task effort for the selected model: model, provider, then global.
function effectiveEffort() {
  if (!state.selection || state.selection.demo) return 'inherit';
  const model = state.config?.models?.find((entry) => entry.id === state.selection.model);
  const provider = state.config?.providers?.find((entry) => entry.id === state.selection.provider);
  return model?.task_reasoning_effort || provider?.task_reasoning_effort
    || state.config?.task_reasoning_effort || 'inherit';
}

// Teamwork needs a configured model and a bound local project. The server
// makes the final decision and reports it when the session is created.
function teamworkEligibility() {
  if (!state.connected) return 'Connect to use agent teamwork.';
  if (!state.selection || state.selection.demo) return 'Needs a configured model; the demo script cannot start helpers.';
  if (!state.projects?.enabled) return 'Needs local project tools (start the server with --tools real).';
  const project = state.session ? state.sessionProjectId : state.projectSelection?.id;
  if (!project) return 'Needs a project for this session.';
  return '';
}

function teamworkHelpText() {
  const base = 'Lets the agent start read-only helper tasks that explore this project in the background and report back. Helpers cannot write files or run commands.';
  const blocked = teamworkEligibility();
  if (blocked) return `${base} ${blocked}`;
  if (state.session && state.sessionDelegation === true && state.sessionDelegationActive === false) {
    return `${base} The server did not enable helpers for this session's model.`;
  }
  if (state.session && (state.sessionDelegation ?? true) !== state.teamwork) {
    return state.savedPersistence
      ? `${base} The change applies when you send the next message.`
      : `${base} Start a new session to apply the change (switching needs --session-store).`;
  }
  return base;
}

function updateModelTrigger() {
  const label = state.selection ? state.selection.label : 'No model';
  dom.modelTriggerName.textContent = label;
  const effort = EFFORT_LABELS[effectiveEffort()] || 'Default';
  dom.modelTriggerEffort.textContent = state.selection && !state.selection.demo ? effort : '';
  dom.modelTrigger.title = state.selection ? `${state.selection.label} — ${state.selection.detail}` : 'No model';
  dom.menuModelValue.textContent = label;
  dom.menuEffortValue.textContent = effort;
  const blocked = teamworkEligibility();
  dom.teamworkSwitch.setAttribute('aria-checked', String(!blocked && state.teamwork !== false));
  dom.teamworkSwitch.disabled = Boolean(blocked);
  dom.teamworkHelp.textContent = teamworkHelpText();
  dom.menuEffort.disabled = !state.selection || state.selection.demo;
  updateToolsModeChip();
  renderContextMeter();
  renderHelperStatus();
}

function updateToolsModeChip() {
  const mode = state.connected ? state.projects?.mode : '';
  const labels = {
    development: ['Auto in project', 'Project and temporary file changes and sandboxed commands run automatically. External paths ask first.'],
    strict: ['Ask first', 'Edits and commands wait for your approval. Reads stay inside the project.'],
    fakes: ['Demo tools', 'Scripted test tools; nothing touches your files.'],
    container: ['Container', 'Tools run in the configured container.'],
  };
  const entry = labels[mode];
  dom.toolsMode.hidden = !entry;
  if (!entry) return;
  dom.toolsMode.replaceChildren(icon(ICONS.shield, 13), document.createTextNode(entry[0]));
  dom.toolsMode.title = entry[1];
}

function renderHelperStatus() {
  let running = 0;
  const tasks = [];
  if (state.active) {
    for (const job of state.delegateJobs.values()) {
      if (job.state !== 'running' || job.run !== state.run) continue;
      running += 1;
      if (job.task) tasks.push(`${delegateRoleLabel(job.role)} · ${job.task}`);
    }
  } else {
    running = Number(state.inbox?.pending_child_results) || 0;
  }
  dom.helperStatus.hidden = running === 0;
  dom.helperStatus.textContent = `${running} helper${running === 1 ? '' : 's'} running`;
  dom.helperStatus.title = tasks.length ? tasks.join('\n') : 'Background helper results arrive when they finish.';
}

function updateContextEstimate(detail) {
  const estimated = Number(detail.estimated_input_tokens);
  const window = Number(detail.context_window_tokens);
  if (!Number.isFinite(estimated) || !Number.isFinite(window) || window <= 0) return;
  state.context = {
    estimated,
    window,
    trigger: Number.isFinite(detail.context_trigger_percent) ? detail.context_trigger_percent : null,
    target: Number.isFinite(detail.context_target_percent) ? detail.context_target_percent : null,
    maxOutput: Number.isFinite(detail.max_output_tokens) ? detail.max_output_tokens : null,
    strategy: typeof detail.compaction_strategy === 'string' ? detail.compaction_strategy : '',
  };
  renderContextMeter();
}

function renderContextMeter() {
  const context = state.context;
  dom.contextMeter.hidden = !context;
  if (!context) {
    if (dom.contextPanel.matches?.(':popover-open')) dom.contextPanel.hidePopover();
    return;
  }
  const ratio = Math.min(1, context.estimated / context.window);
  const percent = Math.round(ratio * 100);
  const label = context.estimated > 0 && percent === 0 ? '<1%' : `${percent}%`;
  dom.contextPercent.textContent = label;
  dom.contextPanelPercent.textContent = label;
  dom.contextRing.setAttribute('stroke-dasharray', `${Math.max(ratio * 100, 0.5)} 100`);
  const warnAt = context.trigger ?? 80;
  dom.contextMeter.dataset.level = percent >= warnAt ? 'high' : percent >= warnAt - 15 ? 'medium' : 'low';
  dom.contextMeter.setAttribute('aria-label', `Context usage: ${label} of ${formatCompactTokens(context.window)} tokens`);
  dom.contextPanelTotal.textContent = `~${formatCompactTokens(context.estimated)} / ${formatCompactTokens(context.window)}`;
  dom.contextBarFill.style.width = `${ratio * 100}%`;
  dom.contextBarTrigger.hidden = context.trigger === null;
  if (context.trigger !== null) dom.contextBarTrigger.style.left = `${Math.min(100, context.trigger)}%`;
  const rows = [['Input estimate', `~${formatCompactTokens(context.estimated)}`]];
  if (context.maxOutput !== null) rows.push(['Output reserve', `~${formatCompactTokens(context.maxOutput)}`]);
  if (context.trigger !== null) {
    const method = context.strategy === 'truncate' ? 'truncate' : 'summarize';
    rows.push(['Compacts at', `${context.trigger}%${context.target !== null ? ` → ${context.target}%` : ''} · ${method}`]);
  }
  fillRows(dom.contextRows, rows);
}

function fillRows(list, rows) {
  list.replaceChildren();
  for (const [name, value, tone] of rows) {
    const row = node('div', 'popover-row');
    if (tone) row.dataset.tone = tone;
    row.append(node('dt', '', name), node('dd', '', value));
    list.append(row);
  }
}

function copyButton(text) {
  const button = node('button', 'reply-action');
  button.type = 'button';
  button.title = 'Copy reply';
  button.setAttribute('aria-label', 'Copy reply');
  button.append(icon(ICONS.copy));
  button.addEventListener('click', async () => {
    try {
      await navigator.clipboard.writeText(text);
      button.replaceChildren(icon(ICONS.check));
      button.title = 'Copied';
      setTimeout(() => {
        button.replaceChildren(icon(ICONS.copy));
        button.title = 'Copy reply';
      }, 1500);
    } catch {
      button.title = 'Copy failed: clipboard access was refused';
    }
  });
  return button;
}

let popoverSequence = 0;

// Provider counters differ by protocol: Chat Completions and Responses
// include cached tokens in input; Anthropic Messages reports them separately.
function usageBreakdown(usage, protocol) {
  if (!usage) return null;
  const known = (value) => Number.isFinite(value);
  const inclusive = protocol !== 'anthropic_messages';
  const cacheRead = known(usage.cacheRead) ? usage.cacheRead : null;
  const cacheWrite = known(usage.cacheWrite) ? usage.cacheWrite : null;
  const input = known(usage.input) ? usage.input : null;
  const output = known(usage.output) ? usage.output : null;
  if (input === null && output === null) return null;
  const promptTotal = input === null ? null
    : inclusive ? input : input + (cacheRead || 0) + (cacheWrite || 0);
  const uncached = input === null ? null : inclusive ? Math.max(0, input - (cacheRead || 0)) : input;
  return {
    total: (promptTotal || 0) + (output || 0),
    promptTotal, uncached, cacheRead, cacheWrite, output,
    hitRate: promptTotal && cacheRead !== null ? cacheRead / promptTotal : null,
  };
}

function usageChip(detail) {
  const provider = sessionProviderEntry();
  const breakdown = usageBreakdown(state.lastUsage, provider?.protocol);
  const button = node('button', 'reply-action usage-chip');
  button.type = 'button';
  button.append(icon(ICONS.usage));
  button.append(node('span', '', breakdown ? `Usage ${formatCompactTokens(breakdown.total)} tok` : 'Usage not reported'));
  const panel = node('div', 'popover-panel usage-panel');
  panel.id = `usage-panel-${++popoverSequence}`;
  panel.setAttribute('popover', '');
  panel.setAttribute('role', 'dialog');
  panel.setAttribute('aria-label', 'Run usage');
  button.setAttribute('popovertarget', panel.id);
  button.setAttribute('aria-label', 'Show run usage');
  const head = node('div', 'popover-head');
  const title = node('span', 'popover-title');
  title.append(icon(ICONS.usage), document.createTextNode('Run usage'));
  head.append(title, node('span', '', breakdown ? `${breakdown.total.toLocaleString()} tok` : 'Not reported'));
  panel.append(head);
  const rows = [];
  const model = selectedModelEntry();
  if (state.sessionProvider) rows.push(['Provider / model', `${state.sessionProvider}/${model?.name || state.sessionModel || 'default'}`]);
  else rows.push(['Model', 'Demo script']);
  const tokens = (value) => value === null ? 'Not reported' : `${value.toLocaleString()} tok`;
  if (breakdown) {
    if (breakdown.hitRate !== null) rows.push(['Cache hit', `${Math.round(breakdown.hitRate * 1000) / 10}%`]);
    rows.push(['Uncached input', tokens(breakdown.uncached)]);
    if (breakdown.cacheRead !== null) rows.push(['Cached input', tokens(breakdown.cacheRead)]);
    if (breakdown.cacheWrite !== null) rows.push(['Cache write', tokens(breakdown.cacheWrite)]);
    rows.push(['Output', tokens(breakdown.output)]);
  }
  let helperInput = 0;
  let helperOutput = 0;
  let helperKnown = false;
  for (const record of state.tools.values()) {
    if (!record.helperUsage) continue;
    helperKnown = true;
    helperInput += record.helperUsage.input || 0;
    helperOutput += record.helperUsage.output || 0;
  }
  if (helperKnown) rows.push(['Helpers (separate)', `${formatCompactTokens(helperInput)} in · ${formatCompactTokens(helperOutput)} out`]);
  if (detail.persistence) {
    rows.push(['History', persistenceLabel(detail.persistence), detail.persistence === 'SaveFailed' ? 'error' : '']);
  }
  const list = node('dl', 'popover-rows');
  fillRows(list, rows);
  panel.append(list);
  if (!breakdown) panel.append(node('p', 'popover-note', 'The provider did not report token counts for this run.'));
  const wrap = node('span', 'usage-wrap');
  wrap.append(button, panel);
  panel.addEventListener('toggle', (event) => {
    if (event.newState === 'open') positionPopover(panel, button, 'start');
  });
  return wrap;
}

// Popovers sit in the top layer; place them above their trigger and keep
// them on screen. Positions use CSSOM, which the style CSP allows.
function positionPopover(panel, anchor, align = 'end') {
  // Rectangles are in viewport pixels; inline offsets are scaled by the page
  // zoom, so convert back before assigning them.
  const zoom = interfaceZoom();
  const rect = anchor.getBoundingClientRect();
  const box = panel.getBoundingClientRect();
  const margin = 8;
  let left = align === 'start' ? rect.left : rect.right - box.width;
  left = Math.max(margin, Math.min(left, window.innerWidth - box.width - margin));
  let top = rect.top - box.height - margin;
  if (top < margin) top = Math.min(rect.bottom + margin, window.innerHeight - box.height - margin);
  panel.style.left = `${left / zoom}px`;
  panel.style.top = `${Math.max(margin, top) / zoom}px`;
}

function showMenuView(view) {
  for (const element of [dom.menuMain, dom.menuModels, dom.menuEfforts]) element.hidden = element !== view;
  if (view === dom.menuModels) renderModelOptions();
  if (view === dom.menuEfforts) renderEffortOptions();
  positionPopover(dom.modelMenu, dom.modelTrigger);
  (view.querySelector('[aria-checked="true"]') || view.querySelector('button:not(:disabled)'))?.focus();
}

function menuOption(label, detail, checked, disabled, onSelect) {
  const button = node('button', 'menu-option');
  button.type = 'button';
  button.setAttribute('role', 'menuitemradio');
  button.setAttribute('aria-checked', String(checked));
  button.disabled = disabled;
  const text = node('span', 'menu-option-text');
  text.append(node('span', '', label));
  if (detail) text.append(node('span', 'menu-option-detail', detail));
  button.append(text);
  button.addEventListener('click', onSelect);
  return button;
}

function renderModelOptions() {
  dom.menuModelOptions.replaceChildren();
  const selected = state.selection ? choiceKey(state.selection) : '';
  state.choices.forEach((choice, index) => {
    dom.menuModelOptions.append(menuOption(choice.label, choice.detail, choiceKey(choice) === selected, dom.model.disabled, () => {
      dom.model.value = String(index);
      dom.model.dispatchEvent(new Event('change'));
      showMenuView(dom.menuMain);
    }));
  });
}

function renderEffortOptions() {
  dom.menuEffortOptions.replaceChildren();
  const current = state.config?.task_reasoning_effort || 'inherit';
  const busy = !state.connected || state.thinkingSaving || Boolean(state.configSaving);
  for (const [value, detail] of [['inherit', 'Use each adapter\'s normal behavior'], ['low', 'Request low-effort thinking'], ['none', 'Turn thinking off']]) {
    dom.menuEffortOptions.append(menuOption(EFFORT_LABELS[value], detail, value === current, busy, () => {
      // Reuse the Settings form so validation, status, and refresh stay in one place.
      dom.taskThinking.value = value;
      dom.thinkingForm.requestSubmit();
      showMenuView(dom.menuMain);
    }));
  }
  const effective = effectiveEffort();
  const override = effective !== current ? ` This model resolves to ${EFFORT_LABELS[effective] || effective} through its own or its provider's override.` : '';
  dom.menuEffortHelp.textContent = `Global default for new sessions. Only detected Ollama Direct / Chat Completions endpoints accept Low or Off; other adapters refuse an override.${override}`;
}

/* Sidebar visibility */

const SIDEBAR_KEY = 'nexus.sidebar';
const narrowSidebar = window.matchMedia('(max-width: 720px)');

// Wide layouts remember the collapsed state; narrow layouts use a drawer that
// starts closed and is never persisted.
function readSidebarPreference() {
  try {
    return localStorage.getItem(SIDEBAR_KEY) !== 'closed';
  } catch {
    return true;
  }
}

function setSidebarOpen(open, { persist = false } = {}) {
  document.body.dataset.sidebar = open ? 'open' : 'closed';
  const label = open ? 'Hide sidebar' : 'Show sidebar';
  dom.toggleSidebar.setAttribute('aria-expanded', String(open));
  dom.toggleSidebar.setAttribute('aria-label', label);
  dom.toggleSidebar.title = label;
  dom.sidebarBackdrop.hidden = !(open && narrowSidebar.matches);
  dom.sidebar.inert = !open;
  if (!persist || narrowSidebar.matches) return;
  try {
    localStorage.setItem(SIDEBAR_KEY, open ? 'open' : 'closed');
  } catch {
    // The preference is a convenience; the toggle still works for this page.
  }
}

function applySidebarLayout() {
  setSidebarOpen(narrowSidebar.matches ? false : readSidebarPreference());
}

function closeNarrowSidebar() {
  if (narrowSidebar.matches && document.body.dataset.sidebar === 'open') {
    setSidebarOpen(false);
    dom.toggleSidebar.focus();
  }
}

/* Settings navigation */

function selectSettingsSection(id) {
  if (!dom.settingsPanels.some((panel) => panel.id === id)) return;
  for (const panel of dom.settingsPanels) panel.hidden = panel.id !== id;
  for (const button of dom.settingsButtons) {
    if (button.dataset.settingsSection === id) button.setAttribute('aria-current', 'page');
    else button.removeAttribute('aria-current');
  }
  dom.settingsContent.scrollTop = 0;
}

/* Wiring */

for (const button of dom.settingsButtons) {
  button.addEventListener('click', () => selectSettingsSection(button.dataset.settingsSection));
}

dom.openSettings.addEventListener('click', () => {
  if (!state.connected) selectSettingsSection('connection-form');
  dom.settings.showModal();
  requestAnimationFrame(() => {
    if (!state.connected) dom.token.focus();
    else dom.settings.querySelector('[aria-current="page"]')?.focus();
  });
});
dom.closeSettings.addEventListener('click', () => dom.settings.close());
dom.welcomeConnect.addEventListener('click', () => dom.openSettings.click());
dom.modelMenu.addEventListener('toggle', (event) => {
  dom.modelTrigger.setAttribute('aria-expanded', String(event.newState === 'open'));
  if (event.newState === 'open') {
    dom.teamworkHelp.hidden = true;
    dom.teamworkInfo.setAttribute('aria-expanded', 'false');
    showMenuView(dom.menuMain);
  }
});
dom.contextPanel.addEventListener('toggle', (event) => {
  dom.contextMeter.setAttribute('aria-expanded', String(event.newState === 'open'));
  if (event.newState === 'open') positionPopover(dom.contextPanel, dom.contextMeter);
});
dom.menuModel.addEventListener('click', () => showMenuView(dom.menuModels));
dom.menuEffort.addEventListener('click', () => showMenuView(dom.menuEfforts));
for (const back of dom.modelMenu.querySelectorAll('[data-menu-back]')) {
  back.addEventListener('click', () => showMenuView(dom.menuMain));
}
dom.teamworkInfo.addEventListener('click', () => {
  const open = dom.teamworkHelp.hidden;
  dom.teamworkHelp.hidden = !open;
  dom.teamworkInfo.setAttribute('aria-expanded', String(open));
  positionPopover(dom.modelMenu, dom.modelTrigger);
});
dom.teamworkSwitch.addEventListener('click', () => {
  if (dom.teamworkSwitch.disabled) return;
  state.teamwork = state.teamwork === false;
  try {
    localStorage.setItem(TEAMWORK_KEY, state.teamwork ? 'on' : 'off');
  } catch {
    // The choice still applies to this page.
  }
  updateModelTrigger();
  dom.teamworkHelp.hidden = false;
  dom.teamworkInfo.setAttribute('aria-expanded', 'true');
  positionPopover(dom.modelMenu, dom.modelTrigger);
});
state.teamwork = readTeamworkPreference();
dom.toggleSidebar.addEventListener('click', () => {
  const open = document.body.dataset.sidebar !== 'open';
  setSidebarOpen(open, { persist: true });
  if (open && narrowSidebar.matches) dom.newSession.focus();
});
dom.sidebarBackdrop.addEventListener('click', closeNarrowSidebar);
narrowSidebar.addEventListener('change', applySidebarLayout);
dom.sidebar.addEventListener('keydown', (event) => {
  if (event.key === 'Escape') closeNarrowSidebar();
});
// Choosing something in the drawer returns to the conversation.
dom.sidebar.addEventListener('click', (event) => {
  if (event.target.closest('.tree-session-open, .tree-project-add, #new-session, #add-project-sidebar')) {
    closeNarrowSidebar();
  }
});
applySidebarLayout();
dom.addProjectSidebar.addEventListener('click', () => {
  if (!dom.settings.open) dom.settings.showModal();
  selectSettingsSection('projects-block');
  requestAnimationFrame(() => {
    dom.chooseProjectFolder.focus();
  });
});
dom.settings.addEventListener('close', () => {
  dom.token.value = '';
});

dom.imageLightbox?.addEventListener('click', closeImageLightbox);
document.addEventListener('keydown', (event) => {
  if (event.key === 'Escape' && dom.imageLightbox && !dom.imageLightbox.hidden) {
    event.preventDefault();
    closeImageLightbox();
  }
});
dom.conversation.addEventListener('click', (event) => {
  const image = imageFromEvent(event);
  if (!image) return;
  event.preventDefault();
  event.stopPropagation();
  enlargeImage(image);
}, true);
dom.conversation.addEventListener('keydown', (event) => {
  if (event.key !== 'Enter' && event.key !== ' ') return;
  const image = imageFromEvent(event);
  if (!image || event.target !== image) return;
  event.preventDefault();
  enlargeImage(image);
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

dom.project.addEventListener('change', () => {
  const index = Number(dom.project.value);
  state.projectSelection = state.projectChoices[index] || null;
  renderSidebarTree();
  updateProjectDisplay();
  updateControls();
});

dom.chooseProjectFolder.addEventListener('click', () => void chooseProjectFolder());
dom.projectForm.addEventListener('submit', async (event) => {
  event.preventDefault();
  if (!state.connected || !state.projects?.enabled || state.projectPicking || state.projectSaving) return;
  const name = dom.projectName.value.trim();
  const path = dom.projectPath.value;
  if (!path) {
    setFormStatus(dom.projectStatus, 'Choose a project folder first.', 'error');
    return;
  }
  state.projectSaving = true;
  updateControls();
  setFormStatus(dom.projectStatus, 'Adding project…', 'busy');
  try {
    const body = { path };
    if (name) body.name = name;
    const payload = await api('/projects', { method: 'POST', body });
    state.projects = payload;
    populateProjects();
    const added = payload.project?.id;
    if (added && !state.session) {
      const choice = state.projectChoices.find((entry) => entry.id === added);
      if (choice) {
        state.projectSelection = choice;
        dom.project.value = String(state.projectChoices.indexOf(choice));
      }
    }
    renderProjectsList();
    renderSidebarTree();
    dom.projectName.value = '';
    dom.projectPath.value = '';
    dom.projectName.placeholder = 'Uses the folder name';
    setFormStatus(dom.projectStatus, `Added ${payload.project?.name || name}.`, 'ok');
    updateProjectDisplay();
    updateSessionDisplay();
    updateControls();
  } catch (error) {
    setFormStatus(dom.projectStatus, error.message, 'error');
    if (error instanceof ApiError && error.status === 401) markUnauthorized();
  } finally {
    state.projectSaving = false;
    updateControls();
  }
});

dom.newProvider.addEventListener('click', () => resetProviderForm({ open: true }));
dom.newModel.addEventListener('click', () => {
  if (!state.config?.providers?.length) return;
  resetModelForm({ open: true });
});
dom.cancelProvider.addEventListener('click', () => resetProviderForm());
dom.cancelModel.addEventListener('click', () => resetModelForm());
dom.modelAddProvider.addEventListener('click', () => selectSettingsSection('providers-block'));
for (const form of [dom.providerForm, dom.modelForm]) {
  const markDirty = () => {
    form.dataset.dirty = 'true';
    const provider = form === dom.providerForm;
    setFormStatus(provider ? dom.providerStatus : dom.modelStatus, 'Unsaved changes', 'busy');
  };
  form.addEventListener('input', markDirty);
  form.addEventListener('change', markDirty);
}
dom.providerCredentialKind.addEventListener('change', updateProviderCredentialFields);
dom.deleteProvider.addEventListener('click', () => deleteProvider(dom.providerForm.dataset.original));

dom.thinkingForm.addEventListener('submit', async (event) => {
  event.preventDefault();
  if (!state.connected || state.thinkingSaving || state.configSaving) return;
  const effort = dom.taskThinking.value;
  state.thinkingSaving = true;
  updateControls();
  setFormStatus(dom.thinkingStatus, 'Saving thinking…', 'busy');
  try {
    const summary = await api('/config/thinking', {
      method: 'POST',
      body: { task_reasoning_effort: effort },
    });
    state.config = summary;
    updateConnectionDetail();
    populateModels();
    renderConfigEditors({ hydrateThinking: true });
    setFormStatus(dom.thinkingStatus, 'Saved task thinking. Changes apply to new Web sessions.', 'ok');
    updateSessionDisplay();
  } catch (error) {
    setFormStatus(dom.thinkingStatus, error.message, 'error');
    if (error instanceof ApiError && error.status === 401) markUnauthorized();
  } finally {
    state.thinkingSaving = false;
    updateControls();
  }
});

dom.providerForm.addEventListener('submit', async (event) => {
  event.preventDefault();
  if (!state.connected || state.configSaving || state.thinkingSaving) return;
  const id = dom.providerId.value.trim();
  if (!dom.providerForm.dataset.original && state.config?.providers?.some((provider) => provider.id === id)) {
    setFormStatus(dom.providerStatus, 'This provider ID already exists. Choose another ID or edit the existing provider.', 'error');
    dom.providerId.focus();
    return;
  }
  const displayName = dom.providerDisplay.value.trim();
  const defaultModel = dom.providerDefaultModel.value.trim();
  const credentialName = dom.providerCredentialName.value.trim();
  if (!id || !displayName || !defaultModel || !credentialName) {
    setFormStatus(dom.providerStatus, 'Provider ID, display name, default model, and credential name are required.', 'error');
    return;
  }
  let contextWindow;
  try {
    contextWindow = optionalCount(dom.providerContextWindow, 'Context window');
  } catch (error) {
    setFormStatus(dom.providerStatus, error.message, 'error');
    return;
  }
  const endpoint = dom.providerEndpoint.value.trim() || null;
  const credentialKind = dom.providerCredentialKind.value;
  const credential = { [credentialKind]: credentialName };
  const body = {
    id,
    display_name: displayName,
    adapter: dom.providerAdapter.value,
    protocol: dom.providerProtocol.value,
    endpoint,
    enabled: dom.providerEnabled.value === 'true',
    credential,
    default_model: defaultModel,
    context_window_tokens: contextWindow,
    task_reasoning_effort: dom.providerTaskThinking.value,
  };
  if (credentialKind === 'stored' && dom.providerApiToken.value) {
    body.api_token = dom.providerApiToken.value;
  }
  dom.saveProvider.disabled = true;
  state.configSaving = 'provider';
  updateControls();
  setFormStatus(dom.providerStatus, 'Saving provider…', 'busy');
  try {
    const summary = await api('/config/providers', { method: 'POST', body });
    state.config = summary;
    updateConnectionDetail();
    populateModels();
    renderConfigEditors();
    editProvider(id, { force: true });
    setFormStatus(dom.providerStatus, `Saved ${displayName}. Changes apply to new sessions.`, 'ok');
    updateSessionDisplay();
    updateControls();
  } catch (error) {
    setFormStatus(dom.providerStatus, error.message, 'error');
    if (error instanceof ApiError && error.status === 401) markUnauthorized();
  } finally {
    state.configSaving = false;
    updateControls();
    dom.saveProvider.disabled = false;
  }
});

dom.modelForm.addEventListener('submit', async (event) => {
  event.preventDefault();
  if (!state.connected || state.configSaving || state.thinkingSaving) return;
  const id = dom.modelId.value.trim();
  if (!dom.modelForm.dataset.original && state.config?.models?.some((model) => model.id === id)) {
    setFormStatus(dom.modelStatus, 'This model ID already exists. Choose another ID or edit the existing model.', 'error');
    dom.modelId.focus();
    return;
  }
  const provider = dom.modelProvider.value;
  const name = dom.modelName.value.trim();
  if (!id || !provider || !name) {
    setFormStatus(dom.modelStatus, 'Model ID, provider, and vendor name are required.', 'error');
    return;
  }
  let contextWindow;
  let maxOutput;
  try {
    contextWindow = optionalCount(dom.modelContextWindow, 'Context window');
    maxOutput = optionalCount(dom.modelMaxOutput, 'Max output tokens');
  } catch (error) {
    setFormStatus(dom.modelStatus, error.message, 'error');
    return;
  }
  dom.saveModel.disabled = true;
  state.configSaving = 'model';
  updateControls();
  setFormStatus(dom.modelStatus, 'Saving model…', 'busy');
  try {
    const summary = await api('/config/models', {
      method: 'POST',
      body: {
        id,
        provider,
        name,
        context_window_tokens: contextWindow,
        max_output_tokens: maxOutput,
        task_reasoning_effort: dom.modelTaskThinking.value,
      },
    });
    state.config = summary;
    updateConnectionDetail();
    populateModels();
    renderConfigEditors();
    editModel(id, { force: true });
    setFormStatus(dom.modelStatus, `Saved ${name}. Changes apply to new sessions.`, 'ok');
    updateSessionDisplay();
    updateControls();
  } catch (error) {
    setFormStatus(dom.modelStatus, error.message, 'error');
    if (error instanceof ApiError && error.status === 401) markUnauthorized();
  } finally {
    state.configSaving = false;
    updateControls();
    dom.saveModel.disabled = !state.config?.providers?.length;
  }
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
dom.steerRun.addEventListener('click', () => void sendMessage({ steer: true }));

dom.cancel.addEventListener('click', () => void cancelRun());
dom.newSession.addEventListener('click', () => startNewSession());
dom.newSessionMobile.addEventListener('click', () => startNewSession());

dom.main.addEventListener('scroll', () => {
  state.pinned = nearBottom();
  updateJumpLatest();
});

dom.jumpLatest.addEventListener('click', () => {
  state.pinned = true;
  scrollToBottomSoon();
  updateJumpLatest();
  dom.message.focus();
});

window.addEventListener('beforeunload', (event) => {
  if (dom.providerForm.dataset.dirty !== 'true' && dom.modelForm.dataset.dirty !== 'true' && !state.configSaving) return;
  event.preventDefault();
  event.returnValue = '';
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

state.collapsedProjects = readCollapsedProjects();
updateSessionDisplay();
updateControls();
updateJumpLatest();
setConnectionStatus('Connecting…', 'busy');
setRunStatus('Connecting…', 'busy');
async function redeemDesktopLoginCode() {
  const fragment = new URLSearchParams(location.hash.slice(1));
  const code = fragment.get('nexus-login') || '';
  if (!code) return '';
  history.replaceState(null, '', `${location.pathname}${location.search}`);
  if (!TOKEN_PATTERN.test(code)) return '';
  try {
    const reply = await api('/desktop-login', {
      method: 'POST',
      credential: '',
      body: { code },
    });
    return typeof reply?.token === 'string' && TOKEN_PATTERN.test(reply.token)
      ? reply.token
      : '';
  } catch {
    return '';
  }
}

void (async () => {
  const credential = await redeemDesktopLoginCode();
  void connect(credential || readSavedCredential());
})();
