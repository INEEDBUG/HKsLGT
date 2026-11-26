import { useGameResourceProvider } from '@renderer-shared/providers/game-resource/context'
import { useInstance } from '@renderer-shared/shards'
import { useAppCommonStore } from '@renderer-shared/shards/app-common/store'
import { AutoGameflowRenderer } from '@renderer-shared/shards/auto-gameflow'
import { useAutoGameflowStore } from '@renderer-shared/shards/auto-gameflow/store'
import { useExtraAssetsStore } from '@renderer-shared/shards/extra-assets/store'
import { AkariIpcRenderer } from '@renderer-shared/shards/ipc'
import { useLeagueClientStore } from '@renderer-shared/shards/league-client/store'
import { PiniaMobxUtilsRenderer } from '@renderer-shared/shards/pinia-mobx-utils'
import type { OpggAramBalanceItem } from '@shared/types/opgg'
import { useIntervalFn } from '@vueuse/core'
import { useMessage } from 'naive-ui'
import { computed, inject, onMounted, provide, ref, shallowReactive, watch, type InjectionKey, type Ref } from 'vue'

const NAMESPACE = 'aux-activity-main'
const ARAM_ICON = '/lol-game-data/assets/content/src/LeagueClient/GameModeAssets/ARAM/img/game-select-icon-hover.png'
// Riot's map asset catalog for map 12 specifies the same queue badge for ARAM and KIWI.
const PUBLIC_ARAM_ICON = 'https://raw.communitydragon.org/16.19/plugins/rcp-be-lol-game-data/global/default/content/src/leagueclient/gamemodeassets/aram/img/game-select-icon-hover.png'
const OPGG_ARAM_BALANCE = 'https://lol-api-champion.op.gg/api/contents/aram-balance'
const BALANCE_VALUES = [
  'attack_speed', 'damage_dealt', 'damage_taken', 'cooldown_reduction', 'healing',
  'tenacity', 'shield_amount', 'energy_regen', 'area_of_effect_damage'
] as const satisfies readonly (keyof OpggAramBalanceItem)[]
let balanceRequested = false

interface LocalAction {
  id: number
  type: string
  championId: number
  completed: boolean
  isInProgress: boolean
}

interface AuxView {
  phase: 'Lobby' | 'Matchmaking' | 'ReadyCheck' | 'ChampSelect'
  gameMode: 'ARAM' | 'KIWI'
  queueName?: string | null
  mapName?: string | null
  modeIconPath?: string | null
  queueSeconds: number | null
  estimatedSeconds: number | null
  penaltySeconds?: number | null
  penaltyTotalSeconds?: number | null
  readyResponse: 'Accepted' | 'Declined' | null
  championId: number | null
  bench: number[]
  pickableChampionIds?: number[] | null
  subsetChampionIds?: number[]
  benchEnabled?: boolean
  timerPhase?: string
  localActions?: LocalAction[]
  selectionActionable?: boolean
  localPlayerCellId?: number
  rerollsRemaining?: number
  allowRerolling?: boolean
  allowSubsetChampionPicks?: boolean
  skinSelectorAvailable?: boolean
  skinSelectionDisabled?: boolean
  selectedSkinId?: number | null
  skins?: Array<{ id: number, name: string, previewPath?: string | null, isChroma?: boolean }>
}

