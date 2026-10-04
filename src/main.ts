import './style.css'
import { getVersion } from '@tauri-apps/api/app'
import { invoke } from '@tauri-apps/api/core'
import { LogicalSize, PhysicalPosition } from '@tauri-apps/api/dpi'
import { listen } from '@tauri-apps/api/event'
import { availableMonitors, currentMonitor, cursorPosition, getCurrentWindow, monitorFromPoint } from '@tauri-apps/api/window'
import { disable, enable, isEnabled } from '@tauri-apps/plugin-autostart'
import { openUrl } from '@tauri-apps/plugin-opener'
import { load, type Store } from '@tauri-apps/plugin-store'
import {
  createIcons,
  Download,
  Eye,
  EyeOff,
  ExternalLink,
  LogOut,
  RefreshCw,
} from 'lucide'

/** Display toggles for pool platforms (maps to Sub2API account.platform). */
type ModelKey = 'claude' | 'codex' | 'grok'

interface ModelVisibility {
  claude: boolean
  codex: boolean
  grok: boolean
}

interface PetSettings {
  baseUrl: string
  email: string
  alwaysOnTop: boolean
  autoStart: boolean
  /** Max accounts shown on the pet panel / tray. */
  maxDisplayAccounts: number
  /** Auto refresh interval in seconds. */
  refreshIntervalSec: number
  /** Which model families appear on the pet / tray. Default: all on. */
  showModels: ModelVisibility
  /**
   * Opacity of the frosted card behind the quota meters (0 = invisible, 1 = solid).
   * Stored as 0–1; settings UI uses percent.
   */
  cardOpacity: number
  windowX?: number
  windowY?: number
  windowMonitor?: { x: number; y: number }
}

interface LoginResult {
  status: 'connected' | 'requires2fa'
  temp_token?: string
  email_masked?: string
}

interface QuotaWindow {
  label: string
  used_percent: number
  remaining_percent: number
  reset_at?: string | null
}

interface AccountQuotaRow {
  id: number
  name: string
  status: string
  plan?: string
  platform?: string
  account_type?: string
  remaining_percent?: number | null
  windows?: QuotaWindow[]
  updated_at?: string | null
  source?: 'active' | 'cached' | string | null
}

const BAR_SEGMENTS = 5

const isDesktop = '__TAURI_INTERNALS__' in window
/** Windows WebView2 pays a steep cost for backdrop-filter on transparent always-on-top windows. */
const isWindows =
  typeof navigator !== 'undefined' && /Windows/i.test(navigator.userAgent || '')
if (isWindows) {
  document.documentElement.classList.add('is-windows')
}
const defaultShowModels: ModelVisibility = {
  claude: true,
  codex: true,
  grok: true,
}

const defaultSettings: PetSettings = {
  baseUrl: '',
  email: '',
  alwaysOnTop: true,
  autoStart: false,
  maxDisplayAccounts: 5,
  refreshIntervalSec: 30,
  showModels: { ...defaultShowModels },
  cardOpacity: 1.0,
}

const DISPLAY_COUNT_OPTIONS = [1, 2, 3, 4, 5, 6, 8, 10, 15, 20] as const
const REFRESH_INTERVAL_OPTIONS = [
  { value: 10, label: '10 秒' },
  { value: 15, label: '15 秒' },
  { value: 30, label: '30 秒' },
  { value: 60, label: '1 分钟' },
  { value: 120, label: '2 分钟' },
  { value: 300, label: '5 分钟' },
  { value: 600, label: '10 分钟' },
] as const

let settings = { ...defaultSettings }
let appStore: Store | null = null
let poolRows: AccountQuotaRow[] = []
let connected = false
let settingsOpen = false
let refreshing = false
let tempToken = ''
let autoRefreshTimer: number | undefined
let moveSaveTimer: number | undefined
let snapping = false

document.querySelector<HTMLDivElement>('#app')!.innerHTML = `
  <svg class="svg-defs" aria-hidden="true" style="position: absolute; width: 0; height: 0; overflow: hidden; pointer-events: none;">
    <defs>
      <!-- 5-Hour Session Quota (Left semi-circle across all models): High-light #4FA3FF -->
      <linearGradient id="gradient-quota-5h" x1="0%" y1="0%" x2="0%" y2="100%">
        <stop offset="0%" stop-color="#72b8ff" />
        <stop offset="100%" stop-color="#4FA3FF" />
      </linearGradient>

      <!-- 7-Day Weekly Quota (Right semi-circle & full circle across all models): High-light #4FA3FF -->
      <linearGradient id="gradient-quota-7d" x1="0%" y1="0%" x2="0%" y2="100%">
        <stop offset="0%" stop-color="#72b8ff" />
        <stop offset="100%" stop-color="#4FA3FF" />
      </linearGradient>

      <!-- Alert / Low Quota Gradient (<= 15%) -->
      <linearGradient id="gradient-alert" x1="0%" y1="0%" x2="0%" y2="100%">
        <stop offset="0%" stop-color="#f87171" />
        <stop offset="100%" stop-color="#dc2626" />
      </linearGradient>
    </defs>
  </svg>

  <main class="pet-shell" id="pet-shell">
    <section class="pet-stage" aria-live="polite">
      <div class="quota-panel" id="quota-dock" aria-label="账号池额度面板">
        <div
          class="meter-card"
          id="meter-card"
          role="button"
          tabindex="0"
          aria-label="${isWindows ? '单击刷新 · 拖动调整屏幕和位置' : '单击刷新 · 拖动调整屏幕和位置 · 右键打开菜单'}"
          title="${isWindows ? '单击刷新 · 拖动调整屏幕和位置' : '单击刷新 · 拖动调整屏幕和位置 · 右键菜单'}"
        >
          <svg class="dock-backdrop-svg" id="dock-backdrop-svg" aria-hidden="true" focusable="false">
            <path class="dock-backdrop-path" id="dock-backdrop-path" />
          </svg>
          <div class="meter-board" id="account-list" role="list"></div>
        </div>
        <span class="visually-hidden" id="updated-label">尚未同步</span>
      </div>
      <div class="toast" id="toast" role="status"></div>
    </section>

    <section class="settings-sheet is-hidden" id="settings-sheet" aria-label="连接设置">
      <form id="connection-form" class="settings-form" autocomplete="on">
        <div class="settings-scroll">
          <header class="sheet-header">
            <div>
              <span class="eyebrow">AIRA</span>
              <h1>账号池额度</h1>
            </div>
          </header>

          <label class="field">
            <span>平台地址</span>
            <div class="input-row">
              <input id="base-url" name="url" type="url" placeholder="https://api.example.com" required />
              <button type="button" class="input-icon" id="open-site" title="打开管理后台" aria-label="打开管理后台">
                <i data-lucide="external-link"></i>
              </button>
            </div>
          </label>

          <label class="field">
            <span>管理员邮箱</span>
            <input id="email" name="username" type="email" placeholder="admin@example.com" autocomplete="username" required />
          </label>

          <label class="field login-only">
            <span>密码</span>
            <div class="input-row">
              <input id="password" name="password" type="password" autocomplete="current-password" />
              <button type="button" class="input-icon" id="password-toggle" title="显示密码" aria-label="显示密码">
                <i data-lucide="eye"></i>
              </button>
            </div>
          </label>

          <label class="field totp-field is-hidden" id="totp-field">
            <span>两步验证码</span>
            <input id="totp-code" inputmode="numeric" autocomplete="one-time-code" maxlength="6" placeholder="000000" />
          </label>

          <div class="setting-lines connected-only is-hidden">
            <div class="field model-pick-field">
              <span>展示模型</span>
              <div class="model-checks" id="model-checks" role="group" aria-label="展示模型">
                <label class="model-check">
                  <input id="show-claude" type="checkbox" checked />
                  <span><strong>Claude</strong><small>Anthropic</small></span>
                </label>
                <label class="model-check">
                  <input id="show-grok" type="checkbox" checked />
                  <span><strong>Grok</strong><small>xAI</small></span>
                </label>
                <label class="model-check">
                  <input id="show-codex" type="checkbox" checked />
                  <span><strong>Codex</strong><small>OpenAI</small></span>
                </label>
              </div>
              <small class="field-hint">默认三项全开；取消勾选后对应账号不在宠物与托盘中展示</small>
            </div>
            <label class="field setting-select-field">
              <span>展示账号数</span>
              <select id="max-display-accounts"></select>
              <small class="field-hint">宠物面板与托盘最多展示前 N 个账号</small>
            </label>
            <label class="field setting-select-field">
              <span>刷新频率</span>
              <select id="refresh-interval"></select>
              <small class="field-hint">自动同步缓存额度的时间间隔</small>
            </label>
            <label class="field setting-select-field card-opacity-field">
              <span>卡片不透明度</span>
              <div class="opacity-row">
                <input
                  id="card-opacity"
                  type="range"
                  min="0"
                  max="100"
                  step="1"
                  value="62"
                  aria-valuemin="0"
                  aria-valuemax="100"
                />
                <span class="opacity-value" id="card-opacity-label">62%</span>
              </div>
              <small class="field-hint">柱状图背景卡片：0% 全透明，100% 不透明</small>
            </label>
            <label class="switch-line">
              <span><strong>始终置顶</strong><small>宠物保持在其他窗口上方</small></span>
              <input id="always-on-top" type="checkbox" role="switch" checked />
            </label>
            <label class="switch-line">
              <span><strong>开机启动</strong><small>登录系统后自动显示宠物</small></span>
              <input id="auto-start" type="checkbox" role="switch" />
            </label>
          </div>
          <p class="pool-hint connected-only is-hidden">按勾选的模型展示账号额度。${isWindows ? '右键托盘图标可打开操作菜单（刷新 / 设置 / 管理）。' : '右键宠物可打开操作菜单（刷新 / 设置 / 管理）。'}</p>

          <section class="about-card" aria-label="关于与更新">
            <div class="about-head">
              <div class="about-title">
                <span class="about-eyebrow">关于应用</span>
                <strong>Aira</strong>
              </div>
              <div class="about-version" title="当前安装版本">
                <span class="about-version-label">当前版本</span>
                <span class="about-version-value" id="app-version">—</span>
              </div>
            </div>
            <p class="about-status" id="update-status">启动后可检查 GitHub 最新版本</p>
            <div class="about-actions">
              <button class="text-button about-update-btn" id="check-update-button" type="button" title="检查并下载更新">
                <i data-lucide="refresh-cw"></i>
                <span id="update-label">检查更新</span>
              </button>
              <button class="text-button about-download-btn is-hidden" id="download-update-button" type="button" title="打开安装包下载">
                <i data-lucide="download"></i>
                <span id="download-label">立即更新</span>
              </button>
            </div>
          </section>

          <footer class="settings-footer">
            <button class="text-button danger connected-only is-hidden" id="logout-button" type="button">
              <i data-lucide="log-out"></i><span>退出登录</span>
            </button>
            <div class="footer-actions">
              <span class="connected-only is-hidden" id="refresh-hint">自动同步</span>
            </div>
          </footer>
        </div>

        <div class="settings-actions">
          <p class="form-error" id="form-error"></p>
          <button class="primary-button login-only" id="connect-button" type="submit">连接平台</button>
          <button class="primary-button connected-only is-hidden" id="save-button" type="submit">保存设置</button>
        </div>
      </form>
    </section>
  </main>
`

