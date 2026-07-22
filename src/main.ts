import './style.css'
import { invoke } from '@tauri-apps/api/core'
import { LogicalSize, PhysicalPosition } from '@tauri-apps/api/dpi'
import { listen } from '@tauri-apps/api/event'
import { getCurrentWindow } from '@tauri-apps/api/window'
import { disable, enable, isEnabled } from '@tauri-apps/plugin-autostart'
import { openUrl } from '@tauri-apps/plugin-opener'
import { load, type Store } from '@tauri-apps/plugin-store'
import {
  createIcons,
  Eye,
  EyeOff,
  ExternalLink,
  LogOut,
  Minus,
  Pin,
  RefreshCw,
  Settings as SettingsIcon,
  X,
} from 'lucide'
import petIdle from './assets/pet-idle.png'
import petRefresh from './assets/pet-refresh.png'
import petSuccess from './assets/pet-success.png'
import petAlert from './assets/pet-alert.png'

type PetMood = 'idle' | 'refreshing' | 'success' | 'alert'

interface PetSettings {
  baseUrl: string
  email: string
  accountId: number | null
  accountName: string
  alwaysOnTop: boolean
  autoStart: boolean
  windowX?: number
  windowY?: number
}

interface LoginResult {
  status: 'connected' | 'requires2fa'
  temp_token?: string
  email_masked?: string
}

interface CodexAccount {
  id: number
  name: string
  status: string
  plan?: string
}

interface QuotaSnapshot {
  account_id: number
  account_name: string
  used_percent: number
  remaining_percent: number
  reset_at?: string
  updated_at: string
  source: 'active' | 'cached'
}

const isDesktop = '__TAURI_INTERNALS__' in window
const defaultSettings: PetSettings = {
  baseUrl: '',
  email: '',
  accountId: null,
  accountName: '',
  alwaysOnTop: true,
  autoStart: false,
}

let settings = { ...defaultSettings }
let appStore: Store | null = null
let snapshot: QuotaSnapshot | null = null
let accounts: CodexAccount[] = []
let connected = false
let settingsOpen = false
let refreshing = false
let tempToken = ''
let autoRefreshTimer: number | undefined
let moodTimer: number | undefined
let moveSaveTimer: number | undefined

document.querySelector<HTMLDivElement>('#app')!.innerHTML = `
  <main class="pet-shell" id="pet-shell">
    <div class="drag-strip" data-tauri-drag-region aria-hidden="true"></div>

    <nav class="window-actions" aria-label="窗口操作">
      <button class="icon-button pin-button is-active" id="pin-button" title="取消置顶" aria-label="切换窗口置顶">
        <i data-lucide="pin"></i>
      </button>
      <button class="icon-button" id="settings-button" title="连接设置" aria-label="打开连接设置">
        <i data-lucide="settings"></i>
      </button>
      <button class="icon-button" id="hide-button" title="隐藏到菜单栏" aria-label="隐藏窗口">
        <i data-lucide="minus"></i>
      </button>
    </nav>

    <section class="pet-stage" aria-live="polite">
      <button class="pet-hitbox" id="pet-button" aria-label="Codex 周额度，双击刷新" title="双击主动刷新">
        <img class="pet-image" id="pet-image" src="${petIdle}" alt="Sub2API 桌面宠物" draggable="false" />
        <span class="refresh-orbit" aria-hidden="true"><i data-lucide="refresh-cw"></i></span>
      </button>

      <div class="quota-dock" id="quota-dock">
        <div class="quota-heading">
          <div class="account-block">
            <span class="status-dot" id="status-dot"></span>
            <span class="account-name" id="account-name">等待连接</span>
          </div>
          <strong class="quota-value" id="quota-value">--%</strong>
        </div>
        <div class="quota-meta">
          <span>周限额剩余</span>
          <span id="reset-label">--</span>
        </div>
        <div class="quota-track" role="progressbar" aria-label="Codex 周限额剩余" aria-valuemin="0" aria-valuemax="100" aria-valuenow="0">
          <span class="quota-fill" id="quota-fill"></span>
        </div>
        <div class="dock-footer">
          <span id="updated-label">尚未同步</span>
          <button class="dock-refresh" id="refresh-button" title="主动刷新" aria-label="主动刷新">
            <i data-lucide="refresh-cw"></i>
          </button>
        </div>
      </div>
      <div class="toast" id="toast" role="status"></div>
    </section>

    <section class="settings-sheet is-hidden" id="settings-sheet" aria-label="连接设置">
      <header class="sheet-header">
        <div>
          <span class="eyebrow">SUB2API PET</span>
          <h1>Codex 周额度</h1>
        </div>
        <button class="icon-button" id="close-settings" title="关闭" aria-label="关闭设置">
          <i data-lucide="x"></i>
        </button>
      </header>

      <form id="connection-form" autocomplete="on">
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

        <label class="field connected-only is-hidden">
          <span>Codex 账号</span>
          <select id="account-select"></select>
        </label>

        <div class="setting-lines connected-only is-hidden">
          <label class="switch-line">
            <span><strong>始终置顶</strong><small>宠物保持在其他窗口上方</small></span>
            <input id="always-on-top" type="checkbox" role="switch" checked />
          </label>
          <label class="switch-line">
            <span><strong>开机启动</strong><small>登录系统后自动显示宠物</small></span>
            <input id="auto-start" type="checkbox" role="switch" />
          </label>
        </div>

        <p class="form-error" id="form-error"></p>
        <button class="primary-button login-only" id="connect-button" type="submit">连接平台</button>
        <button class="primary-button connected-only is-hidden" id="save-button" type="submit">保存设置</button>
      </form>

      <footer class="settings-footer connected-only is-hidden">
        <button class="text-button danger" id="logout-button" type="button">
          <i data-lucide="log-out"></i><span>退出登录</span>
        </button>
        <span>每 30 秒自动同步</span>
      </footer>
    </section>
  </main>
`

