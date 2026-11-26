import { readFileSync } from 'node:fs'
import { copyFile, readdir, realpath, stat, writeFile } from 'node:fs/promises'
import { createRequire } from 'node:module'
import { isAbsolute, join, relative, resolve, sep } from 'node:path'
import { fileURLToPath, pathToFileURL } from 'node:url'
import { resgDisplayProgram } from '../../league-resg/resg-display.ts'
import { artworkModule, logoComponents, useMitIconImports } from './artwork.mjs'
import { registerChampionPicker } from './champion-picker.mjs'
import { MINI_ACTIVITY_MODULE, patchNativeMini } from './mini-renderer.mjs'

const IPC_MODULE = 'virtual:mgs-league-native-ipc'
const IPC_SOURCE = fileURLToPath(new URL('./tauri-ipc.ts', import.meta.url))
const GAME_SHORTCUT_MODULE = 'virtual:mgs-league-game-shortcut'
const GAME_SHORTCUT_COMPONENT = new URL('./NativeGameShortcut.vue', import.meta.url)
const OPGG_COMPANION = new URL('./NativeOpggCompanion.vue', import.meta.url)
const MINI_ACTIVITY = new URL('./native-aux-activity.ts', import.meta.url)
const TOOLBOX_ICON = new URL('./artwork/toolbox.ico', import.meta.url)
const GAME_ART_ORIGIN = 'http://league-art.localhost'
const WINDOW_ENTRIES = {
  main: 'main-window.html',
  aux: 'aux-window.html',
  opgg: 'opgg-window.html'
}

function replaceOnce(source, anchor, replacement, file) {
  const first = source.indexOf(anchor)
  if (first === -1 || source.indexOf(anchor, first + anchor.length) !== -1) {
    throw new Error(`Expected exactly one native renderer patch anchor in ${file}: ${anchor}`)
  }
  return source.slice(0, first) + replacement + source.slice(first + anchor.length)
}

const IPC_TRANSFORMS = {
  'index.ts': [
    [
      "import { ElectronAPI } from '@electron-toolkit/preload'",
      `import { listenLeagueEvents } from '${IPC_MODULE}'`
    ],
    ["import { IpcRendererEvent } from 'electron'\n", ''],
    [
      `declare global {
  interface Window {
    electron: ElectronAPI
  }
}

`,
      ''
    ],
    ['private _cancelFn: (() => void) | null', 'private _cancelFn: (() => Promise<void>) | null'],
    [
      `  private _dispatchEvent(
    _event: IpcRendererEvent,
    namespace: string,
    eventName: string,
    ...args: any[]
  ) {
    this._eventRegistry.dispatch(_event, namespace, eventName, ...args)
  }

  async onInit() {
    this._cancelFn = window.electron.ipcRenderer.on('akari-event', this._dispatchEvent)
    await window.electron.ipcRenderer.invoke('akariRendererRegister', 'register')
  }

  async onDispose() {
    this._cancelFn?.()
    this._cancelFn = null
    this._eventRegistry.clear()
    await window.electron.ipcRenderer.invoke('akariRendererRegister', 'unregister')
  }`,
      `  private _dispatchEvent(namespace: string, eventName: string, ...args: unknown[]) {
    this._eventRegistry.dispatch(namespace, eventName, ...args)
  }

  async onInit() {
    this._cancelFn = await listenLeagueEvents(this._dispatchEvent)
  }

  async onDispose() {
    const cancel = this._cancelFn
    this._cancelFn = null
    this._eventRegistry.clear()
    await cancel?.()
  }`
    ]
  ],
  'event-registry.ts': [
    ["import type { IpcRendererEvent } from 'electron'\n\n", ''],
    ['dispatch(_event: IpcRendererEvent, namespace:', 'dispatch(namespace:']
  ],
  'call-service.ts': [
    [
      "import { formatError } from '@shared/utils/errors'",
      `import { formatError } from '@shared/utils/errors'
import { invokeLeagueCall } from '${IPC_MODULE}'`
    ],
    [
      `    const result: IpcMainDataType<T> = await window.electron.ipcRenderer.invoke(
      'akariCall',
      namespace,
      fnName,
      ...args
    )`,
      '    const result = await invokeLeagueCall<IpcMainDataType<T>>(namespace, fnName, ...args)'
    ]
  ]
}