function paintIcons(): void {
  createIcons({
    icons: {
      Eye,
      EyeOff,
      Download,
      ExternalLink,
      LogOut,
      RefreshCw,
    },
    attrs: { 'stroke-width': 2 },
  })
}

paintIcons()

const element = <T extends HTMLElement>(selector: string) => document.querySelector<T>(selector)!
const shell = element<HTMLElement>('#pet-shell')
const quotaDock = element<HTMLElement>('#quota-dock')
const meterCard = element<HTMLElement>('#meter-card')
const accountList = element<HTMLElement>('#account-list')

// Block native text/image selection on the pet stage (WebKit still selects otherwise).
const petStage = element<HTMLElement>('.pet-stage')
petStage.addEventListener('selectstart', (event) => event.preventDefault())
petStage.addEventListener('dragstart', (event) => event.preventDefault())
const updatedLabel = element<HTMLElement>('#updated-label')
const toast = element<HTMLElement>('#toast')
const settingsSheet = element<HTMLElement>('#settings-sheet')
const connectionForm = element<HTMLFormElement>('#connection-form')
const baseUrlInput = element<HTMLInputElement>('#base-url')
const emailInput = element<HTMLInputElement>('#email')
const passwordInput = element<HTMLInputElement>('#password')
const totpField = element<HTMLElement>('#totp-field')
const totpInput = element<HTMLInputElement>('#totp-code')
const alwaysOnTopInput = element<HTMLInputElement>('#always-on-top')
const autoStartInput = element<HTMLInputElement>('#auto-start')
const showClaudeInput = element<HTMLInputElement>('#show-claude')
const showCodexInput = element<HTMLInputElement>('#show-codex')
const showGrokInput = element<HTMLInputElement>('#show-grok')
const cardOpacityInput = element<HTMLInputElement>('#card-opacity')
const cardOpacityLabel = element<HTMLElement>('#card-opacity-label')
const maxDisplayAccountsInput = element<HTMLSelectElement>('#max-display-accounts')
const refreshIntervalInput = element<HTMLSelectElement>('#refresh-interval')
const refreshHint = element<HTMLElement>('#refresh-hint')
const formError = element<HTMLElement>('#form-error')
const connectButton = element<HTMLButtonElement>('#connect-button')
const checkUpdateButton = element<HTMLButtonElement>('#check-update-button')
const downloadUpdateButton = element<HTMLButtonElement>('#download-update-button')
const updateLabel = element<HTMLElement>('#update-label')
const downloadLabel = element<HTMLElement>('#download-label')
const updateStatus = element<HTMLElement>('#update-status')
const appVersionLabel = element<HTMLElement>('#app-version')

interface AppUpdateInfo {
  available: boolean
  current_version: string
  latest_version?: string | null
  html_url?: string | null
  download_url?: string | null
  notes?: string | null
}

let checkingUpdate = false
/** Pending GitHub release the pet bubble can open on click. */
let pendingUpdateInfo: AppUpdateInfo | null = null
let updateBubbleActive = false
let updateCheckTimer: number | undefined
let currentAppVersion = ''

function formatAppVersion(raw?: string | null): string {
  if (!raw) return '—'
  return `v${raw.replace(/^v/i, '')}`
}

function setSettingsUpdateUi(state: {
  status: string
  button?: string
  available?: boolean
  latest?: string | null
}): void {
  updateStatus.textContent = state.status
  updateStatus.classList.toggle('has-update', Boolean(state.available))
  if (state.button) updateLabel.textContent = state.button
  if (state.available && state.latest) {
    downloadUpdateButton.classList.remove('is-hidden')
    downloadLabel.textContent = `立即更新 ${formatAppVersion(state.latest)}`
  } else {
    downloadUpdateButton.classList.add('is-hidden')
  }
  paintIcons()
}

async function loadAppVersion(): Promise<void> {
  try {
    currentAppVersion = isDesktop ? await getVersion() : '0.0.0-web'
  } catch {
    currentAppVersion = ''
  }
  appVersionLabel.textContent = currentAppVersion ? formatAppVersion(currentAppVersion) : '—'
  if (!pendingUpdateInfo?.available) {
    setSettingsUpdateUi({
      status: currentAppVersion
        ? `当前 ${formatAppVersion(currentAppVersion)} · 点击下方检查更新`
        : '点击下方检查更新',
      button: '检查更新',
      available: false,
    })
  }
}

function showUpdateBubble(info: AppUpdateInfo, _force = false): void {
  if (!info.available || !info.latest_version) return
  pendingUpdateInfo = info
  const version = info.latest_version.replace(/^v/i, '')
  showToast(`发现新版本 v${version}，可在设置中更新`)
}

async function openPendingUpdate(fromSettings = false): Promise<void> {
  const info = pendingUpdateInfo
  if (!info?.available) return
  const url = info.download_url || info.html_url
  if (!url) {
    showToast('未找到下载地址', 'error')
    if (fromSettings) {
      setSettingsUpdateUi({
        status: '未找到下载地址，请前往 GitHub Releases',
        button: '检查更新',
        available: true,
        latest: info.latest_version,
      })
    }
    return
  }
  try {
    if (isDesktop) await openUrl(url)
    else window.open(url, '_blank', 'noopener')
    showToast(`正在打开 v${(info.latest_version || '').replace(/^v/i, '')} 下载…`)
    if (fromSettings) {
      setSettingsUpdateUi({
        status: `已打开 ${formatAppVersion(info.latest_version)} 下载页，安装后重启应用`,
        button: '检查更新',
        available: true,
        latest: info.latest_version,
      })
    } else {
      pendingUpdateInfo = null
    }
  } catch (error) {
    showToast(errorMessage(error), 'error')
  }
}

async function checkForUpdate(manual = false): Promise<void> {
  if (!isDesktop || checkingUpdate) return
  checkingUpdate = true
  checkUpdateButton.disabled = true
  downloadUpdateButton.disabled = true
  if (manual || settingsOpen) {
    setSettingsUpdateUi({
      status: '正在检查 GitHub 最新版本…',
      button: '检查中…',
      available: Boolean(pendingUpdateInfo?.available),
      latest: pendingUpdateInfo?.latest_version,
    })
  }
  try {
    const info = await invoke<AppUpdateInfo>('check_app_update')
    currentAppVersion = info.current_version || currentAppVersion
    appVersionLabel.textContent = formatAppVersion(currentAppVersion)
    if (info.available && info.latest_version) {
      pendingUpdateInfo = info
      setSettingsUpdateUi({
        status: `发现新版本 ${formatAppVersion(info.latest_version)}（当前 ${formatAppVersion(info.current_version)}）`,
        button: '重新检查',
        available: true,
        latest: info.latest_version,
      })
      // Always try the pet bubble (queued if settings/dock is open).
      showUpdateBubble(info, false)
    } else {
      pendingUpdateInfo = null
      setSettingsUpdateUi({
        status: manual
          ? `已是最新版本 ${formatAppVersion(info.current_version)}`
          : `当前 ${formatAppVersion(info.current_version)} · 已是最新`,
        button: '检查更新',
        available: false,
      })
      if (manual) showToast(`当前已是最新版本（${formatAppVersion(info.current_version)}）`)
    }
  } catch (error) {
    setSettingsUpdateUi({
      status: `检查失败：${errorMessage(error)}`,
      button: '检查更新',
      available: Boolean(pendingUpdateInfo?.available),
      latest: pendingUpdateInfo?.latest_version,
    })
    if (manual) showToast(`检查更新失败：${errorMessage(error)}`, 'error')
  } finally {
    checkingUpdate = false
    checkUpdateButton.disabled = false
    downloadUpdateButton.disabled = false
  }
}

