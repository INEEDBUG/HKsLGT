import { join } from 'node:path'

export const MINI_ACTIVITY_MODULE = 'virtual:mgs-league-mini-activity'

const scopedClient = [
  "import { useLeagueClientStore } from '@renderer-shared/shards/league-client/store'",
  `import { useNativeAuxClient, useNativeAuxActivity } from '${MINI_ACTIVITY_MODULE}'`
]

const miniPatches = {
  'views/Indicator.vue': [
    ['  <KeepAlive>', '  <div class="aux-activity h-full" :data-phase="view?.phase"><KeepAlive>'],
    ['  </KeepAlive>', '  </KeepAlive></div>']
  ],
  'views/Lounge.vue': [
    ['v-else-if="lcs.matchmaking.readyCheck?.playerResponse === \'Accepted\'">\n          <span class="mb-2', 'v-else-if="lcs.matchmaking.readyCheck?.playerResponse === \'Accepted\'">\n          <span class="ready-response mb-2'],
    ['          v-if="lcs.matchmaking.search"', '          class="queue-time"\n          v-if="lcs.matchmaking.search"'],
    ['          class="mb-2 text-[13px] text-gray-500 dark:text-gray-400"\n          class="queue-time"', '          class="queue-time mb-2 text-[13px] text-gray-500 dark:text-gray-400"'],
    [':loading="isCancelingSearching"', ':loading="pending"'],
    ['@click="() => handleCancelSearching()"', 'data-action="cancel-matchmaking" @click="() => handleCancelSearching()"'],
    ...[10, 12].flatMap((indent) => [
      ['primary', 'Accept', 'accept'], ['warning', 'Decline', 'decline']
    ].map(([type, action, selector]) => {
      const prefix = '\n' + ' '.repeat(indent) + `<NButton type="${type}" secondary size="tiny"`
      const click = ` @click="() => handle${action}()">`
      return [prefix + click, prefix + ` :disabled="pending" data-action="${selector}-ready-check"` + click]
    }))
  ],
  'components/BenchChampionsMini.vue': [
    [scopedClient[0], `import { useNativeAuxClient, useNativeAuxActivity, useNativeAramBalance } from '${MINI_ACTIVITY_MODULE}'`],
    ['const lcs = useLeagueClientStore()', 'const lcs = useNativeAuxClient()\nconst { view, pending, run } = useNativeAuxActivity()\nuseNativeAramBalance()'],
    ["import { LeagueClientRenderer } from '@renderer-shared/shards/league-client'\n", ''],
    ['const lc = useInstance(LeagueClientRenderer)\n', ''],
    ['class="size-9 cursor-default rounded-full', 'class="current-champion size-9 cursor-default rounded-full'],
    [':champion-id="lcs.champSelect.currentChampion || -1"', ':champion-id="lcs.champSelect.currentChampion || -1" :data-champion-id="lcs.champSelect.currentChampion"'],
    ['<div class="grid grid-cols-5 gap-1">', '<div class="bench grid grid-cols-5 gap-1">'],
    [':disabled="rerollsRemaining === 0 || isRerolling"\n            size="tiny"', ':disabled="pending || rerollsRemaining === 0 || isRerolling"\n            size="tiny"'],
    [':disabled="rerollsRemaining === 0 || isRerolling"\n            @click="() => handleReroll(true)"', ':disabled="pending || rerollsRemaining === 0 || isRerolling"\n            @click="() => handleReroll(true)"'],
    ['            :title="\n              t(\'auxWindow.championBench.reroll\'', '            data-action="reroll"\n            :title="\n              t(\'auxWindow.championBench.reroll\''],
    ['            :title="\n              t(\'auxWindow.championBench.charity\'', '            data-action="charity-reroll"\n            :title="\n              t(\'auxWindow.championBench.charity\''],
    [`                  'cursor-not-allowed grayscale-[0.8]':
                    !isChampionSwappable(c.championId) || !canUseBench`, `                  'cursor-not-allowed': pending || !canUseBench || !isChampionSwappable(c.championId),
                  'grayscale-[0.8]':
                    (view?.pickableChampionIds != null && !isChampionSwappable(c.championId)) ||
                    (view?.timerPhase && !canUseBench)`],
    [`              :champion-id="c.championId"
              @click="() => handleBenchSwapOrPick(c.championId)"
              @click.right="handleBenchSwapOrPick(c.championId, false)"`, `              :champion-id="c.championId"
              :data-champion-id="c.championId"
              :aria-label="lcs.gameData.championName(c.championId)"
              :aria-disabled="pending || !canUseBench || !isChampionSwappable(c.championId)"
              :tabindex="!pending && canUseBench && isChampionSwappable(c.championId) ? 0 : -1"
              role="button"
              @click="() => handleBenchSwapOrPick(c.championId)"
              @keydown.enter.prevent="handleBenchSwapOrPick(c.championId)"
              @keydown.space.prevent="handleBenchSwapOrPick(c.championId)"
              @contextmenu.prevent="handleBenchSwapOrPick(c.championId, false)"`],
    ['  if (!isChampionSwappable(championId)) {', '  if (!canUseBench.value || !isChampionSwappable(championId)) {'],
    ["        await lc.api.champSelect.pickOrBan(championId, complete, 'pick', firstPickAction.id)", "        await run('chooseChampion', championId, complete)"],
    ['      await lc.api.champSelect.benchSwap(championId)', "      await run('swapBenchChampion', championId)"],
    [`    const prevId = lcs.champSelect.currentChampion

    await lc.api.champSelect.reroll()

    // 使用一个简短的延时来实现，simple workaround
    if (grabBack && prevId !== null) {
      window.setTimeout(async () => {
        if (combinedChampions.value) {
          await handleBenchSwapOrPick(prevId)
        }
      }, 25)
    }`, "    await run(grabBack ? 'charityReroll' : 'reroll')"]
  ],
  'components/ChampSelectActions.vue': [
    [scopedClient[0], `import { useNativeAuxClient } from '${MINI_ACTIVITY_MODULE}'`],
    ['const lcs = useLeagueClientStore()', 'const lcs = useNativeAuxClient()']
  ],
  'components/LoungeOperations.vue': [
    scopedClient,
    ['const lcs = useLeagueClientStore()', 'const lcs = useNativeAuxClient()\nconst { pending, withFeedback } = useNativeAuxActivity()'],
    ...Object.entries({
      autoAcceptEnabled: ['val', 'setAutoAcceptEnabled', 'val'],
      autoMatchmakingEnabled: ['val', 'setAutoMatchmakingEnabled', 'val'],
      autoMatchmakingMinimumMembers: ['val', 'setAutoMatchmakingMinimumMembers', 'val || 1'],
      autoMatchmakingDelaySeconds: ['value', 'setAutoMatchmakingDelaySeconds', 'value || 0'],
      autoMatchmakingWaitForInvitees: ['val', 'setAutoMatchmakingWaitForInvitees', 'val']
    }).flatMap(([key, [argument, action, value]]) => [
      [`:value="agfs.settings.${key}"`, `:disabled="pending" data-setting="${key}" :value="agfs.settings.${key}"`],
      [`@update:value="(${argument}) => agf.${action}(${value})"`, `@update:value="(${argument}) => withFeedback(() => agf.${action}(${value}))"`]
    ])
  ],
  'components/ChampSelectOperations.vue': [
    ['@positive-click="dodgeLoop"', `@positive-click="() => run('leaveGame')"`],
    [':disabled="isLoopingDodge"\n        :negative-text', ':disabled="pending"\n        :negative-text'],
    [':disabled="isLoopingDodge">', ':disabled="pending" data-action="leave-game">'],
    ["            <template v-if=\"iteration\">({{ iteration >= 1000 ? '999+' : iteration }})</template>\n", ''],
    [`      <NButton
        v-if="isLoopingDodge"
        size="tiny"
        type="warning"
        secondary
        @click="isLoopingDodge = false"
      >
        {{ t('auxWindow.champSelect.operations.dodge.cancel') }}
      </NButton>
`, ''],
    ['@update:value="(val) => as.setTemporarilyDisabled(val)"', `:disabled="pending" data-action="pause-auto-select" @update:value="(val) => run('pauseAutoSelect', val)"`]
  ],
  'components/SkinSelectionMini.vue': [
    ['v-if="aws.settings.showSkinSelector && lcs.champSelect.currentChampion && skinOptions.length"', 'v-if="showSkinSelector && skinOptions.length"'],
    [':value="lcs.champSelect.skinSelectorInfo?.selectedSkinId"', ':value="view?.selectedSkinId ?? undefined"'],
    [':loading="isSettingSkin"', ':loading="pending" :disabled="pending || view?.skinSelectionDisabled" data-action="select-skin"']
  ]
}

