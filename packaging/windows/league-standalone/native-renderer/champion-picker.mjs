// Adapt the pinned picker's three components while retaining its transfer list and drag ordering.
export function registerChampionPicker(transforms, renderer) {
  const picker = renderer + '/views/automation/components/ordered-champion-list'
  transforms.set(picker + '/OrderedChampionList.vue', [
    ["const champions = defineModel<number[]>('champions', { default: () => [] })",
      "const savedChampions = defineModel<number[]>('champions', { default: () => [] })"],
    ['const lcs = useLeagueClientStore()', `const lcs = useLeagueClientStore()
const isClassic = (id: number) => lcs.gameData.champions[id]?.alias?.startsWith('Jade_') === true
const champions = computed({
  get: () => savedChampions.value.filter(id => !isClassic(id)),
  set: (ids: number[]) => { savedChampions.value = ids.filter(id => !isClassic(id)) }
})`],
    ['Object.values(lcs.gameData.champions).map', 'Object.values(lcs.gameData.champions).filter(c => !isClassic(c.id)).map'],
    ['const { positionMap } = useRecommendedChampionPositions()',
      'const { positionMap, failed, retry } = useRecommendedChampionPositions(show)'],
    [`  if (positionMap.value && selectedPosition.value) {
    const position = positionMap.value[selectedPosition.value]
    if (position && !position.has(b.value)) {
      return false
    }
  }`, `  if (selectedPosition.value && !positionMap.value?.[selectedPosition.value]?.has(b.value)) {
    return false
  }`],
    ['const renderPositionFilter = () => {', `const renderPositionFilter = () => {
  if (failed.value) {
    return <NButton size="tiny" onClick={retry}>{t('automation.orderedChampionList.retryPositions')}</NButton>
  }`]
  ])

  transforms.set(renderer + '/composables/useRecommendedChampionPositions.ts', [
    ["import { computed, readonly, shallowRef, watch } from 'vue'",
      "import { computed, readonly, ref, shallowRef, watch, type Ref } from 'vue'"],
    ['export function useRecommendedChampionPositions() {', 'export function useRecommendedChampionPositions(open: Ref<boolean>) {'],
    [`  watch(
    () => lcs.isConnected,
    async (isConnected) => {
      if (isConnected) {
        data.value = (await lc.api.perks.getRecommendedChampionPositions()).data
      } else {
        data.value = null
      }
    },`, `  const attempt = ref(0)
  const failed = ref(false)
  watch(
    [() => open.value && lcs.isConnected, attempt],
    async ([active], _, onCleanup) => {
      data.value = null
      failed.value = false
      if (!active) return
      const controller = new AbortController()
      onCleanup(() => controller.abort())
      try {
        const response = await lc.api.perks.getRecommendedChampionPositions({ signal: controller.signal })
        if (!controller.signal.aborted) data.value = response.data
      } catch {
        if (!controller.signal.aborted) failed.value = true
      }
    },`],
    ['const map: Record<string, Set<number>> = {}', 'const map: Record<string, Set<number>> = Object.create(null)'],
    ['for (const position of recommendedPositions) {', 'for (const lane of recommendedPositions) {\n        const position = lane.toUpperCase()'],
    ['    data: readonly(data),', '    failed: readonly(failed),\n    retry: () => { attempt.value++ },\n    data: readonly(data),']
  ])

  transforms.set(picker + '/PositionFilter.vue', [
    ['    <div\n      class=', '    <button type="button"\n      class='],
    ['      :key="position"', `      :key="position"
      :aria-label="t('positions.' + position, { ns: 'common' })"
      :title="t('positions.' + position, { ns: 'common' })"
      :aria-pressed="positionModel === position"`],
    ['    </div>\n  </div>', '    </button>\n  </div>'],
    ['const positionModel =', "import { useTranslation } from 'i18next-vue'\n\nconst { t } = useTranslation()\nconst positionModel ="]
  ])
}