function startUpdatePolling(): void {
  window.clearInterval(updateCheckTimer)
  // Quiet background check every 6 hours.
  updateCheckTimer = window.setInterval(() => void checkForUpdate(false), 6 * 60 * 60 * 1000)
}

function errorMessage(error: unknown): string {
  if (typeof error === 'string') return error
  if (error instanceof Error) return error.message
  return '操作失败，请稍后重试'
}

function clampDisplayAccounts(value: unknown): number {
  const n = Number(value)
  if (!Number.isFinite(n)) return defaultSettings.maxDisplayAccounts
  return Math.max(1, Math.min(20, Math.round(n)))
}

function clampRefreshInterval(value: unknown): number {
  const n = Number(value)
  if (!Number.isFinite(n)) return defaultSettings.refreshIntervalSec
  return Math.max(10, Math.min(3600, Math.round(n)))
}

/** Accept 0–1 or 0–100; clamp to 0–1 for storage. */
function clampCardOpacity(value: unknown): number {
  const n = Number(value)
  if (!Number.isFinite(n)) return defaultSettings.cardOpacity
  const unit = n > 1 ? n / 100 : n
  return Math.max(0, Math.min(1, Math.round(unit * 100) / 100))
}

function cardOpacityPercent(opacity: number = settings.cardOpacity): number {
  return Math.round(clampCardOpacity(opacity) * 100)
}

function applyCardOpacity(opacity: number = settings.cardOpacity): void {
  const alpha = clampCardOpacity(opacity)
  shell.style.setProperty('--meter-card-opacity', String(alpha))
  shell.style.setProperty('--meter-card-border', String(Math.min(0.45, Math.max(0.16, alpha * 0.55))))
  shell.style.setProperty('--meter-card-blur', '0px')
}

function normalizeShowModels(raw: unknown): ModelVisibility {
  const source =
    raw && typeof raw === 'object' ? (raw as Partial<Record<ModelKey, unknown>>) : {}
  return {
    claude: source.claude !== false,
    codex: source.codex !== false,
    grok: source.grok !== false,
  }
}

function normalizeSettings(raw: Partial<PetSettings> | null | undefined): PetSettings {
  return {
    ...defaultSettings,
    ...raw,
    maxDisplayAccounts: clampDisplayAccounts(raw?.maxDisplayAccounts),
    refreshIntervalSec: clampRefreshInterval(raw?.refreshIntervalSec),
    showModels: normalizeShowModels(raw?.showModels),
    cardOpacity: clampCardOpacity(raw?.cardOpacity),
  }
}

/** Map Sub2API platform string → display model key. */
function modelKeyForPlatform(platform?: string | null): ModelKey {
  const value = (platform || '').toLowerCase()
  if (value === 'anthropic' || value === 'claude') return 'claude'
  if (value === 'xai' || value === 'grok') return 'grok'
  return 'codex'
}

function isModelEnabled(platform?: string | null): boolean {
  return settings.showModels[modelKeyForPlatform(platform)]
}

function readShowModelsFromForm(): ModelVisibility {
  return {
    claude: showClaudeInput.checked,
    codex: showCodexInput.checked,
    grok: showGrokInput.checked,
  }
}

function writeShowModelsToForm(models: ModelVisibility): void {
  showClaudeInput.checked = models.claude
  showCodexInput.checked = models.codex
  showGrokInput.checked = models.grok
}

function writeCardOpacityToForm(opacity: number = settings.cardOpacity): void {
  const percent = cardOpacityPercent(opacity)
  cardOpacityInput.value = String(percent)
  cardOpacityLabel.textContent = `${percent}%`
}

function populateSettingSelects(): void {
  maxDisplayAccountsInput.replaceChildren()
  for (const count of DISPLAY_COUNT_OPTIONS) {
    const option = document.createElement('option')
    option.value = String(count)
    option.textContent = `${count} 个`
    maxDisplayAccountsInput.append(option)
  }

  refreshIntervalInput.replaceChildren()
  for (const item of REFRESH_INTERVAL_OPTIONS) {
    const option = document.createElement('option')
    option.value = String(item.value)
    option.textContent = item.label
    refreshIntervalInput.append(option)
  }
}

function refreshHintText(): string {
  const sec = settings.refreshIntervalSec
  if (sec < 60) return `每 ${sec} 秒自动同步`
  if (sec % 60 === 0) return `每 ${sec / 60} 分钟自动同步`
  return `每 ${sec} 秒自动同步`
}

function platformSortRank(platform?: string | null): number {
  const model = modelKeyForPlatform(platform)
  if (model === 'claude') return 0
  if (model === 'grok') return 1
  return 2
}

function visiblePoolRows(): AccountQuotaRow[] {
  const limit = clampDisplayAccounts(settings.maxDisplayAccounts)
  return poolRows
    .filter((row) => isModelEnabled(row.platform))
    .slice()
    .sort((a, b) => {
      const activeDiff = (a.status === 'active' ? 0 : 1) - (b.status === 'active' ? 0 : 1)
      if (activeDiff !== 0) return activeDiff
      const rankDiff = platformSortRank(a.platform) - platformSortRank(b.platform)
      if (rankDiff !== 0) return rankDiff
      return a.name.localeCompare(b.name)
    })
    .slice(0, limit)
}

populateSettingSelects()

async function loadSettings(): Promise<void> {
  if (!isDesktop) {
    const raw = localStorage.getItem('sub2api-pet-settings')
    settings = normalizeSettings(raw ? JSON.parse(raw) : null)
    applyCardOpacity()
    return
  }
  if (!appStore) {
    appStore = await load('settings.json', { autoSave: true })
  } else {
    // Settings window may have written to disk; refresh in-memory cache.
    try {
      await appStore.reload()
    } catch {
      // reload is best-effort; fall through to get()
    }
  }
  settings = normalizeSettings(await appStore.get<PetSettings>('connection'))
  settings.autoStart = await isEnabled().catch(() => settings.autoStart)
  applyCardOpacity()
}

async function saveSettings(): Promise<void> {
  if (!isDesktop) {
    localStorage.setItem('sub2api-pet-settings', JSON.stringify(settings))
    return
  }
  if (!appStore) appStore = await load('settings.json', { autoSave: true })
  await appStore.set('connection', settings)
  await appStore.save()
}

async function registerPositionPersistence(): Promise<void> {
  if (!isDesktop) return
  await anchorWindowToRight()
}

function formatClock(value: string): string {
  const date = new Date(value)
  if (Number.isNaN(date.getTime())) return '--'
  return date.toLocaleTimeString('zh-CN', { hour: '2-digit', minute: '2-digit' })
}

function formatReset(value?: string): string {
  if (!value) return '重置时间未知'
  const date = new Date(value)
  if (Number.isNaN(date.getTime())) return '重置时间未知'
  const weekday = new Intl.DateTimeFormat('zh-CN', { weekday: 'short' }).format(date)
  const time = date.toLocaleTimeString('zh-CN', { hour: '2-digit', minute: '2-digit' })
  return `${weekday} ${time} 重置`
}

function rowWindows(row: AccountQuotaRow): QuotaWindow[] {
  if (row.windows?.length) return row.windows
  if (row.remaining_percent == null) return []
  return [
    {
      label: row.platform === 'anthropic' ? '7d' : '7d',
      used_percent: Math.max(0, 100 - row.remaining_percent),
      remaining_percent: row.remaining_percent,
      reset_at: null,
    },
  ]
}

function lowestRemaining(row: AccountQuotaRow): number | null {
  const windows = rowWindows(row)
  if (!windows.length) return row.remaining_percent ?? null
  return windows.reduce((min, window) => Math.min(min, window.remaining_percent), 100)
}

function showToast(message: string, kind: 'normal' | 'error' = 'normal'): void {
  toast.textContent = message
  toast.className = `toast is-visible ${kind === 'error' ? 'is-error' : ''}`
  window.setTimeout(() => toast.classList.remove('is-visible'), 2600)
}