const setupScripts = {
  'views/Indicator.vue': ['const currentPage = computed(', `
import { computed } from 'vue'
import { provideNativeAuxActivity } from '${MINI_ACTIVITY_MODULE}'

import ChampSelect from './ChampSelect.vue'
import Lounge from './Lounge.vue'
import Placeholder from './Placeholder.vue'

const { view } = provideNativeAuxActivity()
const currentPage = computed(() => {
  if (!view.value) return 'placeholder'
  return view.value.phase === 'ChampSelect' ? 'champ-select' : 'lounge'
})
`],
  'views/Lounge.vue': ["const handleAccept = async () =>", `
import LoungeOperations from '@aux-window/components/LoungeOperations.vue'
import LcuImage from '@renderer-shared/components/LcuImage.vue'
import { useTranslation } from 'i18next-vue'
import { NButton } from 'naive-ui'
import { useNativeAuxLounge } from '${MINI_ACTIVITY_MODULE}'

const { t } = useTranslation()
const {
  lcs, agfs, pending, penaltyTime, penaltyTotalTime, willAcceptIn, willSearchMatchIn,
  handleAccept, handleDecline, handleCancelAutoAccept, handleCancelAutoSearchMatch, handleCancelSearching
} = useNativeAuxLounge()

const formatMapModeText = () => {
  const session = lcs.gameflow.session
  const queue = session?.gameData.queue.name || t('auxWindow.lounge.panel.gameMode')
  const map = session?.map.name || t('auxWindow.lounge.panel.map')
  return map === queue ? queue : queue + ' · ' + map
}

const formatMatchmakingSearchText = (search: NonNullable<typeof lcs.matchmaking.search>) => {
  const elapsed = search.timeInQueue.toFixed(1) + ' s'
  const wait = penaltyTime.value
  const penalty = penaltyTotalTime.value
  if (wait != null && ((penalty ?? 0) > 0 || wait > 0)) {
    const remaining = t('auxWindow.lounge.panel.wait') + ' ' + Number(wait.toFixed(1)) + ' s'
    return penalty != null && penalty > 0 ? remaining + ' (' + Number(penalty.toFixed(1)) + ' s)' : remaining
  }
  const limit = agfs.settings.autoMatchmakingRematchStrategy === 'fixed-duration'
    ? ' (' + t('auxWindow.lounge.panel.atMost') + ' ' + agfs.settings.autoMatchmakingRematchFixedDuration + ' s)'
    : ''
  return elapsed + limit + (search.estimatedQueueTime == null ? '' : ' / ' + search.estimatedQueueTime.toFixed(1) + ' s')
}
`],
  'components/ChampSelectOperations.vue': ['const dodgeLoop = async () =>', `
import { useAutoSelectStore } from '@renderer-shared/shards/auto-select/store'
import { useTranslation } from 'i18next-vue'
import { NButton, NCard, NPopconfirm, NSwitch } from 'naive-ui'
import { useNativeAuxActivity } from '${MINI_ACTIVITY_MODULE}'

const { t } = useTranslation()
const as2 = useAutoSelectStore()
const isCustomGame = false
const { pending, run } = useNativeAuxActivity()
`],
  'components/SkinSelectionMini.vue': ['getCarouselSkins()', `
import LcuImage from '@renderer-shared/components/LcuImage.vue'
import { useAuxWindowStore } from '@renderer-shared/shards/window-manager/store'
import { useTranslation } from 'i18next-vue'
import { NCard, NSelect, type SelectRenderLabel, type SelectRenderTag } from 'naive-ui'
import { computed } from 'vue'
import { useNativeAuxActivity } from '${MINI_ACTIVITY_MODULE}'

const { t } = useTranslation()
const { view, pending, run } = useNativeAuxActivity()
const aws = useAuxWindowStore()
const showSkinSelector = computed(() => aws.settings.showSkinSelector && view.value?.skinSelectorAvailable === true)
const skinOptions = computed(() => (view.value?.skins ?? []).map((skin) => ({
  label: skin.name, value: skin.id, previewPath: skin.previewPath
})))
const renderLabel: SelectRenderLabel = (option) => (
  <div class="flex items-center justify-center gap-2 px-1 py-0.5">
    {typeof option.previewPath === 'string' && <LcuImage src={option.previewPath} class="h-5 w-9 rounded-[2px] object-contain" />}
    <div class="overflow-hidden text-xs text-ellipsis whitespace-nowrap text-black/70 dark:text-white/70" title={String(option.label)}>{option.label}</div>
  </div>
)
const renderTag: SelectRenderTag = ({ option }) => <span class="text-xs text-black/70 dark:text-white/70">{option.label}</span>
const handleSetSkin = (skinId: number) => run('selectSkin', skinId)
`]
}

export function patchNativeMini(code, filename, rendererRoot, replaceOnce) {
  const root = join(rendererRoot, 'src-aux-window').replaceAll('\\', '/') + '/'
  if (!filename.startsWith(root)) return code
  const relative = filename.slice(root.length)
  const patches = miniPatches[relative]
  if (!patches) return code
  code = code.replaceAll('\r\n', '\n')
  for (const [anchor, replacement] of patches) {
    code = replaceOnce(code, anchor, replacement, filename)
  }
  if (setupScripts[relative]) {
    const [anchor, script] = setupScripts[relative]
    const block = code.match(/(<script setup lang="tsx?">)([\s\S]*?)<\/script>/)
    if (!block?.[2].includes(anchor)) throw new Error(`Unexpected Mini component script in ${filename}`)
    code = replaceOnce(code, block[0], `${block[1]}${script}</script>`, filename)
  }
  return code
}