createIcons({
  icons: {
    Eye,
    EyeOff,
    ExternalLink,
    LogOut,
    Minus,
    Pin,
    RefreshCw,
    Settings: SettingsIcon,
    X,
  },
  attrs: { 'stroke-width': 2 },
})

const element = <T extends HTMLElement>(selector: string) => document.querySelector<T>(selector)!
const shell = element<HTMLElement>('#pet-shell')
const petImage = element<HTMLImageElement>('#pet-image')
const petButton = element<HTMLButtonElement>('#pet-button')
const quotaDock = element<HTMLElement>('#quota-dock')
const quotaFill = element<HTMLElement>('#quota-fill')
const quotaValue = element<HTMLElement>('#quota-value')
const accountName = element<HTMLElement>('#account-name')
const resetLabel = element<HTMLElement>('#reset-label')
const updatedLabel = element<HTMLElement>('#updated-label')
const statusDot = element<HTMLElement>('#status-dot')
const toast = element<HTMLElement>('#toast')
const settingsSheet = element<HTMLElement>('#settings-sheet')
const connectionForm = element<HTMLFormElement>('#connection-form')
const baseUrlInput = element<HTMLInputElement>('#base-url')
const emailInput = element<HTMLInputElement>('#email')
const passwordInput = element<HTMLInputElement>('#password')
const totpField = element<HTMLElement>('#totp-field')
const totpInput = element<HTMLInputElement>('#totp-code')
const accountSelect = element<HTMLSelectElement>('#account-select')
const alwaysOnTopInput = element<HTMLInputElement>('#always-on-top')
const autoStartInput = element<HTMLInputElement>('#auto-start')
const formError = element<HTMLElement>('#form-error')
const connectButton = element<HTMLButtonElement>('#connect-button')
const progressbar = element<HTMLElement>('.quota-track')

function errorMessage(error: unknown): string {
  if (typeof error === 'string') return error
  if (error instanceof Error) return error.message
  return '操作失败，请稍后重试'
}

async function loadSettings(): Promise<void> {
  if (!isDesktop) {
    const raw = localStorage.getItem('sub2api-pet-settings')
    settings = raw ? { ...defaultSettings, ...JSON.parse(raw) } : { ...defaultSettings }
    return
  }
  appStore = await load('settings.json', { autoSave: true })
  settings = { ...defaultSettings, ...((await appStore.get<PetSettings>('connection')) ?? {}) }
  settings.autoStart = await isEnabled().catch(() => settings.autoStart)
}

async function saveSettings(): Promise<void> {
  if (!isDesktop) {
    localStorage.setItem('sub2api-pet-settings', JSON.stringify(settings))
    return
  }
  await appStore?.set('connection', settings)
}