function mockPoolRows(): AccountQuotaRow[] {
  return [
    {
      id: 21,
      name: 'Claude 主账号',
      status: 'active',
      plan: 'max',
      platform: 'anthropic',
      account_type: 'oauth',
      remaining_percent: 68,
      windows: [
        {
          label: '5h',
          used_percent: 32,
          remaining_percent: 68,
          reset_at: new Date(Date.now() + 2 * 3600000).toISOString(),
        },
        {
          label: '7d',
          used_percent: 18,
          remaining_percent: 82,
          reset_at: new Date(Date.now() + 4.1 * 86400000).toISOString(),
        },
      ],
      updated_at: new Date().toISOString(),
      source: 'cached',
    },
    {
      id: 31,
      name: 'Grok 主账号',
      status: 'active',
      plan: 'super',
      platform: 'xai',
      account_type: 'api_key',
      remaining_percent: 72,
      windows: [
        {
          label: '7d',
          used_percent: 28,
          remaining_percent: 72,
          reset_at: new Date(Date.now() + 2.5 * 86400000).toISOString(),
        },
      ],
      updated_at: new Date().toISOString(),
      source: 'cached',
    },
    {
      id: 7,
      name: 'Codex 主账号',
      status: 'active',
      plan: 'team',
      platform: 'openai',
      account_type: 'oauth',
      remaining_percent: 60,
      windows: [
        {
          label: '5h',
          used_percent: 40,
          remaining_percent: 60,
          reset_at: new Date(Date.now() + 1.8 * 3600000).toISOString(),
        },
        {
          label: '7d',
          used_percent: 25,
          remaining_percent: 75,
          reset_at: new Date(Date.now() + 3.4 * 86400000).toISOString(),
        },
      ],
      updated_at: new Date().toISOString(),
      source: 'cached',
    },
  ]
}

function latestUpdatedAt(): string | null {
  let latest: number | null = null
  let latestIso: string | null = null
  for (const row of poolRows) {
    if (!row.updated_at) continue
    const time = new Date(row.updated_at).getTime()
    if (Number.isNaN(time)) continue
    if (latest === null || time > latest) {
      latest = time
      latestIso = row.updated_at
    }
  }
  return latestIso
}

interface ModelIconInfo {
  path: string
  viewBox: string
}

const MODEL_ICON_PATHS: Record<ModelKey, ModelIconInfo> = {
  grok: {
    path: 'M13.2371 21.0407L24.3186 12.8506C24.8619 12.4491 25.6384 12.6057 25.8973 13.2294C27.2597 16.5185 26.651 20.4712 23.9403 23.1851C21.2297 25.8989 17.4581 26.4941 14.0108 25.1386L10.2449 26.8843C15.6463 30.5806 22.2053 29.6665 26.304 25.5601C29.5551 22.3051 30.562 17.8683 29.6205 13.8673L29.629 13.8758C28.2637 7.99809 29.9647 5.64871 33.449 0.844576C33.5314 0.730667 33.6139 0.616757 33.6964 0.5L29.1113 5.09055V5.07631L13.2343 21.0436M10.9503 23.0313C7.07343 19.3235 7.74185 13.5853 11.0498 10.2763C13.4959 7.82722 17.5036 6.82767 21.0021 8.2971L24.7595 6.55998C24.0826 6.07017 23.215 5.54334 22.2195 5.17313C17.7198 3.31926 12.3326 4.24192 8.67479 7.90126C5.15635 11.4239 4.0499 16.8403 5.94992 21.4622C7.36924 24.9165 5.04257 27.3598 2.69884 29.826C1.86829 30.7002 1.0349 31.5745 0.36364 32.5L10.9474 23.0341',
    viewBox: '0 0 34 33',
  },
  codex: {
    path: 'M904.533333 435.285333c-2.730667-3.328-3.456-5.973333-2.218666-9.898666 10.197333-32.426667 13.013333-65.365333 7.466666-99.029334a220.501333 220.501333 0 0 0-83.413333-142.293333c-53.632-42.197333-115.029333-57.258667-182.741333-46.122667-6.4 1.024-10.24-0.725333-14.72-5.12-57.685333-57.301333-127.872-79.402667-207.573334-64.085333-84.522667 16.085333-142.506667 66.304-173.056 146.304-1.450667 3.669333-3.029333 5.674667-7.552 6.741333-52.309333 12.373333-95.701333 38.912-128.341333 81.194667-41.557333 54.058667-56.746667 114.773333-44.032 181.845333a216.618667 216.618667 0 0 0 50.346667 102.912c3.541333 4.096 4.138667 7.594667 2.56 12.501334-6.997333 20.565333-9.216 41.941333-9.770667 63.872 0.64 15.957333 1.706667 32.298667 5.546667 48.213333 27.178667 119.04 144.768 195.584 266.282666 173.269333 4.864-0.768 7.168 0.256 10.197334 3.413334 57.941333 58.837333 128.768 81.834667 209.706666 66.261333 84.608-16.213333 142.08-67.029333 172.885334-146.773333 1.408-3.584 2.986667-5.248 6.997333-6.186667 96.426667-21.973333 167.509333-102.826667 175.829333-200.106667 5.845333-62.592-12.757333-118.698667-54.4-166.912z m-55.210666-110.421333c3.882667 18.901333 5.12 37.802667 2.730666 56.96-0.256 2.176-0.981333 4.266667-1.578666 7.253333l-49.621334-28.245333c-43.52-24.874667-87.210667-49.493333-130.56-74.752a37.461333 37.461333 0 0 0-41.386666 0.085333c-66.304 38.357333-132.949333 75.946667-199.424 113.877334-2.133333 1.109333-3.882667 3.029333-7.253334 2.773333V318.037333c0-3.157333 2.048-4.138667 4.181334-5.418666 59.178667-33.706667 117.845333-68.437333 177.664-100.821334 97.493333-52.693333 222.976 5.76 245.248 113.066667z m-247.808 186.368c0 15.488-0.085333 30.890667 0.085333 46.293333 0 3.584-0.981333 5.717333-4.352 7.552-27.178667 15.317333-54.186667 30.805333-81.152 46.464-2.986667 1.664-5.12 1.834667-8.277333 0.085334-26.88-15.573333-54.016-31.061333-81.152-46.378667-3.541333-2.005333-4.608-4.266667-4.608-8.234667 0.170667-30.122667 0.170667-60.16 0-90.24 0-4.181333 1.237333-6.528 5.034666-8.746666 26.496-14.933333 52.906667-29.994667 79.232-45.226667 3.925333-2.176 6.741333-2.56 10.922667-0.170667 26.325333 15.317333 52.650667 30.378667 79.146667 45.312 3.712 2.133333 5.205333 4.394667 5.205333 8.661334-0.256 14.805333-0.085333 29.781333-0.085333 44.629333zM293.802667 294.4c0.085333-84.608 54.784-152.618667 138.709333-169.258667 51.584-10.026667 98.730667 2.986667 141.354667 36.053334l-69.12 39.253333c-38.314667 21.76-76.586667 43.733333-115.029334 65.322667a32 32 0 0 0-17.578666 30.464v236.970666c-2.645333 0.725333-4.138667-1.237333-5.930667-2.176-22.186667-12.501333-44.202667-25.386667-66.474667-37.632-4.608-2.56-5.930667-5.546667-5.930666-10.496 0.085333-62.848 0-125.653333 0-188.501333z m-169.514667 163.882667c-8.362667-72.96 37.290667-147.2 106.666667-172.8 1.152-0.341333 2.304-0.64 3.925333-0.981334V336.213333c0 51.626667 0.170667 103.253333 0 154.88-0.170667 15.146667 5.930667 25.6 19.413333 33.194667 67.072 37.717333 133.973333 76.032 200.832 114.090667l6.442667 3.84-74.24 42.453333c-2.56 1.493333-4.522667 2.133333-7.466667 0.341333-59.989333-34.389333-120.789333-67.2-179.626666-103.168-45.653333-27.818667-70.016-70.613333-75.946667-123.562666z m74.965333 297.898666a166.229333 166.229333 0 0 1-25.344-121.130666l11.776 6.4c56.917333 32.469333 113.92 64.938667 170.794667 97.578666a33.962667 33.962667 0 0 0 36.693333 0c67.797333-38.784 135.68-77.44 203.477334-116.053333l5.034666-2.773333c0 29.141333 0 57.130667 0.170667 85.12 0 3.413333-1.664 4.821333-4.138667 6.229333-58.581333 33.152-116.565333 67.413333-175.829333 99.413333-77.397333 41.472-173.994667 17.664-222.634667-54.784z m530.346667-12.074666c-1.706667 60.458667-41.045333 120.149333-107.776 146.346666a172.373333 172.373333 0 0 1-170.666667-28.032l80.426667-45.781333c34.218667-19.498667 68.352-39.210667 102.741333-58.368a31.274667 31.274667 0 0 0 17.536-30.165333c-0.256-76.672-0.085333-153.344-0.085333-230.144 0-8.405333 0-8.405333 7.168-4.48 22.058667 12.586667 44.16 25.301333 66.304 37.717333 3.541333 2.005333 4.949333 4.010667 4.864 8.106667-0.085333 68.181333 1.322667 136.533333-0.512 204.8z m68.266667-7.68c-8.832 3.754667-8.832 3.754667-8.832-5.632 0-66.56-0.256-133.162667 0.213333-199.68a32.426667 32.426667 0 0 0-18.261333-31.317334c-66.218667-37.461333-132.266667-75.264-198.357334-112.896l-10.112-5.888 75.178667-42.752c2.645333-1.578667 4.522667-0.725333 6.826667 0.512 59.349333 33.962667 119.381333 66.688 177.834666 101.76 44.672 26.965333 69.973333 67.84 76.928 119.168A167.125333 167.125333 0 0 1 797.866667 736.426667z',
    viewBox: '0 0 1024 1024',
  },
  claude: {
    path: 'M252.8 652.8l167.89504-94.29504 2.76992-8.10496-2.76992-4.48h-8.11008l-28.16-1.70496-96-2.56-83.2-3.41504-80.64-4.26496-20.26496-4.27008-18.98496-24.96 1.92-12.58496 17.06496-11.52 24.32 2.13504L182.61504 486.4 263.68 491.94496l58.66496 3.41504 87.04 9.17504h13.87008l1.92-5.55008-4.69504-3.40992-3.62496-3.41504-83.84-56.74496-90.67008-60.16-47.56992-34.56L168.96 323.2l-13.01504-16.42496-5.54496-35.84 23.25504-25.81504 31.36 2.13504 7.88992 2.12992 31.79008 24.32 67.84 52.48 88.52992 65.28 13.01504 10.88 5.12-3.62496 0.64-2.56-5.76-9.81504-48.21504-87.04-51.40992-88.52992L291.62496 174.08l-5.96992-21.97504a107.85792 107.85792 0 0 1-3.63008-26.02496l26.67008-36.05504 14.72-4.68992 35.40992 4.68992L373.76 103.04l21.97504 50.34496 35.62496 79.36L486.61504 340.48l16.20992 32 8.75008 29.65504 3.2 9.16992h5.54496v-5.12l4.48-60.8 8.32-74.44992 8.10496-96 2.77504-27.09504 13.44-32.42496 26.66496-17.49504 20.69504 10.02496 17.06496 24.32-2.34496 15.79008-10.24 65.92-19.84 103.24992-13.01504 69.12h7.47008l8.74496-8.74496 34.98496-46.50496 58.67008-73.39008 26.02496-29.22496 30.29504-32.21504 19.40992-15.36H798.72l27.09504 40.11008-12.16 41.38496-37.76 48-31.36 40.53504-45.01504 60.58496-28.16 48.42496 2.56 3.84 6.61504-0.64 101.54496-21.54496 54.82496-10.02496 65.49504-11.31008 29.65504 13.87008 3.2 14.08-11.73504 28.8-69.97504 17.28-82.12992 16.42496-122.24 29.01504-1.49504 1.06496 1.70496 2.13504 55.04 5.12 23.47008 1.28h57.6l107.30496 7.88992 28.16 18.56 16.85504 22.61504-2.77504 17.28-43.30496 21.97504-58.24-13.87008-136.11008-32.42496-46.72-11.73504h-6.4v3.84l38.83008 37.97504 71.24992 64.42496 89.17504 82.99008 4.48 20.48-11.52 16.20992L824.32 803.84l-78.50496-58.88-30.29504-26.66496-68.48-57.6h-4.48v5.96992l15.78496 23.04 83.41504 125.23008 4.26496 38.4-5.96992 12.58496-21.55008 7.46496-23.68-4.26496-48.84992-68.48-50.35008-77.22496-40.52992-69.12-4.91008 2.76992-23.88992 258.13504-11.31008 13.22496-26.02496 10.03008-21.54496-16.43008-11.52-26.66496 11.52-52.48L481.28 774.4l11.30496-54.4 10.24-67.62496 5.97504-22.4-0.42496-1.49504-4.91008 0.64-50.98496 69.97504L374.82496 803.84l-61.44 65.70496-14.72 5.76-25.38496-13.22496 2.34496-23.46496 14.29504-20.91008 84.90496-107.94496 51.2-66.98496L459.09504 604.16v-5.54496h-2.13504l-225.49504 146.56-40.10496 5.12L174.08 734.08l2.13504-26.66496L184.32 698.66496l67.84-46.72h-0.21504l0.85504 0.85504z',
    viewBox: '0 0 1024 1024',
  },
}