export function provideNativeAuxActivity() {
  const ipc = useInstance(AkariIpcRenderer)
  const sync = useInstance(PiniaMobxUtilsRenderer)
  const app = useAppCommonStore()
  const message = useMessage()
  const state = shallowReactive<{ view: AuxView | null }>({ view: null })
  const view = computed(() => state.view)
  const chinese = computed(() => app.settings.locale.startsWith('zh'))
  const pending = ref(false)
  const selectionBlocked = computed(() => view.value?.phase === 'ChampSelect' && view.value.selectionActionable !== true)

  onMounted(async () => {
    try {
      await sync.sync(NAMESPACE, 'state', state)
    } catch {
      message.warning(chinese.value ? '暂时无法读取大乱斗状态。' : 'Could not read the ARAM status.')
    }
  })

  async function withFeedback(action: () => Promise<unknown>) {
    if (!view.value || pending.value || selectionBlocked.value) return false
    pending.value = true
    try {
      await action()
      return true
    } catch (error) {
      const reason = error as { message?: string } | null
      message.warning(reason?.message ?? (chinese.value ? '客户端操作失败。' : 'The client action failed.'))
      return false
    } finally {
      pending.value = false
    }
  }

  function run(action: 'cancelMatchmaking' | 'acceptReadyCheck' | 'declineReadyCheck' | 'reroll' | 'charityReroll' | 'leaveGame'): Promise<boolean>
  function run(action: 'swapBenchChampion' | 'selectSkin', id: number): Promise<boolean>
  function run(action: 'chooseChampion', championId: number, complete: boolean): Promise<boolean>
  function run(action: 'pauseAutoSelect', paused: boolean): Promise<boolean>
  function run(action: string, ...args: Array<number | boolean>) {
    return withFeedback(async () => {
      const result = await ipc.call<{ rerolled?: boolean, grabbedBack?: boolean, swapReason?: string } | null>(NAMESPACE, action, ...args)
      if (action === 'charityReroll' && result?.rerolled && !result.grabbedBack) {
        const notice = chinese.value ? '已重随，未换回原英雄。' : 'Rerolled. The original champion was not recovered.'
        message.warning(result.swapReason ? `${notice} ${result.swapReason}` : notice)
      }
    })
  }

  const activity = { view, pending, selectionBlocked, chinese, run, withFeedback }
  provide(nativeAuxActivity, activity)
  return activity
}

const nativeAuxActivity: InjectionKey<ReturnType<typeof provideNativeAuxActivity>> = Symbol('NativeAuxActivity')

export function useNativeAuxActivity() {
  const activity = inject(nativeAuxActivity)
  if (!activity) throw new Error('The native Mini activity provider is missing.')
  return activity
}

function isBalanceRow(value: unknown): value is OpggAramBalanceItem {
  if (!value || typeof value !== 'object') return false
  const row = value as Partial<OpggAramBalanceItem>
  return typeof row.champion_id === 'number' && Number.isSafeInteger(row.champion_id) && row.champion_id > 0 &&
    typeof row.default === 'boolean' && BALANCE_VALUES.every((field) => Number.isFinite(row[field]))
}

export function useNativeAramBalance() {
  const { view } = useNativeAuxActivity()
  const { opgg } = useExtraAssetsStore()
  watch(() => view.value?.phase === 'ChampSelect' && view.value.gameMode === 'ARAM', async (active) => {
    if (!active || opgg.aramBalance || balanceRequested) return
    balanceRequested = true
    try {
      const response = await fetch(OPGG_ARAM_BALANCE, {
        credentials: 'omit', referrerPolicy: 'no-referrer', redirect: 'error',
        cache: 'no-store', signal: AbortSignal.timeout(4000)
      })
      if (!response.ok) return
      const result = await response.json() as { data?: unknown } | null
      if (Array.isArray(result?.data) && result.data.length <= 1000) {
        opgg.aramBalance = result.data.filter(isBalanceRow)
      }
    } catch {
      // Balance tooltips are supplementary. Choosing a champion never waits for OP.GG.
    }
  }, { immediate: true })
}