async function registerPositionPersistence(): Promise<void> {
  if (!isDesktop) return
  const appWindow = getCurrentWindow()
  if (Number.isFinite(settings.windowX) && Number.isFinite(settings.windowY)) {
    await appWindow.setPosition(new PhysicalPosition(settings.windowX!, settings.windowY!))
  }
  await appWindow.onMoved(({ payload }) => {
    settings.windowX = payload.x
    settings.windowY = payload.y
    window.clearTimeout(moveSaveTimer)
    moveSaveTimer = window.setTimeout(() => void saveSettings(), 250)
  })
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

function setMood(mood: PetMood): void {
  window.clearTimeout(moodTimer)
  shell.dataset.mood = mood
  const images: Record<PetMood, string> = {
    idle: petIdle,
    refreshing: petRefresh,
    success: petSuccess,
    alert: petAlert,
  }
  petImage.src = images[mood]
}

function restingMood(): PetMood {
  return snapshot && snapshot.remaining_percent <= 15 ? 'alert' : 'idle'
}

function showToast(message: string, kind: 'normal' | 'error' = 'normal'): void {
  toast.textContent = message
  toast.className = `toast is-visible ${kind === 'error' ? 'is-error' : ''}`
  window.setTimeout(() => toast.classList.remove('is-visible'), 2600)
}

function renderSnapshot(): void {
  if (!snapshot) {
    accountName.textContent = connected ? settings.accountName || '选择 Codex 账号' : '等待连接'
    quotaValue.textContent = '--%'
    quotaFill.style.width = '0%'
    resetLabel.textContent = '--'
    updatedLabel.textContent = '尚未同步'
    statusDot.className = `status-dot ${connected ? 'is-online' : ''}`
    progressbar.setAttribute('aria-valuenow', '0')
    return
  }
  const remaining = Math.max(0, Math.min(100, snapshot.remaining_percent))
  accountName.textContent = snapshot.account_name
  quotaValue.textContent = `${Math.round(remaining)}%`
  quotaFill.style.width = `${remaining}%`
  quotaDock.classList.toggle('is-low', remaining <= 15)
  resetLabel.textContent = formatReset(snapshot.reset_at)
  updatedLabel.textContent = `${formatClock(snapshot.updated_at)} 更新${snapshot.source === 'cached' ? ' · 缓存' : ''}`
  statusDot.className = 'status-dot is-online'
  progressbar.setAttribute('aria-valuenow', String(Math.round(remaining)))
}

function populateAccounts(): void {
  accountSelect.replaceChildren()
  for (const account of accounts) {
    const option = document.createElement('option')
    option.value = String(account.id)
    option.textContent = `${account.name}${account.plan ? ` · ${account.plan}` : ''}${account.status !== 'active' ? ' · 已停用' : ''}`
    option.disabled = account.status !== 'active'
    option.selected = account.id === settings.accountId
    accountSelect.append(option)
  }
}

async function listAccounts(): Promise<void> {
  accounts = isDesktop
    ? await invoke<CodexAccount[]>('list_codex_accounts')
    : [
        { id: 7, name: 'Codex 主账号', status: 'active', plan: 'team' },
        { id: 12, name: 'Codex 备用账号', status: 'active', plan: 'plus' },
      ]
  if (!accounts.length) throw new Error('账号池中没有可用的 OpenAI/Codex 账号')
  if (!accounts.some((account) => account.id === settings.accountId && account.status === 'active')) {
    const first = accounts.find((account) => account.status === 'active') ?? accounts[0]
    settings.accountId = first.id
    settings.accountName = first.name
    await saveSettings()
  }
  populateAccounts()
}

async function refreshQuota(force: boolean): Promise<void> {
  if (refreshing || !connected || settings.accountId === null) return
  refreshing = true
  setMood('refreshing')
  quotaDock.classList.add('is-refreshing')
  try {
    snapshot = isDesktop
      ? await invoke<QuotaSnapshot>('refresh_quota', { accountId: settings.accountId, force })
      : {
          account_id: settings.accountId,
          account_name: settings.accountName || 'Codex 主账号',
          used_percent: 36,
          remaining_percent: 64,
          reset_at: new Date(Date.now() + 3.4 * 86400000).toISOString(),
          updated_at: new Date().toISOString(),
          source: force ? 'active' : 'cached',
        }
    renderSnapshot()
    setMood('success')
    if (force) showToast('周额度已更新')
    moodTimer = window.setTimeout(() => setMood(restingMood()), 1300)
  } catch (error) {
    setMood('alert')
    showToast(errorMessage(error), 'error')
    moodTimer = window.setTimeout(() => setMood(restingMood()), 2200)
  } finally {
    refreshing = false
    quotaDock.classList.remove('is-refreshing')
  }
}

function startAutoRefresh(): void {
  window.clearInterval(autoRefreshTimer)
  autoRefreshTimer = window.setInterval(() => void refreshQuota(false), 30_000)
}

async function resizeWindow(open: boolean): Promise<void> {
  if (!isDesktop) return
  await getCurrentWindow().setSize(new LogicalSize(open ? 380 : 340, open ? 590 : 430))
}

async function setSettingsOpen(open: boolean): Promise<void> {
  settingsOpen = open
  settingsSheet.classList.toggle('is-hidden', !open)
  shell.classList.toggle('has-settings', open)
  baseUrlInput.value = settings.baseUrl
  emailInput.value = settings.email
  alwaysOnTopInput.checked = settings.alwaysOnTop
  autoStartInput.checked = settings.autoStart
  formError.textContent = ''
  document.querySelectorAll('.connected-only').forEach((item) => item.classList.toggle('is-hidden', !connected))
  document.querySelectorAll('.login-only').forEach((item) => item.classList.toggle('is-hidden', connected))
  if (connected) {
    try {
      await listAccounts()
    } catch (error) {
      formError.textContent = errorMessage(error)
    }
  }
  await resizeWindow(open)
  if (open && !connected) window.setTimeout(() => baseUrlInput.focus(), 120)
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
    await listAccounts()
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
  const selected = accounts.find((account) => account.id === Number(accountSelect.value))
  if (selected) {
    settings.accountId = selected.id
    settings.accountName = selected.name
  }
  settings.alwaysOnTop = alwaysOnTopInput.checked
  settings.autoStart = autoStartInput.checked
  if (isDesktop) {
    await getCurrentWindow().setAlwaysOnTop(settings.alwaysOnTop)
    if (settings.autoStart) await enable()
    else await disable()
  }
  await saveSettings()
  element('#pin-button').classList.toggle('is-active', settings.alwaysOnTop)
  await setSettingsOpen(false)
  snapshot = null
  renderSnapshot()
  await refreshQuota(false)
}

connectionForm.addEventListener('submit', (event) => {
  event.preventDefault()
  if (connected) void saveConnectedSettings()
  else void connect()
})

petButton.addEventListener('dblclick', () => void refreshQuota(true))
element('#refresh-button').addEventListener('click', () => void refreshQuota(true))
element('#settings-button').addEventListener('click', () => void setSettingsOpen(true))
element('#close-settings').addEventListener('click', () => void setSettingsOpen(false))
element('#hide-button').addEventListener('click', () => {
  if (isDesktop) void getCurrentWindow().hide()
})
element('#pin-button').addEventListener('click', async () => {
  settings.alwaysOnTop = !settings.alwaysOnTop
  element('#pin-button').classList.toggle('is-active', settings.alwaysOnTop)
  element('#pin-button').title = settings.alwaysOnTop ? '取消置顶' : '保持置顶'
  if (isDesktop) await getCurrentWindow().setAlwaysOnTop(settings.alwaysOnTop)
  await saveSettings()
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
  createIcons({ icons: { Eye, EyeOff }, attrs: { 'stroke-width': 2 } })
})
element('#logout-button').addEventListener('click', async () => {
  if (isDesktop) await invoke('logout')
  window.clearInterval(autoRefreshTimer)
  connected = false
  snapshot = null
  settings.accountId = null
  settings.accountName = ''
  await saveSettings()
  renderSnapshot()
  await setSettingsOpen(true)
})

if (isDesktop) {
  void listen('open-settings', () => void setSettingsOpen(true))
}

async function initialize(): Promise<void> {
  await loadSettings()
  await registerPositionPersistence()
  connected = isDesktop ? await invoke<boolean>('has_session') : true
  element('#pin-button').classList.toggle('is-active', settings.alwaysOnTop)
  if (isDesktop) await getCurrentWindow().setAlwaysOnTop(settings.alwaysOnTop)
  if (!connected) {
    renderSnapshot()
    await setSettingsOpen(true)
    return
  }
  try {
    await listAccounts()
    renderSnapshot()
    await refreshQuota(false)
  } catch (error) {
    showToast(errorMessage(error), 'error')
    if (errorMessage(error).includes('登录')) {
      connected = false
      await setSettingsOpen(true)
    }
  }
  startAutoRefresh()
}

void initialize()