function platformIconSvg(platform?: string): string {
  const model = modelKeyForPlatform(platform)
  const info = MODEL_ICON_PATHS[model]
  return `<svg class="model-mark" viewBox="${info.viewBox}" aria-hidden="true" focusable="false" xmlns="http://www.w3.org/2000/svg"><path d="${info.path}" fill="currentColor"/></svg>`
}

function createRingBadge(row: AccountQuotaRow): HTMLElement {
  const model = modelKeyForPlatform(row.platform)
  const isInactive = row.status !== 'active'
  const windows = rowWindows(row)

  const window5h = windows.find((w) => w.label.toLowerCase() === '5h' || w.label.toLowerCase().includes('5h'))
  const window7d = windows.find((w) => w.label.toLowerCase() === '7d' || w.label.toLowerCase().includes('7d'))

  // Split ring: left semi-circle = 5h quota, right semi-circle = 7d quota
  // Grok only has weekly quota, so Grok always uses a full single circle
  const isSplit = model !== 'grok' && Boolean(window5h && window7d)

  const badge = document.createElement('div')
  badge.className = `ring-badge model-${model}${isInactive ? ' is-inactive' : ''}`
  badge.setAttribute('role', 'listitem')
  badge.dataset.name = row.name

  let ringsHtml = ''
  let hoverQuotaHtml = ''

  if (isSplit) {
    const remaining5h = window5h && Number.isFinite(window5h.remaining_percent)
      ? Math.max(0, Math.min(100, window5h.remaining_percent))
      : (row.remaining_percent ?? 0)

    const remaining7d = window7d && Number.isFinite(window7d.remaining_percent)
      ? Math.max(0, Math.min(100, window7d.remaining_percent))
      : (row.remaining_percent ?? 0)

    const is5hLow = remaining5h > 0 && remaining5h <= 15
    const is7dLow = remaining7d > 0 && remaining7d <= 15

    const ARC_LEN = 49.40
    const offset5h = remaining5h <= 0 ? ARC_LEN : ARC_LEN * (1 - remaining5h / 100)
    const offset7d = remaining7d <= 0 ? ARC_LEN : ARC_LEN * (1 - remaining7d / 100)

    // Tooltip content
    const tooltipLines = [
      `${row.name}${row.plan ? ` (${row.plan})` : ''}`,
      `左半圈 (5小时额度): 剩余 ${Math.round(remaining5h)}% · ${formatReset(window5h?.reset_at ?? undefined)}`,
      `右半圈 (周额度): 剩余 ${Math.round(remaining7d)}% · ${formatReset(window7d?.reset_at ?? undefined)}`,
    ]
    badge.title = tooltipLines.join('\n')

    const tone5h = is5hLow ? 'is-low' : 'tone-quota-5h'
    const tone7d = is7dLow ? 'is-low' : 'tone-quota-7d'

    ringsHtml = `
      <svg class="ring-svg" viewBox="0 0 40 40" aria-hidden="true" focusable="false">
        <!-- Tracks -->
        <path d="M 18.00 3.12 A 17 17 0 0 0 18.00 36.88" class="ring-track left-track" />
        <path d="M 22.00 3.12 A 17 17 0 0 1 22.00 36.88" class="ring-track right-track" />

        <!-- Left Semi-Circle: 5-Hour Session Quota (#4FA3FF) -->
        <path
          d="M 18.00 3.12 A 17 17 0 0 0 18.00 36.88"
          class="ring-progress left-semi ${tone5h}"
          stroke-dasharray="${ARC_LEN}"
          stroke-dashoffset="${offset5h.toFixed(2)}"
          ${remaining5h <= 0 ? 'style="opacity: 0;"' : ''}
        />

        <!-- Right Semi-Circle: 7-Day Weekly Quota (#4FA3FF) -->
        <path
          d="M 22.00 3.12 A 17 17 0 0 1 22.00 36.88"
          class="ring-progress right-semi ${tone7d}"
          stroke-dasharray="${ARC_LEN}"
          stroke-dashoffset="${offset7d.toFixed(2)}"
          ${remaining7d <= 0 ? 'style="opacity: 0;"' : ''}
        />
      </svg>
    `

    hoverQuotaHtml = `
      <div class="ring-hover-values" aria-hidden="true">
        <span class="ring-hover-val val-5h ${is5hLow ? 'is-low' : ''}">${Math.round(remaining5h)}%</span>
        <span class="ring-hover-val val-7d ${is7dLow ? 'is-low' : ''}">${Math.round(remaining7d)}%</span>
      </div>
    `
  } else {
    // Single full circle (e.g. Grok, or accounts with a single quota window)
    const singleWindow = window7d || window5h || windows[0]
    const remaining = singleWindow && Number.isFinite(singleWindow.remaining_percent)
      ? Math.max(0, Math.min(100, singleWindow.remaining_percent))
      : (row.remaining_percent ?? 0)

    const isLow = remaining > 0 && remaining <= 15
    const C_FULL = 106.81
    const offset = remaining <= 0 ? C_FULL : C_FULL * (1 - remaining / 100)

    const label = model === 'grok' ? '周额度' : (singleWindow?.label || '额度')
    const tooltipLines = [
      `${row.name}${row.plan ? ` (${row.plan})` : ''}`,
      `${label}: 剩余 ${Math.round(remaining)}% · ${formatReset(singleWindow?.reset_at ?? undefined)}`,
    ]
    badge.title = tooltipLines.join('\n')

    const tone = isLow ? 'is-low' : 'tone-quota-7d'

    ringsHtml = `
      <svg class="ring-svg" viewBox="0 0 40 40" aria-hidden="true" focusable="false">
        <!-- Full Circle Track -->
        <circle cx="20" cy="20" r="17" class="ring-track full-track" />

        <!-- Full Circle Progress (#4FA3FF) -->
        <circle
          cx="20"
          cy="20"
          r="17"
          class="ring-progress full-circle ${tone}"
          stroke-dasharray="${C_FULL}"
          stroke-dashoffset="${offset.toFixed(2)}"
          ${remaining <= 0 ? 'style="opacity: 0;"' : ''}
        />
      </svg>
    `

    hoverQuotaHtml = `
      <div class="ring-hover-values is-single" aria-hidden="true">
        <span class="ring-hover-val val-7d ${isLow ? 'is-low' : ''}">${Math.round(remaining)}%</span>
      </div>
    `
  }

  // Center logo (26px)
  const iconInfo = MODEL_ICON_PATHS[model]
  const centerIconHtml = `
    <div class="ring-center-icon ${model}" aria-hidden="true">
      <svg viewBox="${iconInfo.viewBox}" class="model-icon-svg" focusable="false">
        <path d="${iconInfo.path}" fill="currentColor" />
      </svg>
    </div>
  `

  badge.innerHTML = ringsHtml + centerIconHtml + hoverQuotaHtml
  return badge
}