const NATIVE_STATE_TRANSFORMS = [
  ['  raw: boolean\n}', '  raw: boolean\n  revision?: number\n}'],
  [
    '  async sync(namespace: string, stateId: string, store: any) {',
    `  async sync(namespace: string, stateId: string, store: any) {
    // Tauri can deliver a state event before the older initial response.
    const revisions = new Map<string, number>()
    const accept = (key: string, revision?: number) => {
      if (revision === undefined) return true
      if (revision < (revisions.get(key) ?? -1)) return false
      revisions.set(key, revision)
      return true
    }
    const decode = (key: string, value: unknown) => {
      const championIds = key === 'currentPickableChampionIds' ||
        key === 'currentBannableChampionIds' || key === 'disabledChampionIds'
      if (namespace === 'league-client-main' && stateId === 'champSelect' && championIds &&
          Array.isArray(value) && value.every(Number.isSafeInteger)) {
        return new Set(value)
      }
      return value
    }`
  ],
  [
    '(path, value, { action, raw }) => {',
    `(path, value, { action, raw, revision }) => {
        if (!accept(path, revision)) return
        value = decode(path, value)`
  ],
  [
    'Object.entries(initial).forEach(([key, { value, config }]) => {',
    `Object.entries(initial).forEach(([key, { value, config }]) => {
      if (!accept(key, config.revision)) return
      value = decode(key, value)`
  ]
]

const DRAG_REGIONS = [
  [
    'src-main-window/components/titlebar/MainWindowTitlebar.vue',
    'class="shard-area"',
    '@mousedown.self.left',
    'main'
  ],
  ['src-main-window/components/titlebar/PlayerTabsTitle.vue', 'class="player-tabs-title"', '@mousedown.self.left', 'main', 'tsx'],
  ['src-main-window/components/sidebar/Sidebar.vue', 'class="app-sidebar__logo"', '@mousedown.left', 'main', 'tsx'],
  [
    'src-aux-window/components/AuxWindowTitlebar.vue',
    'class="flex h-full flex-1 items-center px-2 pt-1 pb-0.5 transition-all"',
    '@mousedown.left',
    'aux'
  ],
  [
    'src-opgg-window/opgg/OpggWindowTitlebar.vue',
    'class="box-border flex h-full flex-1 items-center pt-1 pr-1 pb-0.5 pl-2 transition-all duration-300"',
    '@mousedown.left',
    'opgg'
  ]
]

const UPDATE_BUTTON_TRANSFORMS = [
  [
    `    <NButton v-if="update" size="small" type="primary" tag="a" :href="update.url"
      target="_blank" rel="noopener noreferrer">
      {{ (english ? 'Download ' : '下载 ') + update.version }}`,
    `    <NButton v-if="update" size="small" type="primary" :loading="downloading"
      :disabled="status === 'checking'" @click="openDownload">
      {{ (english ? 'Download ' : '下载 ') + update }}`
  ],
  ["import { leagueUpdate, leagueUpdateFeed } from './league-update'\n", ''],
  [
    'const update = shallowRef<ReturnType<typeof leagueUpdate>>(null)',
    `const update = shallowRef<string | null>(null)
const downloading = ref(false)`
  ],
  [
    "import { NButton } from 'naive-ui'",
    `import { NButton } from 'naive-ui'
import { useInstance } from '@renderer-shared/shards'
import { AkariIpcRenderer } from '@renderer-shared/shards/ipc'`
  ],
  ['const app = useAppCommonStore()', 'const app = useAppCommonStore()\nconst ipc = useInstance(AkariIpcRenderer)'],
  [
    "  if (status.value === 'checking') return",
    "  if (status.value === 'checking' || downloading.value) return"
  ],
  [
    `    const response = await fetch(leagueUpdateFeed(app.version), {
      cache: 'no-store', credentials: 'omit', redirect: 'error', signal: AbortSignal.timeout(6000)
    })
    if (!response.ok) throw new Error('League manifest unavailable')
    update.value = leagueUpdate(await response.json(), app.version)`,
    `    const result = await ipc.call<{ availableVersion: string | null }>('league-update-main', 'check')
    update.value = result.availableVersion`
  ],
  [
    'onMounted(() => {',
    `async function openDownload() {
  const version = update.value
  if (!version || downloading.value || status.value === 'checking') return
  downloading.value = true
  try {
    await ipc.call('league-update-main', 'openDownload', version)
  } catch {
    update.value = null
    status.value = 'error'
  } finally {
    downloading.value = false
  }
}

onMounted(() => {`
  ]
]