// Present the scoped native projection in the shape used by the original Mini components.
export function useNativeAuxClient() {
  const { view, chinese } = useNativeAuxActivity()
  const client = useLeagueClientStore()
  return {
    gameData: client.gameData,
    get gameflow() {
      const current = view.value
      if (!current) return { phase: null, session: null }
      const gameName = current.gameMode === 'KIWI'
        ? (chinese.value ? '海克斯大乱斗' : 'ARAM Mayhem')
        : (chinese.value ? '极地大乱斗' : 'ARAM')
      return {
        phase: current.phase,
        session: {
          gameData: { queue: { gameMode: current.gameMode, name: current.queueName || gameName }, isCustomGame: false },
          map: { name: current.mapName }
        }
      }
    },
    get matchmaking() {
      const current = view.value
      return {
        readyCheck: current?.phase === 'ReadyCheck' ? { playerResponse: current.readyResponse } : null,
        search: current?.queueSeconds == null ? null : {
          timeInQueue: current.queueSeconds,
          estimatedQueueTime: current.estimatedSeconds
        }
      }
    },
    get champSelect() {
      const current = view.value?.phase === 'ChampSelect' ? view.value : null
      const cell = current?.localPlayerCellId ?? -1
      return {
        currentChampion: current?.championId ?? null,
        currentPickableChampionIds: new Set(current?.pickableChampionIds ?? []),
        session: current && {
          benchEnabled: current.benchEnabled ?? current.bench.length > 0,
          benchChampions: current.bench.map((championId) => ({ championId, isPriority: false })),
          allowRerolling: current.allowRerolling ?? false,
          allowSubsetChampionPicks: current.allowSubsetChampionPicks ?? false,
          rerollsRemaining: current.rerollsRemaining ?? 0,
          localPlayerCellId: cell,
          actions: (current.localActions ?? []).map((action) => [{ ...action, actorCellId: cell }]),
          timer: { phase: current.timerPhase ?? '' }
        }
      }
    },
    get lobbyTeamBuilder() {
      return { champSelect: { subsetChampionList: view.value?.subsetChampionIds ?? [] } }
    }
  }
}

export function useNativeModeIcon(view: Readonly<Ref<AuxView | null>>) {
  const { assets } = useGameResourceProvider()
  const gameMode = computed(() => view.value?.gameMode)
  const local = computed(() => gameMode.value === 'ARAM' || gameMode.value === 'KIWI'
    ? assets.resolve(view.value?.modeIconPath || ARAM_ICON)
    : null)
  const failedLocal = ref(false)
  const failedPublic = ref(false)
  watch([gameMode, local], () => {
    failedLocal.value = false
    failedPublic.value = false
  }, { flush: 'sync' })

  const modeIconSrc = computed(() => {
    if (gameMode.value !== 'ARAM' && gameMode.value !== 'KIWI') return null
    if (local.value && !failedLocal.value) return local.value
    return failedPublic.value ? null : PUBLIC_ARAM_ICON
  })
  function handleModeIconError(event: Event) {
    const failed = (event.currentTarget as HTMLImageElement).currentSrc
    if (failed !== modeIconSrc.value) return
    if (failed === PUBLIC_ARAM_ICON) failedPublic.value = true
    else failedLocal.value = true
  }

  return { modeIconSrc, handleModeIconError }
}

export function useNativeAuxLounge() {
  const activity = useNativeAuxActivity()
  const { view, run, withFeedback } = activity
  const lcs = useNativeAuxClient()
  const agfs = useAutoGameflowStore()
  const agf = useInstance(AutoGameflowRenderer)
  const now = ref(Date.now())
  const { pause, resume } = useIntervalFn(() => { now.value = Date.now() }, 100, {
    immediate: false, immediateCallback: true
  })
  watch(() => agfs.willAcceptAt > 0 || agfs.willSearchMatch, (active) => {
    if (active) resume()
    else pause()
  }, { immediate: true })

  return {
    ...activity, lcs, agfs, ...useNativeModeIcon(view),
    willAcceptIn: computed(() => Math.max(0, (agfs.willAcceptAt - now.value) / 1000)),
    willSearchMatchIn: computed(() => Math.max(0, (agfs.willSearchMatchAt - now.value) / 1000)),
    penaltyTime: computed(() => view.value?.penaltySeconds ?? null),
    penaltyTotalTime: computed(() => view.value?.penaltyTotalSeconds ?? null),
    handleAccept: () => run('acceptReadyCheck'),
    handleDecline: () => run('declineReadyCheck'),
    handleCancelAutoAccept: () => withFeedback(() => agf.cancelAutoAccept()),
    handleCancelAutoSearchMatch: () => withFeedback(async () => {
      await agf.setAutoMatchmakingEnabled(false)
      await agf.cancelAutoMatchmaking()
    }),
    handleCancelSearching: async () => {
      if (agfs.settings.autoMatchmakingEnabled && !await withFeedback(() => agf.setAutoMatchmakingEnabled(false))) {
        return
      }
      await run('cancelMatchmaking')
    }
  }
}