function renderPool(): void {
  accountList.replaceChildren()

  if (!connected) {
    updatedLabel.textContent = '未连接'
    quotaDock.classList.remove('is-low', 'has-data')
    const empty = document.createElement('div')
    empty.className = 'meter-empty is-clickable'
    empty.innerHTML = '<span>未连接</span><span class="meter-empty-sub">点击登录</span>'
    empty.addEventListener('click', (e) => {
      e.stopPropagation()
      void setSettingsOpen(true)
    })
    accountList.append(empty)
    void syncTrayMenu()
    return
  }

  if (!poolRows.length) {
    updatedLabel.textContent = '暂无数据'
    quotaDock.classList.remove('is-low')
    quotaDock.classList.add('has-data')
    const empty = document.createElement('div')
    empty.className = 'meter-empty is-clickable'
    empty.innerHTML = '<span>暂无数据</span><span class="meter-empty-sub">点击同步</span>'
    empty.addEventListener('click', (e) => {
      e.stopPropagation()
      void setSettingsOpen(true)
    })
    accountList.append(empty)
    void syncTrayMenu()
    return
  }

  const displayRows = visiblePoolRows()
  if (!displayRows.length) {
    updatedLabel.textContent = '当前模型均未勾选或无匹配账号'
    quotaDock.classList.remove('is-low')
    quotaDock.classList.add('has-data')
    const empty = document.createElement('div')
    empty.className = 'meter-empty is-clickable'
    empty.innerHTML = settings.showModels.claude || settings.showModels.codex || settings.showModels.grok
      ? '<span>无账号</span><span class="meter-empty-sub">点击设置</span>'
      : '<span>请勾选模型</span><span class="meter-empty-sub">点击设置</span>'
    empty.addEventListener('click', (e) => {
      e.stopPropagation()
      void setSettingsOpen(true)
    })
    accountList.append(empty)
    void syncTrayMenu()
    return
  }

  const lowCount = displayRows.filter((row) => {
    if (row.status !== 'active') return false
    const remaining = lowestRemaining(row)
    return remaining != null && remaining <= 15
  }).length

  quotaDock.classList.add('has-data')
  quotaDock.classList.toggle('is-low', lowCount > 0)

  const latest = latestUpdatedAt()
  const anyCached = poolRows.some((row) => row.source === 'cached')
  updatedLabel.textContent = latest
    ? `${formatClock(latest)} 更新${anyCached ? ' · 缓存' : ''}`
    : '已同步'

  for (const row of displayRows) {
    accountList.append(createRingBadge(row))
  }

  const size = widgetWindowSize()
  updateDockBackdrop(size.width, size.height)

  void syncTrayMenu()
}

async function syncTrayMenu(options?: { refreshing?: boolean }): Promise<void> {
  if (!isDesktop) return
  try {
    await invoke('update_tray_menu', {
      payload: {
        accounts: visiblePoolRows().map((row) => {
          const windows = rowWindows(row)
          // Keep Claude order: 5h then 7d so both session and weekly limits appear.
          const ordered =
            row.platform === 'anthropic'
              ? [
                  ...windows.filter((window) => window.label === '5h'),
                  ...windows.filter((window) => window.label === '7d'),
                  ...windows.filter((window) => window.label !== '5h' && window.label !== '7d'),
                ]
              : windows
          return {
            id: row.id,
            name: row.name,
            platform: row.platform || 'openai',
            status: row.status,
            windows: ordered.map((window) => ({
              label: window.label,
              remaining_percent: window.remaining_percent,
              reset_at: window.reset_at ?? null,
            })),
          }
        }),
        synced_at: latestUpdatedAt(),
        refreshing: options?.refreshing ?? refreshing,
      },
    })
  } catch {
    // Tray updates are best-effort and should not interrupt the UI.
  }
}

async function refreshQuota(force: boolean): Promise<void> {
  if (refreshing || !connected) return
  refreshing = true
  // Tooltip-only "syncing" indicator — backend skips full tray menu rebuild for this flag.
  void syncTrayMenu({ refreshing: true })
  quotaDock.classList.add('is-refreshing')
  meterCard.classList.add('is-refreshing')
  try {
    poolRows = isDesktop
      ? await invoke<AccountQuotaRow[]>('refresh_pool_quotas', { force })
      : mockPoolRows().map((row) => ({
          ...row,
          source: force ? 'active' : 'cached',
          updated_at: new Date().toISOString(),
        }))
    renderPool()
    if (!settingsOpen) await applyWindowSize()
    if (force) {
      showToast(`已更新 ${poolRows.length} 个账号额度`)
    }
  } catch (error) {
    showToast(errorMessage(error), 'error')
  } finally {
    refreshing = false
    quotaDock.classList.remove('is-refreshing')
    meterCard.classList.remove('is-refreshing')
    // Single menu rebuild after data lands (fingerprint skips no-op rebuilds).
    void syncTrayMenu({ refreshing: false })
  }
}

function startAutoRefresh(): void {
  window.clearInterval(autoRefreshTimer)
  const intervalMs = clampRefreshInterval(settings.refreshIntervalSec) * 1000
  autoRefreshTimer = window.setInterval(() => void refreshQuota(false), intervalMs)
}

function generateDockPath(w: number, h: number): string {
  // Let each shoulder taper smoothly into the screen edge. The vertical
  // tangent at either end avoids a hard corner; the lower half mirrors it.
  // The screen-side edge runs straight just past the window so the fill covers
  // the last pixel column and its stroke is clipped off-screen.
  const x = (value: number) => Number(((value / 52) * w).toFixed(2))
  const outside = w + 2
  const bottom = (value: number) => h - value

  return [
    `M ${w} 0`,
    `C ${w} 12, ${x(39)} 22, ${x(25)} 22`,
    `C ${x(13)} 22, ${x(3)} 28, ${x(1)} 38`,
    `C 0 43, 0 48, 0 52`,
    `L 0 ${bottom(52)}`,
    `C 0 ${bottom(48)}, 0 ${bottom(43)}, ${x(1)} ${bottom(38)}`,
    `C ${x(3)} ${bottom(28)}, ${x(13)} ${bottom(22)}, ${x(25)} ${bottom(22)}`,
    `C ${x(39)} ${bottom(22)}, ${w} ${bottom(12)}, ${w} ${h}`,
    `L ${outside} ${h}`,
    `L ${outside} 0`,
    `Z`,
  ].join(' ')
}

function updateDockBackdrop(w: number, h: number): void {
  const d = generateDockPath(w, h)
  const pathEl = document.getElementById('dock-backdrop-path')
  const svgEl = document.getElementById('dock-backdrop-svg')
  if (pathEl) {
    pathEl.setAttribute('d', d)
  }
  if (svgEl) {
    svgEl.setAttribute('viewBox', `0 0 ${w} ${h}`)
    svgEl.setAttribute('width', String(w))
    svgEl.setAttribute('height', String(h))
  }
  meterCard.style.clipPath = `path('${d}')`
}

function widgetWindowSize(): { width: number; height: number } {
  const width = 52
  const rows = visiblePoolRows()
  if (!rows.length) {
    return { width, height: 120 }
  }
  const count = rows.length
  // 44px top flare/padding + count * 40px badge + (count - 1) * 5px gap + 44px bottom flare/padding
  const height = 88 + count * 40 + (count - 1) * 5
  return { width, height }
}

async function anchorWindowToRight(width?: number, height?: number): Promise<void> {
  if (!isDesktop) return
  try {
    const win = getCurrentWindow()
    await win.setShadow(false).catch(() => {})
    const monitors = await availableMonitors()
    const monitor = monitors.find((item) =>
      item.position.x === settings.windowMonitor?.x && item.position.y === settings.windowMonitor?.y,
    ) ?? await currentMonitor()
    if (!monitor) {
      await invoke('anchor_main_window_right', { y: settings.windowY ?? null })
      return
    }
    const workArea = monitor.workArea || {
      position: monitor.position,
      size: monitor.size,
    }
    const size = widgetWindowSize()
    const scale = monitor.scaleFactor || 1
    const winW = Math.round((width ?? size.width) * scale)
    const winH = Math.round((height ?? size.height) * scale)
    const targetX = workArea.position.x + workArea.size.width - winW
    const maxY = Math.max(workArea.position.y, workArea.position.y + workArea.size.height - winH)
    let targetY: number
    if (typeof settings.windowY === 'number' && Number.isFinite(settings.windowY)) {
      // Older settings stored logical Y. New positions use physical pixels.
      const savedY = settings.windowMonitor ? settings.windowY : settings.windowY * scale
      targetY = Math.round(Math.max(workArea.position.y, Math.min(maxY, savedY)))
    } else {
      targetY = Math.round(workArea.position.y + (maxY - workArea.position.y) / 2)
    }
    await win.setPosition(new PhysicalPosition(targetX, targetY))
    const actualSize = await win.outerSize()
    if (actualSize.width !== winW || actualSize.height !== winH) {
      const actualMaxY = Math.max(workArea.position.y, workArea.position.y + workArea.size.height - actualSize.height)
      await win.setPosition(new PhysicalPosition(
        workArea.position.x + workArea.size.width - actualSize.width,
        Math.max(workArea.position.y, Math.min(actualMaxY, targetY)),
      ))
    }
  } catch (err) {
    console.error('Failed to anchor window to right:', err)
  }
}