function nativeRendererPlugin(sourceRoot) {
  const stagedSourcePrefix = sourceRoot.replaceAll('\\', '/') + '/'
  const ipcDirectory = join(sourceRoot, 'src/renderer-shared/shards/ipc').replaceAll('\\', '/')
  const transforms = new Map(
    Object.entries(IPC_TRANSFORMS).map(([file, patches]) => [`${ipcDirectory}/${file}`, patches])
  )
  const autoGameflow = `${stagedSourcePrefix}src/renderer/src-main-window/views/automation/AutoGameflow.vue`
  registerChampionPicker(transforms, `${stagedSourcePrefix}src/renderer/src-main-window`)
  transforms.set(autoGameflow, [
    ["import aramTeamSideMessageImage from '@renderer-shared/assets/automation/aram-team-side-message.webp'\n", ''],
    [
      `                  <img
                    :src="aramTeamSideMessageImage"
                    :alt="t('automation.gameflow.autoSendARAMTeamSideEnabled.tooltipImageAlt')"
                    class="mb-2 aspect-[1680/935] w-90 max-w-full rounded border border-black/10 object-cover dark:border-white/10"
                  />
`,
      ''
    ]
  ])
  const appCommon = join(sourceRoot, 'src/renderer-shared/shards/app-common').replaceAll('\\', '/')
  // The standalone logo has no upstream easter egg. Loading its preference
  // here would block startup on an IPC setting this app never uses.
  transforms.set(`${appCommon}/settings-sync.ts`, [
    ["import type { SettingUtilsRenderer } from '../setting-utils'\n", ''],
    ['APP_COMMON_RENDERER_NAMESPACE, MAIN_SHARD_NAMESPACE', 'MAIN_SHARD_NAMESPACE'],
    ['  settingUtils: SettingUtilsRenderer,\n', ''],
    [
      `  await settingUtils.savedGetterVue(
    APP_COMMON_RENDERER_NAMESPACE,
    'tempAkariSubscriptionInfo',
    () => store.tempAkariSubscriptionInfo,
    (v) => (store.tempAkariSubscriptionInfo = v)
  )

`,
      ''
    ]
  ])
  transforms.set(`${appCommon}/index.ts`, [
    ['syncAppCommonRendererState(this._piniaMobxUtils, this._settingUtils,',
      'syncAppCommonRendererState(this._piniaMobxUtils,']
  ])
  const piniaSync = join(sourceRoot, 'src/renderer-shared/shards/pinia-mobx-utils/pinia-state-sync.ts')
  transforms.set(piniaSync.replaceAll('\\', '/'), NATIVE_STATE_TRANSFORMS)
  const updateButton = join(sourceRoot, 'src/renderer/src-main-window/views/league-overview/LeagueUpdateButton.vue')
  transforms.set(updateButton.replaceAll('\\', '/'), UPDATE_BUTTON_TRANSFORMS)
  const resources = join(sourceRoot, 'src/renderer-shared/providers/game-resource/akari.ts')
  const imageComponent = join(sourceRoot, 'src/renderer-shared/components/LcuImage.vue')
  transforms.set(imageComponent.replaceAll('\\', '/'), [
    ['class="lcu-image" @error="handleError"', 'class="lcu-image" referrerpolicy="no-referrer" @error="handleError"'],
    ["import { ref, watchEffect } from 'vue'", `import { computed, ref, watch } from 'vue'
import { useLeagueClientStore } from '@renderer-shared/shards/league-client/store'
import { championPortraitFallback } from 'virtual:hkslgt-champion-portrait'`],
    [`const url = ref<string | null>(null)
const resources = useGameResourceProvider()

watchEffect(() => {
  if (!props.src) {
    url.value = null
    return
  }

  url.value = resources.assets.resolve(props.src)
})

const handleError = () => {
  url.value = null
}`, `const resources = useGameResourceProvider()
const client = useLeagueClientStore()
const local = computed(() => props.src ? resources.assets.resolve(props.src) : null)
const failedLocal = ref(false)
const failedRemote = ref<string | null>(null)
watch(local, () => { failedLocal.value = false; failedRemote.value = null }, { flush: 'sync' })
const url = computed(() => {
  if (!failedLocal.value) return local.value
  const remote = championPortraitFallback(local.value, client.gameData.champions)
  return remote !== failedRemote.value ? remote : null
})
const handleError = (event: Event) => {
  const failed = (event.currentTarget as HTMLImageElement).currentSrc
  if (failed !== new URL(url.value || '', document.baseURI).href) return
  if (!failedLocal.value) failedLocal.value = true
  else failedRemote.value = failed
}`]
  ])
  const resolveFromPath = 'akari://league-client${normalizeLcuPath(normalizedSource)}'
  const resolveFromUrl = 'akari://league-client${normalizeLcuPath(`${url.pathname}${url.search}${url.hash}`)}'
  transforms.set(resources.replaceAll('\\', '/'), [
    ['(?:https?:|image:', '(?:https?:|akari:|image:'],
    [resolveFromPath, resolveFromPath.replace('akari://league-client', GAME_ART_ORIGIN)],
    [resolveFromUrl, resolveFromUrl.replace('akari://league-client', GAME_ART_ORIGIN)]
  ])
  const client = join(sourceRoot, 'src/renderer-shared/shards/league-client/index.ts')
  transforms.set(client.replaceAll('\\', '/'), [
    ["import axios from 'axios'", `import axios from 'axios'\nimport { leaguePlayerRead } from '${IPC_MODULE}'`],
    ["import axiosRetry from 'axios-retry'\n", ''],
    ["import { AkariProtocolRenderer } from '../akari-protocol'\n", ''],
    ["baseURL: 'akari://league-client',\n    adapter: 'fetch',", 'adapter: leaguePlayerRead,'],
    ['    @Dep(AkariProtocolRenderer) private readonly _akariProtocol: AkariProtocolRenderer,\n', ''],
    ['    axiosRetry(this.httpClient, {\n      retries: 2\n    })\n    this._akariProtocol.installProxyRequestCancellation(this.httpClient)\n\n', ''],
    ["return new URL(uri, 'akari://league-client').href", `return new URL(uri, '${GAME_ART_ORIGIN}').href`]
  ])
  const playerTab = `${stagedSourcePrefix}src/renderer/src-main-window/views/player-tabs/components/player-tab`
  transforms.set(`${playerTab}/data/match-history.ts`, [
    ['  page: Ref<MatchHistoryPage | null>\n', '  page: Ref<MatchHistoryPage | null>\n  loadFailed: Ref<boolean>\n'],
    ['  const page = ref<MatchHistoryPage | null>(null)', '  const page = ref<MatchHistoryPage | null>(null)\n  const loadFailed = ref(false)'],
    ['    lcuCompleteGameQueue.clear()\n    isLoading.value = true\n\n    params = {', '    lcuCompleteGameQueue.clear()\n    isLoading.value = true\n    loadFailed.value = false\n\n    params = {'],
    ['    isLoading.value = true\n    const collectPageQueryParams = {', '    isLoading.value = true\n    loadFailed.value = false\n    const collectPageQueryParams = {'],
    ['      log.error(componentName, error)\n    } finally {\n      isLoading.value = false\n      collectState.value = null', '      loadFailed.value = true\n      log.error(componentName, error)\n    } finally {\n      isLoading.value = false\n      collectState.value = null'],
    ['        loadReplayMetadata(page.value.games)\n      }\n    } catch (error: any) {', '        void loadReplayMetadata(page.value.games).catch((error) => log.warn(componentName, error))\n      }\n    } catch (error: any) {\n      loadFailed.value = true'],
    ['            loadReplayMetadata(gamesToAppend)', '            void loadReplayMetadata(gamesToAppend).catch((error) => log.warn(componentName, error))'],
    ['            loadReplayMetadata(completedGamesToAppend)', '            void loadReplayMetadata(completedGamesToAppend).catch((error) => log.warn(componentName, error))'],
    ['  provide(MatchHistoryContextKey, {\n    page,', '  provide(MatchHistoryContextKey, {\n    page,\n    loadFailed,'],
    ['  return {\n    page,\n    filteredGames,', '  return {\n    page,\n    loadFailed,\n    filteredGames,']
  ])
  transforms.set(`${playerTab}/widgets/MatchHistoryList.vue`, [
    ['const {\n  page,\n  isLoading,', 'const {\n  page,\n  loadFailed,\n  isLoading,'],
    [`          rootHasCombinator
            ? t('playerTabs.profile.noFilteredMatchHistory')
            : t('playerTabs.profile.noMatchHistory')`, `          loadFailed
            ? t('playerTabs.profile.failedToLoadMatchHistoryTitle')
            : t(rootHasCombinator ? 'playerTabs.profile.noFilteredMatchHistory' : 'playerTabs.profile.noMatchHistory')`],
    ["        t('playerTabs.profile.noMatchHistory')\n", "        loadFailed ? t('playerTabs.profile.failedToLoadMatchHistoryTitle') : t('playerTabs.profile.noMatchHistory')\n"]
  ])
  const clientToolsDirectory = `${stagedSourcePrefix}src/renderer/src-main-window/views/toolkit/client`
  const clientToolkit = `${clientToolsDirectory}/Client.vue`
  const gameShortcut = `${clientToolsDirectory}/NativeGameShortcut.vue`
  transforms.set(clientToolkit, [
    ["import { resolveNativeInputStatus } from '@renderer-shared/shards/app-common/native-input-status'", "import { AkariIpcRenderer } from '@renderer-shared/shards/ipc'"],
    ["import { computed, reactive, ref, toRaw, useTemplateRef, watch } from 'vue'", "import { computed, onMounted, reactive, ref, toRaw, useTemplateRef, watch } from 'vue'"],
    ["import ShortcutSelector from '@main-window/components/ShortcutSelector.vue'", `import NativeGameShortcut from '${GAME_SHORTCUT_MODULE}'`],
    [`            :label="
              nativeInputRequiresElevation
                ? t('toolkit.client.gameClient.terminateGameClientWithShortcut.labelAdminRequired')
                : t('toolkit.client.gameClient.terminateGameClientWithShortcut.label')
            "`, `            :label="t('toolkit.client.gameClient.terminateGameClientWithShortcut.label')"`],
    [`            :label="
              nativeInputRequiresElevation
                ? t('toolkit.client.gameClient.terminateShortcut.labelAdminRequired')
                : t('toolkit.client.gameClient.terminateShortcut.label')
            "`, `            :label="t('toolkit.client.gameClient.terminateShortcut.label')"`],
    [`            :label="
              adjustWindowRequiresElevation
                ? t('toolkit.client.leagueClientUx.fixWindowMethodAOptions.labelAdminRequired')
                : t('toolkit.client.leagueClientUx.fixWindowMethodAOptions.label')
            "`, `            :label="t('toolkit.client.leagueClientUx.fixWindowMethodAOptions.label')"`],
    [`            <ShortcutSelector
              :target-id="GameClientRenderer.SHORTCUT_ID_TERMINATE_GAME_CLIENT"
              :shortcut-id="gcs.settings.terminateShortcut"
              @update:shortcut-id="(v) => gc.setTerminateShortcut(v)"
            />`, `            <NativeGameShortcut
              :shortcut-id="gcs.settings.terminateShortcut"
              :disabled="!gameHotkeyAvailable"
              :english="english"
              :save="saveGameHotkey"
            />`],
    [`                {{ t('toolkit.client.gameClient.terminateGameClientWithShortcut.description') }}
              </div>
              <div
                v-if="!as.nativeSupport.nativeInput.available"`, `                {{ t('toolkit.client.gameClient.terminateGameClientWithShortcut.description') }}
              </div>
              <div
                v-if="!as.nativeSupport.nativeInput.available || gameHotkeyError"`],
    ['@update:value="(v) => gc.setTerminateGameClientWithShortcut(v)"', '@update:value="toggleGameHotkey"'],
    [`const nativeInputStatus = computed(() =>
  resolveNativeInputStatus(as.nativeSupport.nativeInput, as.isElevated)
)
const nativeInputRequiresElevation = computed(
  () => nativeInputStatus.value === 'requires-elevation'
)
const nativeInputStatusDescription = computed(() => {
  switch (nativeInputStatus.value) {
    case 'unsupported-platform':
      return t('toolkit.client.gameClient.windowsOnlyNativeAddon')
    case 'requires-elevation':
      return t('toolkit.client.gameClient.nativeAddonRequiresAdministrator')
    case 'initialization-failed':
      return t('toolkit.client.gameClient.nativeAddonInitializationFailed')
    default:
      return ''
  }
})

const adjustWindowRequirement = computed(() => as.nativeSupport.adjustLeagueClientWindowSize)
const adjustWindowRequiresElevation = computed(
  () => adjustWindowRequirement.value.requiresElevation && !as.isElevated
)
const adjustLeagueClientWindowSizeSupported = computed(
  () => adjustWindowRequirement.value.available
)
const adjustWindowStatusDescription = computed(() =>
  adjustWindowRequirement.value.availableOnCurrentPlatform
    ? t('toolkit.client.leagueClientUx.fixWindowMethodAOptions.requiresAdministrator')
    : t('toolkit.client.leagueClientUx.fixWindowMethodAOptions.unsupportedCurrentPlatform')
)`, `const english = computed(() => as.settings.locale !== 'zh-CN')
const gameHotkeyAvailable = ref(false)
const gameHotkeyError = ref<string | null>(null)
const adjustLeagueClientWindowSizeSupported = ref(false)
const nativeInputStatusDescription = computed(() =>
  gameHotkeyError.value ||
    (english.value ? 'The Windows shortcut could not be initialized.' : 'Windows 快捷键未能初始化。')
)
const adjustWindowStatusDescription = computed(() =>
  english.value ? 'Window resize is unavailable.' : '暂时无法调整客户端窗口。'
)
const ipc = useInstance(AkariIpcRenderer)
onMounted(async () => {
  try {
    const tools = await ipc.call<{ gameHotkey: boolean; gameHotkeyError?: string; resize: boolean }>('windows-tools-main', 'capabilities')
    gameHotkeyAvailable.value = tools.gameHotkey
    gameHotkeyError.value = tools.gameHotkeyError ?? null
    adjustLeagueClientWindowSizeSupported.value = tools.resize
  } catch {
    message.error(english.value ? 'Could not load Windows tools.' : '无法读取 Windows 工具状态。')
  }
})

async function toggleGameHotkey(enabled: boolean) {
  try {
    await gc.setTerminateGameClientWithShortcut(enabled)
    gameHotkeyError.value = null
  } catch (error: any) {
    message.error(error.message ?? String(error))
  }
}

async function saveGameHotkey(shortcut: string | null) {
  await gc.setTerminateShortcut(shortcut)
  gameHotkeyError.value = null
}`],
    [`        await lc.fixWindowMethodA(toRaw(fixWindowMethodAOptions))
      } catch (error) {
        console.error(error)`, `        await lc.fixWindowMethodA(toRaw(fixWindowMethodAOptions))
        message.success(english.value ? 'Client window resized.' : '已调整客户端窗口。')
      } catch (error: any) {
        message.error(error.message ?? String(error))`]
  ])
  const opggRoot = join(sourceRoot, 'src/renderer/src-opgg-window').replaceAll('\\', '/')
  const opggView = `${opggRoot}/opgg/OpggView.vue`
  const rendererRoot = join(sourceRoot, 'src/renderer')
  const miniActivity = join(rendererRoot, 'src-aux-window/native-aux-activity.ts').replaceAll('\\', '/')
  const opggHtml = join(sourceRoot, 'src/renderer', WINDOW_ENTRIES.opgg).replaceAll('\\', '/')
  transforms.set(`${opggRoot}/App.vue`, [
    ["import { provideOpgg } from './opgg/context'\n", ''],
    ['\nprovideOpgg()\n', '\n']
  ])
  transforms.set(`${opggRoot}/shards/index.ts`, [
    ["import { OpggRenderer } from './opgg'\n", ''],
    ['manager.use(OpggRenderer)\n', '']
  ])
  // WebView2 maps Electron caption regions to native child windows. Animating
  // the player tab's loading icon in that region can stall the Win32 message pump.
  transforms.set(`${stagedSourcePrefix}src/renderer/src-main-window/App.vue`, [
    ['<style scoped>', `<style>
* {
  -webkit-app-region: initial !important;
  app-region: initial !important;
}
</style>

<style scoped>`]
  ])
  for (const [relative, anchor, gesture, label, scriptLang = 'ts'] of DRAG_REGIONS) {
    const file = join(sourceRoot, 'src/renderer', relative).replaceAll('\\', '/')
    const script = `<script setup lang="${scriptLang}">`
    const dragHandler = label === 'main'
      ? `(event: MouseEvent) => event.detail === 2
  ? nativeWindowIpc.call('window-manager-main/main-window', 'toggleMaximize')
  : nativeWindowIpc.call('window-manager-main/main-window', 'startDragging')`
      : `() => nativeWindowIpc.call('window-manager-main/${label}-window', 'startDragging')`
    transforms.set(file, [
      [anchor, `${anchor} ${gesture}="startNativeDrag"`],
      [
        script,
        `${script}
import { useInstance as useNativeShard } from '@renderer-shared/shards'
import { AkariIpcRenderer as NativeIpc } from '@renderer-shared/shards/ipc'

const nativeWindowIpc = useNativeShard(NativeIpc)
const startNativeDrag = ${dragHandler}`
      ]
    ])
  }

  return {
    name: 'mgs-league-native-renderer',
    enforce: 'pre',
    resolveId(id) {
      if (id === 'virtual:hkslgt-champion-portrait') return fileURLToPath(new URL('./champion-portrait.mjs', import.meta.url))
      if (id === IPC_MODULE) return IPC_SOURCE
      if (id === GAME_SHORTCUT_MODULE) return gameShortcut
      if (id === MINI_ACTIVITY_MODULE) return miniActivity
    },
    load(id) {
      if (id === gameShortcut) return readFileSync(GAME_SHORTCUT_COMPONENT, 'utf8')
      if (id === miniActivity) return readFileSync(MINI_ACTIVITY, 'utf8')
      const file = id.split('?', 1)[0].replaceAll('\\', '/')
      if (file.startsWith(stagedSourcePrefix)) return artworkModule(file.slice(stagedSourcePrefix.length))
    },
    transform(source, id) {
      const normalized = id.replaceAll('\\', '/')
      if (normalized.startsWith(stagedSourcePrefix)) {
        const logo = logoComponents.get(normalized.slice(stagedSourcePrefix.length))
        if (logo) return { code: `<template>${logo}</template>`, map: null }
      }
      if (normalized === opggView) {
        if (!source.includes('const { currentTab } = useOpgg()')) {
          throw new Error(`Unexpected OP.GG view in ${id}`)
        }
        return { code: readFileSync(OPGG_COMPANION, 'utf8'), map: null }
      }
      const patches = transforms.get(normalized)
      let code = normalized.startsWith(`${stagedSourcePrefix}src/`) ? useMitIconImports(source) : source
      code = patchNativeMini(code, normalized, rendererRoot, replaceOnce)
      if (patches) {
        code = code.replaceAll('\r\n', '\n')
        for (const [anchor, replacement] of patches) {
          code = replaceOnce(code, anchor, replacement, id)
        }
        if (normalized === clientToolkit) {
          const nativeGate = 'as.nativeSupport.nativeInput.available'
          if (code.split(nativeGate).length !== 6) {
            throw new Error(`Unexpected native input gates in ${id}`)
          }
          code = code.replaceAll(nativeGate, 'gameHotkeyAvailable')
        }
      }
      if (code !== source) return { code, map: null }
    },
    transformIndexHtml(html, { filename }) {
      const opgg = filename.replaceAll('\\', '/') === opggHtml
      if (opgg) {
        html = replaceOnce(html, "default-src https: ws: 'self' data: blob: ;", "default-src 'self' data: blob:;", filename)
        html = replaceOnce(html, "connect-src https: ws: 'self';", "connect-src 'self';", filename)
      }
      const imageSources = opgg ? "data: blob: 'self'" : "https: akari: data: blob: 'self'"
      return replaceOnce(
        html,
        'connect-src ',
        `img-src ${imageSources} ${GAME_ART_ORIGIN}; connect-src ipc: http://ipc.localhost `,
        filename
      )
    }
  }
}

