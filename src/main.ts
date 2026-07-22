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
  RefreshCw,
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
  alwaysOnTop: boolean
  autoStart: boolean
  /** Max accounts shown on the pet panel / tray. */
  maxDisplayAccounts: number
  /** Auto refresh interval in seconds. */
  refreshIntervalSec: number
  windowX?: number
  windowY?: number
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
const defaultSettings: PetSettings = {
  baseUrl: '',
  email: '',
  alwaysOnTop: true,
  autoStart: false,
  maxDisplayAccounts: 5,
  refreshIntervalSec: 30,
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
let moodTimer: number | undefined
let moveSaveTimer: number | undefined

document.querySelector<HTMLDivElement>('#app')!.innerHTML = `
  <main class="pet-shell" id="pet-shell">
    <section class="pet-stage" aria-live="polite">
      <div
        class="pet-hitbox"
        id="pet-button"
        data-tauri-drag-region
        role="button"
        tabindex="0"
        aria-label="拖动宠物，双击刷新全部账号"
        title="按住拖动 · 双击刷新"
      >
        <img class="pet-image" id="pet-image" src="${petIdle}" alt="Sub2API 桌面宠物" draggable="false" />
        <span class="refresh-orbit" aria-hidden="true"><i data-lucide="refresh-cw"></i></span>
      </div>

      <div class="quota-panel" id="quota-dock" aria-label="账号池额度面板">
        <div class="meter-board" id="account-list" role="list"></div>
        <span class="visually-hidden" id="updated-label">尚未同步</span>
      </div>
      <div class="toast" id="toast" role="status"></div>
    </section>

    <section class="settings-sheet is-hidden" id="settings-sheet" aria-label="连接设置">
      <header class="sheet-header">
        <div>
          <span class="eyebrow">SUB2API PET</span>
          <h1>账号池额度</h1>
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

        <div class="setting-lines connected-only is-hidden">
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
          <label class="switch-line">
            <span><strong>始终置顶</strong><small>宠物保持在其他窗口上方</small></span>
            <input id="always-on-top" type="checkbox" role="switch" checked />
          </label>
          <label class="switch-line">
            <span><strong>开机启动</strong><small>登录系统后自动显示宠物</small></span>
            <input id="auto-start" type="checkbox" role="switch" />
          </label>
        </div>
        <p class="pool-hint connected-only is-hidden">会按设定数量展示 OpenAI/Codex 与 Claude 账号额度，双击宠物可主动刷新。</p>

        <p class="form-error" id="form-error"></p>
        <button class="primary-button login-only" id="connect-button" type="submit">连接平台</button>
        <button class="primary-button connected-only is-hidden" id="save-button" type="submit">保存设置</button>
      </form>

      <footer class="settings-footer connected-only is-hidden">
        <button class="text-button danger" id="logout-button" type="button">
          <i data-lucide="log-out"></i><span>退出登录</span>
        </button>
        <span id="refresh-hint">自动同步</span>
      </footer>
    </section>
  </main>
`

function paintIcons(): void {
  createIcons({
    icons: {
      Eye,
      EyeOff,
      ExternalLink,
      LogOut,
      RefreshCw,
      X,
    },
    attrs: { 'stroke-width': 2 },
  })
}

paintIcons()

const element = <T extends HTMLElement>(selector: string) => document.querySelector<T>(selector)!
const shell = element<HTMLElement>('#pet-shell')
const petImage = element<HTMLImageElement>('#pet-image')
const petButton = element<HTMLElement>('#pet-button')
const quotaDock = element<HTMLElement>('#quota-dock')
const accountList = element<HTMLElement>('#account-list')
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
const maxDisplayAccountsInput = element<HTMLSelectElement>('#max-display-accounts')
const refreshIntervalInput = element<HTMLSelectElement>('#refresh-interval')
const refreshHint = element<HTMLElement>('#refresh-hint')
const formError = element<HTMLElement>('#form-error')
const connectButton = element<HTMLButtonElement>('#connect-button')

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

function normalizeSettings(raw: Partial<PetSettings> | null | undefined): PetSettings {
  return {
    ...defaultSettings,
    ...raw,
    maxDisplayAccounts: clampDisplayAccounts(raw?.maxDisplayAccounts),
    refreshIntervalSec: clampRefreshInterval(raw?.refreshIntervalSec),
  }
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

function visiblePoolRows(): AccountQuotaRow[] {
  const limit = clampDisplayAccounts(settings.maxDisplayAccounts)
  return poolRows.slice(0, limit)
}

populateSettingSelects()

async function loadSettings(): Promise<void> {
  if (!isDesktop) {
    const raw = localStorage.getItem('sub2api-pet-settings')
    settings = normalizeSettings(raw ? JSON.parse(raw) : null)
    return
  }
  appStore = await load('settings.json', { autoSave: true })
  settings = normalizeSettings(await appStore.get<PetSettings>('connection'))
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

function restingMood(): PetMood {
  const hasLow = poolRows.some((row) => {
    if (row.status !== 'active') return false
    const remaining = lowestRemaining(row)
    return remaining != null && remaining <= 15
  })
  return hasLow ? 'alert' : 'idle'
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
      id: 22,
      name: 'Claude 备用',
      status: 'active',
      plan: 'pro',
      platform: 'anthropic',
      account_type: 'oauth',
      remaining_percent: 45,
      windows: [
        {
          label: '5h',
          used_percent: 55,
          remaining_percent: 45,
          reset_at: new Date(Date.now() + 1.2 * 3600000).toISOString(),
        },
        {
          label: '7d',
          used_percent: 24,
          remaining_percent: 76,
          reset_at: new Date(Date.now() + 3.2 * 86400000).toISOString(),
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
          label: '7d',
          used_percent: 40,
          remaining_percent: 60,
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

function toneForWindow(platform: string | undefined, index: number, total: number): string {
  if (platform === 'anthropic') {
    return index === 0 && total > 1 ? 'amber' : 'violet'
  }
  return 'violet'
}

function platformIconSvg(platform?: string): string {
  if (platform === 'anthropic') {
    return `<svg viewBox="0 0 1028 1024" aria-hidden="true" xmlns="http://www.w3.org/2000/svg"><path d="M232.294734 0h564.15038C924.208021 0 1028.741857 104.533836 1028.741857 232.294734v559.408523c0 127.762907-104.533836 232.294734-232.296743 232.294734H232.294734C104.533836 1023.997991 0 919.466164 0 791.703257V232.294734C0 104.533836 104.533836 0 232.294734 0z" fill="#D77655"/><path d="M285.857625 636.170348l147.992151-83.034739 2.487466-7.211239-2.487466-4.010486-7.211239-0.002009-24.734008-1.525029-84.557759-2.28654-73.334024-3.04604-71.049494-3.80956-17.876399-3.80755-16.75724-22.09183 1.725956-11.01879 15.033294-10.106585 21.533255 1.87866 47.585338 3.248976 71.403124 4.926709 51.802778 3.048049 76.739723 7.97275h12.184161l1.727965-4.9247-4.165199-3.04805-3.250985-3.048049-73.892599-50.074814-79.988698-52.919928-41.897119-30.472459-22.650405-15.437155-11.426669-14.47472-4.9247-31.587599 20.568809-22.650404 27.627345 1.878659 7.058535 1.880669 27.984994 21.531246 59.773518 46.267263 78.055789 57.488988 11.42667 9.497779 4.571069-3.248976 0.558575-2.286539-5.129644-8.581556-42.455695-76.739722-45.300808-78.055789-20.162939-32.351118-5.332579-19.399419c-1.878659-7.972749-3.248976-14.675646-3.248976-22.85334l23.411914-31.792544 12.94969-4.165199 31.233969 4.165199 13.154635 11.42667 19.399419 44.388604 31.436904 69.882113 48.754729 95.019983 14.271784 28.185919 7.61711 26.104325 2.845115 7.972749 4.920681-0.002009v-4.57107l4.012495-53.528734 7.414175-65.716914 7.211239-84.557758 2.489475-23.817785 11.7803-28.543568 23.411914-15.437156 18.28227 8.736269 15.033294 21.531245-2.081595 13.916145-8.939204 58.097795-17.52076 91.007487-11.42667 60.942909h6.656684l7.619119-7.61711 30.826089-40.932674 51.802778-64.752469 22.85334-25.696444 26.662899-28.388855 17.11288-13.510274 32.349109-0.002009 23.817785 35.397158-10.66315 36.566549-33.315564 42.252759-27.627345 35.80303-39.614598 53.325798-24.732 42.65863 2.28654 3.403689 5.891155-0.558575 89.484467-19.045789 48.346849-8.734259 57.693933-9.901641 26.104324 12.18818 2.845115 12.391115-10.259289 25.342815-61.704419 15.236229-72.369579 14.47472-107.768746 25.495518-1.320085 0.964446 1.52302 1.878659 48.551794 4.57107 20.769735 1.117149h50.836324l94.664343 7.060545 24.731999 16.353379 14.830359 20.010235-2.487466 15.236229-38.089569 19.399419-51.394898-12.18818-119.956927-28.541558-41.137618-10.259289-5.68621-0.00201v3.403689l34.280009 33.5185 62.821568 56.727478 78.666604 73.131088 4.012495 18.081344-10.106585 14.269775-10.66315-1.52302-69.120603-52.005714-26.662899-23.409905-60.384334-50.838333-4.010486-0.002009v5.33258l13.916145 20.365873 73.486727 110.459148 3.807551 33.874139-5.330571 11.020799-19.045789 6.652664-20.924449-3.80755-43.014268-60.386343-44.388604-68.001445-35.80303-60.942908-4.366125 2.487465-21.127384 227.57096-9.903649 11.629606-22.85334 8.736268-19.04378-14.474719-10.106585-23.411914 10.106585-46.267264 12.18818-60.382324 9.90164-47.993219 8.939204-59.622824 5.33258-19.807299-0.355639-1.320085-4.366125 0.558575-44.947179 61.704418-68.357083 92.379814-54.087309 57.896868-12.94969 5.129644-22.449478-11.631614 2.083604-20.769736 12.54382-18.485205 74.859053-95.222918 45.148105-59.014018 29.150365-34.079083-0.202936-4.9247h-1.725955l-198.824457 129.097057-35.399168 4.57107-15.236229-14.271784 1.880668-23.411915 7.21124-7.61711 59.773518-41.137618-0.202935 0.204944 0.048222 0.202936z" fill="#FCF2EE"/></svg>`
  }
  return `<svg viewBox="0 0 1024 1024" aria-hidden="true" xmlns="http://www.w3.org/2000/svg"><path d="M672.881778 64c53.475556 0.056889 97.393778 0.568889 132.835555 5.347556 41.358222 5.575111 76.174222 17.464889 103.822223 45.112888 27.648 27.648 39.537778 62.464 45.112888 103.822223 5.347556 39.822222 5.347556 90.510222 5.347556 153.372444v280.689778c0 62.862222 0 113.493333-5.347556 153.372444-5.575111 41.358222-17.464889 76.174222-45.112888 103.822223-27.648 27.648-62.464 39.537778-103.822223 45.112888-39.822222 5.347556-90.510222 5.347556-153.372444 5.347556H371.655111c-62.862222 0-113.493333 0-153.372444-5.347556-41.358222-5.575111-76.174222-17.464889-103.822223-45.112888-27.648-27.648-39.537778-62.464-45.112888-103.822223-5.347556-39.822222-5.347556-90.510222-5.347556-153.372444V351.118222c0.056889-53.475556 0.568889-97.393778 5.347556-132.835555 5.575111-41.358222 17.464889-76.174222 45.112888-103.822223 27.648-27.648 62.464-39.537778 103.822223-45.112888 39.822222-5.347556 90.510222-5.347556 153.372444-5.347556z m-194.844445 128a163.441778 163.441778 0 0 0-155.875555 111.616c-45.511111 9.102222-84.878222 37.319111-108.088889 77.425778a159.402667 159.402667 0 0 0 20.081778 188.928 156.672 156.672 0 0 0 13.994666 131.015111 164.522667 164.522667 0 0 0 176.014223 77.425778 161.962667 161.962667 0 0 0 121.742222 53.589333 162.929778 162.929778 0 0 0 155.875555-111.843556 161.564444 161.564444 0 0 0 108.032-77.368888 158.833778 158.833778 0 0 0-20.081777-188.871112c14.791111-43.52 9.671111-91.420444-13.994667-130.901333a164.522667 164.522667 0 0 0-175.957333-77.425778 162.417778 162.417778 0 0 0-121.742223-53.589333z m133.688889 296.789333l54.613334 31.232a1.422222 1.422222 0 0 1 0.967111 1.365334v148.821333c0 66.161778-54.328889 119.808-121.173334 120.035556-28.444444 0-55.978667-9.841778-77.937777-27.875556 1.080889-0.568889 2.673778-1.365333 3.868444-2.161778l129.137778-73.614222a20.195556 20.195556 0 0 0 10.524444-18.204444V488.789333z m-29.582222 96.995556v62.236444c0 0.568889-0.170667 1.137778-0.796444 1.592889l-130.56 74.353778a122.709333 122.709333 0 0 1-166.001778-43.804444 118.613333 118.613333 0 0 1-14.563556-80.384 30.435556 30.435556 0 0 0 3.811556 2.218666l129.137778 73.557334a21.162667 21.162667 0 0 0 21.276444 0l157.752889-89.770667z m15.872-201.955556l130.56 74.353778a118.954667 118.954667 0 0 1 44.373333 163.84 121.628444 121.628444 0 0 1-63.260444 52.622222V522.808889a19.683556 19.683556 0 0 0-10.353778-17.92L541.582222 415.175111l54.556445-31.175111a1.991111 1.991111 0 0 1 1.877333-0.170667zM313.969778 349.297778V501.191111c0 7.395556 4.039111 14.336 10.524444 18.090667l157.696 89.884444-54.499555 30.890667a1.991111 1.991111 0 0 1-1.820445 0.170667L295.310222 565.816889a119.296 119.296 0 0 1 18.659556-216.462222zM512 432.014222l70.371556 39.992889v79.985778l-70.144 39.992889-70.371556-39.992889-0.170667-79.985778 70.314667-39.992889z m-33.792-197.973333c28.330667-0.113778 55.808 9.728 77.653333 27.761778-1.024 0.568889-2.616889 1.422222-3.811555 2.218666l-129.137778 73.557334a19.968 19.968 0 0 0-10.524444 18.204444l-0.227556 179.2-54.499556-30.947555a1.536 1.536 0 0 1-1.024-1.365334V353.792c0-66.161778 54.499556-120.035556 121.571556-119.808z m260.892444 109.966222c12.458667 21.162667 18.033778 45.568 15.928889 69.802667l-1.365333 10.296889-1.763556-1.024a31.516444 31.516444 0 0 0-2.048-1.080889l-129.137777-73.557334a21.333333 21.333333 0 0 0-21.276445 0L441.685333 438.215111V375.978667c0-0.568889 0.170667-1.137778 0.853334-1.592889l130.56-74.410667a122.538667 122.538667 0 0 1 165.944889 44.032z" fill="#222222"/></svg>`
}

function createMeterColumn(window: QuotaWindow, tone: string, accountName: string): HTMLElement {
  const remaining = Math.max(0, Math.min(100, window.remaining_percent))
  const filled = Math.round((remaining / 100) * BAR_SEGMENTS)
  const isLow = remaining <= 15

  const col = document.createElement('div')
  col.className = `meter-col tone-${tone}${isLow ? ' is-low' : ''}`
  col.title = `${accountName} · ${window.label} 剩余 ${Math.round(remaining)}%\n${formatReset(window.reset_at ?? undefined)}`

  const value = document.createElement('div')
  value.className = 'meter-value'
  value.textContent = `${Math.round(remaining)}%`

  const stack = document.createElement('div')
  stack.className = 'meter-stack'
  stack.setAttribute('role', 'progressbar')
  stack.setAttribute('aria-label', `${accountName} ${window.label} 剩余`)
  stack.setAttribute('aria-valuemin', '0')
  stack.setAttribute('aria-valuemax', '100')
  stack.setAttribute('aria-valuenow', String(Math.round(remaining)))

  for (let i = BAR_SEGMENTS; i >= 1; i -= 1) {
    const segment = document.createElement('span')
    segment.className = `meter-seg${i <= filled ? ' is-on' : ''}`
    stack.append(segment)
  }

  col.append(value, stack)
  return col
}

function renderPool(): void {
  accountList.replaceChildren()

  if (!connected) {
    updatedLabel.textContent = '等待连接'
    quotaDock.classList.remove('is-low', 'has-data')
    const empty = document.createElement('div')
    empty.className = 'meter-empty'
    empty.textContent = '连接后展示账号池'
    accountList.append(empty)
    void syncTrayMenu()
    return
  }

  if (!poolRows.length) {
    updatedLabel.textContent = '账号池为空'
    quotaDock.classList.remove('is-low')
    quotaDock.classList.add('has-data')
    const empty = document.createElement('div')
    empty.className = 'meter-empty'
    empty.textContent = '暂无 Codex / Claude 账号'
    accountList.append(empty)
    void syncTrayMenu()
    return
  }

  const displayRows = visiblePoolRows()
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

  for (const [index, row] of displayRows.entries()) {
    const windows = rowWindows(row)
    const isInactive = row.status !== 'active'
    const group = document.createElement('article')
    group.className = `meter-group platform-${row.platform || 'openai'}${isInactive ? ' is-inactive' : ''}`
    group.setAttribute('role', 'listitem')
    group.dataset.name = row.name

    if (index > 0) {
      const divider = document.createElement('div')
      divider.className = 'meter-divider'
      divider.setAttribute('aria-hidden', 'true')
      accountList.append(divider)
    }

    const meters = document.createElement('div')
    meters.className = 'meter-columns'

    if (!windows.length) {
      const emptyCol = document.createElement('div')
      emptyCol.className = 'meter-col tone-muted'
      emptyCol.innerHTML = `<div class="meter-value">--%</div><div class="meter-stack empty-stack"><span class="meter-seg"></span><span class="meter-seg"></span><span class="meter-seg"></span><span class="meter-seg"></span><span class="meter-seg"></span></div>`
      emptyCol.title = `${row.name} · 暂无额度数据`
      meters.append(emptyCol)
    } else {
      windows.forEach((window, windowIndex) => {
        meters.append(
          createMeterColumn(
            window,
            toneForWindow(row.platform, windowIndex, windows.length),
            row.name,
          ),
        )
      })
    }

    const badge = document.createElement('div')
    badge.className = `platform-chip platform-${row.platform || 'openai'}`
    badge.title = `${row.name}${row.plan ? ` · ${row.plan}` : ''}`
    badge.innerHTML = platformIconSvg(row.platform)

    group.append(meters, badge)
    accountList.append(group)
  }

  void syncTrayMenu()
}

async function syncTrayMenu(): Promise<void> {
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
      },
    })
  } catch {
    // Tray updates are best-effort and should not interrupt the UI.
  }
}

async function refreshQuota(force: boolean): Promise<void> {
  if (refreshing || !connected) return
  refreshing = true
  setMood('refreshing')
  quotaDock.classList.add('is-refreshing')
  try {
    poolRows = isDesktop
      ? await invoke<AccountQuotaRow[]>('refresh_pool_quotas', { force })
      : mockPoolRows().map((row) => ({
          ...row,
          source: force ? 'active' : 'cached',
          updated_at: new Date().toISOString(),
        }))
    renderPool()
    if (!settingsOpen) await resizeWindow('pet')
    setMood('success')
    if (force) showToast(`已更新 ${poolRows.length} 个账号额度`)
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
  const intervalMs = clampRefreshInterval(settings.refreshIntervalSec) * 1000
  autoRefreshTimer = window.setInterval(() => void refreshQuota(false), intervalMs)
}

function petWindowSize(): { width: number; height: number } {
  // Fit the transparent window tightly around pet + meters.
  // Pet image ~152px, meters (label+bars+icon) ~55px, pet/panel overlap ~18px.
  const rows = visiblePoolRows()
  let content = 8
  for (const row of rows) {
    const count = Math.max(rowWindows(row).length, 1)
    // Dual Claude columns are only ~25px wide; single bar group ~28px + padding.
    content += count === 1 ? 34 : 42
  }
  content += Math.max(0, rows.length - 1) * 10
  // Keep at least as wide as the pet artwork.
  const width = Math.min(420, Math.max(168, content + 12))
  // Pet 152 + meters ~55 - overlap 18 + padding/shadow ~20 ≈ 209
  const height = 210
  return { width, height }
}

type WindowMode = 'pet' | 'settings'

async function resizeWindow(mode: WindowMode): Promise<void> {
  if (!isDesktop) return
  if (mode === 'settings') {
    await getCurrentWindow().setSize(new LogicalSize(380, 640))
    return
  }
  const size = petWindowSize()
  await getCurrentWindow().setSize(new LogicalSize(size.width, size.height))
}

async function setSettingsOpen(open: boolean): Promise<void> {
  settingsOpen = open
  settingsSheet.classList.toggle('is-hidden', !open)
  shell.classList.toggle('has-settings', open)
  shell.classList.toggle('pet-hidden', open)
  baseUrlInput.value = settings.baseUrl
  emailInput.value = settings.email
  alwaysOnTopInput.checked = settings.alwaysOnTop
  autoStartInput.checked = settings.autoStart
  maxDisplayAccountsInput.value = String(clampDisplayAccounts(settings.maxDisplayAccounts))
  refreshIntervalInput.value = String(clampRefreshInterval(settings.refreshIntervalSec))
  // If saved interval is not in the select options, add it so value sticks.
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
  await resizeWindow(open ? 'settings' : 'pet')
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
  settings.alwaysOnTop = alwaysOnTopInput.checked
  settings.autoStart = autoStartInput.checked
  settings.maxDisplayAccounts = clampDisplayAccounts(maxDisplayAccountsInput.value)
  settings.refreshIntervalSec = clampRefreshInterval(refreshIntervalInput.value)
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
  await refreshQuota(false)
}

connectionForm.addEventListener('submit', (event) => {
  event.preventDefault()
  if (connected) void saveConnectedSettings()
  else void connect()
})

// Drag anywhere on the pet body; double-click still force-refreshes.
petButton.addEventListener('mousedown', (event) => {
  if (!isDesktop || event.button !== 0) return
  void getCurrentWindow().startDragging()
})
petButton.addEventListener('contextmenu', (event) => {
  event.preventDefault()
})
petButton.addEventListener('dblclick', (event) => {
  event.preventDefault()
  void refreshQuota(true)
})
petButton.addEventListener('keydown', (event) => {
  if (event.key === 'Enter' || event.key === ' ') {
    event.preventDefault()
    void refreshQuota(true)
  }
})
document.addEventListener('keydown', (event) => {
  if (event.key === 'Escape' && settingsOpen) void setSettingsOpen(false)
})
element('#close-settings').addEventListener('click', () => void setSettingsOpen(false))
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

if (isDesktop) {
  void listen('open-settings', () => void setSettingsOpen(true))
  void listen('open-admin', () => {
    const url = (baseUrlInput.value.trim() || settings.baseUrl).replace(/\/+$/, '')
    if (!url) {
      showToast('请先配置平台地址', 'error')
      void setSettingsOpen(true)
      return
    }
    void openUrl(url)
  })
}

async function initialize(): Promise<void> {
  await loadSettings()
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
    await resizeWindow('pet')
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