async function applyWindowSize(): Promise<void> {
  if (!isDesktop || dragActive) return
  const win = getCurrentWindow()
  const size = widgetWindowSize()
  updateDockBackdrop(size.width, size.height)
  await win.setSize(new LogicalSize(size.width, size.height))
  await win.setShadow(false).catch(() => {})
  await anchorWindowToRight(size.width, size.height)
}

/** Fill the in-page settings form (browser fallback only). */
function fillInPageSettingsForm(): void {
  baseUrlInput.value = settings.baseUrl
  emailInput.value = settings.email
  alwaysOnTopInput.checked = settings.alwaysOnTop
  autoStartInput.checked = settings.autoStart
  writeShowModelsToForm(settings.showModels)
  writeCardOpacityToForm(settings.cardOpacity)
  maxDisplayAccountsInput.value = String(clampDisplayAccounts(settings.maxDisplayAccounts))
  refreshIntervalInput.value = String(clampRefreshInterval(settings.refreshIntervalSec))
  if (![...refreshIntervalInput.options].some((option) => option.value === refreshIntervalInput.value)) {
    const option = document.createElement('option')
    option.value = refreshIntervalInput.value
    option.textContent = `${settings.refreshIntervalSec} 秒`
    refreshIntervalInput.append(option)
  }
  if (![...maxDisplayAccountsInput.options].some((option) => option.value === maxDisplayAccountsInput.value)) {
    const option = document.createElement('option')
    option.value = maxDisplayAccountsInput.value
    option.textContent = `${settings.maxDisplayAccounts} 个`
    maxDisplayAccountsInput.append(option)
  }
  refreshHint.textContent = refreshHintText()
  formError.textContent = ''
  document.querySelectorAll('.connected-only').forEach((item) => item.classList.toggle('is-hidden', !connected))
  document.querySelectorAll('.login-only').forEach((item) => item.classList.toggle('is-hidden', connected))
}

/**
 * Open / close settings.
 * Desktop: independent decorated window — pet stays visible in place.
 * Browser: in-page sheet overlay (pet stage remains visible underneath).
 */
async function setSettingsOpen(open: boolean): Promise<void> {
  settingsOpen = open

  if (!isDesktop) {
    settingsSheet.classList.toggle('is-hidden', !open)
    shell.classList.toggle('has-settings', open)
    shell.classList.remove('pet-hidden')
    if (open) {
      fillInPageSettingsForm()
      if (!connected) window.setTimeout(() => baseUrlInput.focus(), 120)
    }
    return
  }

  if (open) {
    try {
      await invoke('show_settings_window')
    } catch (error) {
      showToast(errorMessage(error), 'error')
      settingsOpen = false
    }
  } else {
    try {
      await invoke('hide_settings_window')
    } catch {
      // already closed
    }
  }
}

/** Apply changes from the independent settings window. */
async function onSettingsChanged(kind: string, payload?: unknown): Promise<void> {
  if (kind === 'preview-opacity') {
    applyCardOpacity(Number(payload))
    return
  }

  // Prefer payload from settings window (avoids store cache races across webviews).
  if (payload && typeof payload === 'object') {
    settings = normalizeSettings(payload as Partial<PetSettings>)
    applyCardOpacity()
  } else {
    await loadSettings()
  }

  if (kind === 'login') {
    connected = true
    settingsOpen = false
    renderPool()
    await refreshQuota(true)
    startAutoRefresh()
    await applyWindowSize()
    showToast('Sub2API 连接成功')
    return
  }

  if (kind === 'logout') {
    connected = false
    poolRows = []
    window.clearInterval(autoRefreshTimer)
    renderPool()
    await applyWindowSize()
    return
  }

  if (kind === 'save') {
    settingsOpen = false
    if (isDesktop) {
      try {
        await getCurrentWindow().setAlwaysOnTop(settings.alwaysOnTop)
      } catch {
        // ignore
      }
    }
    renderPool()
    startAutoRefresh()
    await applyWindowSize()
    if (connected) await refreshQuota(false)
  }
}

async function connect(): Promise<void> {
  const baseUrl = baseUrlInput.value.trim()
  const email = emailInput.value.trim()
  const password = passwordInput.value
  formError.textContent = ''
  connectButton.disabled = true
  connectButton.textContent = tempToken ? '验证中…' : '连接中…'
  try {
    const result = !isDesktop
      ? ({ status: 'connected' } as LoginResult)
      : tempToken
        ? await invoke<LoginResult>('complete_login', {
            baseUrl,
            email,
            tempToken,
            totpCode: totpInput.value.trim(),
          })
        : await invoke<LoginResult>('login', { baseUrl, email, password })
    if (result.status === 'requires2fa') {
      tempToken = result.temp_token ?? ''
      totpField.classList.remove('is-hidden')
      totpInput.required = true
      totpInput.focus()
      return
    }
    settings.baseUrl = baseUrl.replace(/\/+$/, '')
    settings.email = email
    connected = true
    tempToken = ''
    passwordInput.value = ''
    totpInput.value = ''
    totpField.classList.add('is-hidden')
    await saveSettings()
    await setSettingsOpen(false)
    await refreshQuota(true)
    startAutoRefresh()
  } catch (error) {
    formError.textContent = errorMessage(error)
  } finally {
    connectButton.disabled = false
    connectButton.textContent = tempToken ? '验证并连接' : '连接平台'
  }
}

async function saveConnectedSettings(): Promise<void> {
  const showModels = readShowModelsFromForm()
  if (!showModels.claude && !showModels.codex && !showModels.grok) {
    formError.textContent = '请至少勾选一个展示模型'
    return
  }
  settings.alwaysOnTop = alwaysOnTopInput.checked
  settings.autoStart = autoStartInput.checked
  settings.showModels = showModels
  settings.cardOpacity = clampCardOpacity(Number(cardOpacityInput.value) / 100)
  settings.maxDisplayAccounts = clampDisplayAccounts(maxDisplayAccountsInput.value)
  settings.refreshIntervalSec = clampRefreshInterval(refreshIntervalInput.value)
  applyCardOpacity()
  if (isDesktop) {
    await getCurrentWindow().setAlwaysOnTop(settings.alwaysOnTop)
    if (settings.autoStart) await enable()
    else await disable()
  }
  await saveSettings()
  refreshHint.textContent = refreshHintText()
  startAutoRefresh()
  await setSettingsOpen(false)
  renderPool()
  if (!settingsOpen) await applyWindowSize()
  await refreshQuota(false)
}

connectionForm.addEventListener('submit', (event) => {
  event.preventDefault()
  if (connected) void saveConnectedSettings()
  else void connect()
})

// Live-preview card opacity while dragging the slider (persists on 保存设置).
cardOpacityInput.addEventListener('input', () => {
  const percent = Math.max(0, Math.min(100, Number(cardOpacityInput.value) || 0))
  cardOpacityLabel.textContent = `${percent}%`
  applyCardOpacity(percent / 100)
})

async function openActionMenu(): Promise<void> {
  // Native popup menus are flaky on Windows transparent always-on-top windows;
  // use the tray menu instead.
  if (isWindows) return
  if (!isDesktop) {
    // Browser preview: fall back to in-page settings affordance.
    showToast('桌面版右键可打开独立菜单')
    return
  }
  try {
    await invoke('show_action_menu')
  } catch (error) {
    showToast(errorMessage(error), 'error')
  }
}

async function handleMenuAction(action: string): Promise<void> {
  switch (action) {
    case 'panel':
      // Large account info window listing every account's quota.
      if (isDesktop) {
        try {
          await invoke('show_account_panel')
        } catch (error) {
          showToast(errorMessage(error), 'error')
        }
      }
      break
    case 'refresh':
      await refreshQuota(true)
      break
    case 'settings':
      await setSettingsOpen(true)
      break
    case 'admin': {
      const url = (settings.baseUrl || baseUrlInput.value.trim()).replace(/\/+$/, '')
      if (!url) {
        showToast('请先配置平台地址', 'error')
        await setSettingsOpen(true)
        return
      }
      if (isDesktop) await openUrl(url)
      else window.open(url, '_blank', 'noopener')
      break
    }
    case 'quit':
      if (isDesktop) await invoke('quit_app')
      break
    default:
      break
  }
}