const DEVELOPMENT_PLUGINS = new Set([
  'vite-plugin-inspect',
  'vite-plugin-vue-inspector',
  'vite-plugin-vue-inspector:post',
  'vite-plugin-vue-devtools'
])

export function createNativeRendererConfig(renderer, sourceRoot, outDir) {
  const root = join(sourceRoot, 'src/renderer')
  const input = Object.fromEntries(
    Object.entries(WINDOW_ENTRIES).map(([label, filename]) => [label, join(root, filename)])
  )
  const { rollupOptions, rolldownOptions, ...buildOptions } = renderer.build ?? {}
  const plugins = (renderer.plugins ?? [])
    .flat(Infinity)
    .filter((plugin) => !DEVELOPMENT_PLUGINS.has(plugin?.name))

  return {
    ...renderer,
    configFile: false,
    root,
    base: './',
    plugins: [nativeRendererPlugin(sourceRoot), ...plugins],
    build: {
      ...buildOptions,
      outDir,
      emptyOutDir: false,
      rolldownOptions: { ...rollupOptions, ...rolldownOptions, input }
    }
  }
}

async function buildRenderer(args) {
  if (args.length !== 2) {
    throw new Error(
      'Usage: node build.mjs <staged League source directory> <frontendDist directory>'
    )
  }
  const sourceRoot = await realpath(args[0])
  const outDir = resolve(args[1])
  const outputFromSource = relative(sourceRoot, outDir)
  if (
    !isAbsolute(outputFromSource) &&
    outputFromSource !== '..' &&
    !outputFromSource.startsWith(`..${sep}`)
  ) {
    throw new Error(
      'The native renderer output directory must be outside the staged League source.'
    )
  }
  // The desktop bundle embeds this full directory. Reusing one with old hashed
  // chunks would retain obsolete assets in each new installation.
  const oldFiles = await readdir(outDir).catch((error) => {
    if (error.code === 'ENOENT') return []
    throw error
  })
  if (oldFiles.length) {
    throw new Error('The native renderer output must be a new or empty directory.')
  }

  process.chdir(sourceRoot)
  process.env.NODE_ENV = 'production'
  const sourceRequire = createRequire(join(sourceRoot, 'package.json'))
  const vite = await import(pathToFileURL(sourceRequire.resolve('vite')).href)
  // The pinned config uses the CommonJS directory binding for HTML entry paths.
  // The runner can supply it without writing a bundled config into the dependency cache.
  const { module: loaded } = await vite.runnerImport(join(sourceRoot, 'electron.vite.config.ts'), {
    root: sourceRoot,
    define: { __dirname: JSON.stringify(sourceRoot) }
  })
  if (!loaded.default?.renderer) {
    throw new Error('The staged League source has no renderer configuration.')
  }

  await vite.build(createNativeRendererConfig(loaded.default.renderer, sourceRoot, outDir))
  await Promise.all(Object.values(WINDOW_ENTRIES).map((filename) => stat(join(outDir, filename))))
  await copyFile(TOOLBOX_ICON, join(outDir, 'league.ico'))
  await writeFile(join(outDir, 'resg-display.js'), resgDisplayProgram(), 'utf8')
  console.log(`Native League renderer built at ${outDir}`)
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  await buildRenderer(process.argv.slice(2))
}