// Keep drag coordinates in physical pixels across monitors with different scale factors.
let dragActive = false
let dragReady = false
let dragStarted = false
let wasDragging = false
let dragPointerId: number | null = null
let dragSequence = 0
let dragStartScreenX = 0
let dragStartScreenY = 0
let dragStartCursor = new PhysicalPosition(0, 0)
let dragStartWindow = new PhysicalPosition(0, 0)
let dragRafId: number | null = null
let dragMovePromise: Promise<void> = Promise.resolve()

meterCard.addEventListener('pointerdown', async (event) => {
  if (event.button !== 0 || !isDesktop || dragActive) return
  if ((event.target as HTMLElement)?.closest('.icon-button')) return

  dragActive = true
  dragPointerId = event.pointerId
  const sequence = ++dragSequence
  dragReady = false
  dragStarted = false
  wasDragging = false
  dragMovePromise = Promise.resolve()
  dragStartScreenX = event.screenX
  dragStartScreenY = event.screenY

  try {
    meterCard.setPointerCapture(event.pointerId)
  } catch {
    // ignore
  }

  try {
    const win = getCurrentWindow()
    const [windowPosition, pointerPosition] = await Promise.all([win.outerPosition(), cursorPosition()])
    if (!dragActive || sequence !== dragSequence) return
    dragStartWindow = windowPosition
    dragStartCursor = pointerPosition
    dragReady = true
  } catch {
    dragActive = false
  }
})

window.addEventListener('pointermove', (event) => {
  if (!dragActive || !dragReady || !isDesktop || event.pointerId !== dragPointerId) return

  const deltaX = event.screenX - dragStartScreenX
  const deltaY = event.screenY - dragStartScreenY
  if (!dragStarted && Math.hypot(deltaX, deltaY) > 3) {
    dragStarted = true
    wasDragging = true
    document.documentElement.classList.add('is-dragging')
  }

  if (dragStarted && dragRafId === null) {
    const sequence = dragSequence
    dragRafId = requestAnimationFrame(() => {
      void (async () => {
        try {
          const pointerPosition = await cursorPosition()
          if (!dragActive || sequence !== dragSequence) return
          const target = new PhysicalPosition(
            dragStartWindow.x + pointerPosition.x - dragStartCursor.x,
            dragStartWindow.y + pointerPosition.y - dragStartCursor.y,
          )
          dragMovePromise = getCurrentWindow().setPosition(target)
          await dragMovePromise
        } catch {
          // The final drop still selects the monitor under the pointer.
        } finally {
          dragRafId = null
        }
      })()
    })
  }
})

async function finishDrag(event: PointerEvent): Promise<void> {
  if (!dragActive || event.pointerId !== dragPointerId) return
  dragActive = false
  dragReady = false
  dragPointerId = null
  dragSequence++

  try {
    meterCard.releasePointerCapture(event.pointerId)
  } catch {
    // ignore
  }

  if (dragRafId) {
    cancelAnimationFrame(dragRafId)
    dragRafId = null
  }

  if (dragStarted) {
    document.documentElement.classList.remove('is-dragging')
    try {
      await dragMovePromise.catch(() => {})
      const pointerPosition = await cursorPosition()
      const monitor = await monitorFromPoint(pointerPosition.x, pointerPosition.y)
        ?? await currentMonitor()
      if (monitor) {
        const area = monitor.workArea || { position: monitor.position, size: monitor.size }
        const windowHeight = Math.round(widgetWindowSize().height * (monitor.scaleFactor || 1))
        const maxY = Math.max(area.position.y, area.position.y + area.size.height - windowHeight)
        const finalY = Math.round(Math.max(
          area.position.y,
          Math.min(maxY, dragStartWindow.y + pointerPosition.y - dragStartCursor.y),
        ))
        settings.windowMonitor = { x: monitor.position.x, y: monitor.position.y }
        settings.windowY = finalY
        await saveSettings()
        await applyWindowSize()
      }
    } catch (error) {
      console.error('Failed to dock window after drag:', error)
    }

    window.setTimeout(() => {
      wasDragging = false
    }, 120)
  } else {
    wasDragging = false
  }
  dragStarted = false
}

window.addEventListener('pointerup', (event) => void finishDrag(event))
window.addEventListener('pointercancel', (event) => void finishDrag(event))
meterCard.addEventListener('lostpointercapture', (event) => void finishDrag(event))

// Click anywhere on card (outside child buttons) to refresh quota (suppressed after drag).
meterCard.addEventListener('click', (event) => {
  if (wasDragging) {
    event.preventDefault()
    event.stopPropagation()
    return
  }
  if ((event.target as HTMLElement)?.closest('.icon-button')) return
  void refreshQuota(true)
})

// Prevent titlebar-style double-click maximize (borderless widget must stay compact).
meterCard.addEventListener('dblclick', (event) => {
  event.preventDefault()
  event.stopPropagation()
  if (isDesktop) {
    const win = getCurrentWindow()
    void win.setMaximizable(false)
    void win.unmaximize().catch(() => {})
  }
})
shell.addEventListener('dblclick', (event) => {
  event.preventDefault()
  if (isDesktop) {
    void getCurrentWindow().setMaximizable(false)
    void getCurrentWindow().unmaximize().catch(() => {})
  }
})

meterCard.addEventListener('contextmenu', (event) => {
  event.preventDefault()
  event.stopPropagation()
  // Windows: swallow the event only (no popup). Tray menu remains available.
  if (!isWindows) void openActionMenu()
})
meterCard.addEventListener('keydown', (event) => {
  if (event.key === 'Enter' || event.key === ' ') {
    event.preventDefault()
    void refreshQuota(true)
  } else if (
    !isWindows &&
    (event.key === 'ContextMenu' || (event.shiftKey && event.key === 'F10'))
  ) {
    event.preventDefault()
    void openActionMenu()
  }
})
document.addEventListener('keydown', (event) => {
  if (event.key === 'Escape' && settingsOpen) void setSettingsOpen(false)
})
element('#open-site').addEventListener('click', () => {
  const url = baseUrlInput.value.trim()
  if (!url) return
  if (isDesktop) void openUrl(url)
  else window.open(url, '_blank', 'noopener')
})
element('#password-toggle').addEventListener('click', (event) => {
  const button = event.currentTarget as HTMLButtonElement
  const visible = passwordInput.type === 'text'
  passwordInput.type = visible ? 'password' : 'text'
  button.innerHTML = `<i data-lucide="${visible ? 'eye' : 'eye-off'}"></i>`
  button.title = visible ? '显示密码' : '隐藏密码'
  paintIcons()
})
element('#logout-button').addEventListener('click', async () => {
  if (isDesktop) await invoke('logout')
  window.clearInterval(autoRefreshTimer)
  connected = false
  poolRows = []
  await saveSettings()
  renderPool()
  await setSettingsOpen(true)
})
checkUpdateButton.addEventListener('click', () => void checkForUpdate(true))
downloadUpdateButton.addEventListener('click', () => {
  if (pendingUpdateInfo?.available) void openPendingUpdate(true)
  else void checkForUpdate(true)
})

if (isDesktop) {
  void listen('open-settings', () => void setSettingsOpen(true))
  void listen('open-admin', () => void handleMenuAction('admin'))
  void listen<string>('pet-menu-action', (event) => {
    void handleMenuAction(event.payload)
  })
  void listen('main-close-requested', () => {
    void getCurrentWindow().hide()
  })
  void listen('settings-closed', () => {
    settingsOpen = false
    // Surface a queued update prompt once the settings window is gone.
    if (pendingUpdateInfo?.available) {
      window.setTimeout(() => showUpdateBubble(pendingUpdateInfo!, true), 280)
    }
  })
  void listen<{ kind: string; payload?: unknown }>('settings-changed', (event) => {
    void onSettingsChanged(event.payload.kind, event.payload.payload)
  })
}

async function initialize(): Promise<void> {
  await loadSettings()
  await loadAppVersion()
  if (isDesktop) {
    const win = getCurrentWindow()
    // Overlay window — never allow maximize / fullscreen chrome.
    void win.setMaximizable(false)
    void win.unmaximize().catch(() => {})
    try {
      await win.setVisibleOnAllWorkspaces(true)
    } catch {
      // ignore if unsupported or permission missing
    }
    // Check soon after boot so the update prompt can display if needed.
    window.setTimeout(() => void checkForUpdate(false), 2500)
    startUpdatePolling()
  }
  await registerPositionPersistence()
  connected = isDesktop ? await invoke<boolean>('has_session') : true
  if (isDesktop) await getCurrentWindow().setAlwaysOnTop(settings.alwaysOnTop)
  if (!connected) {
    renderPool()
    await setSettingsOpen(true)
    return
  }
  try {
    renderPool()
    await refreshQuota(false)
    await applyWindowSize()
    if (pendingUpdateInfo?.available) {
      window.setTimeout(() => showUpdateBubble(pendingUpdateInfo!, true), 600)
    }
  } catch (error) {
    const err = errorMessage(error)
    showToast(err, 'error')
    const lower = err.toLowerCase()
    if (
      lower.includes('登录') ||
      lower.includes('unauthorized') ||
      lower.includes('token') ||
      lower.includes('401') ||
      lower.includes('keyring')
    ) {
      connected = false
      renderPool()
      await setSettingsOpen(true)
    }
  }
  startAutoRefresh()
}

void initialize()
