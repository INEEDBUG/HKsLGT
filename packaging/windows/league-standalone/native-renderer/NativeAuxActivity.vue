<template>
  <div class="aux-activity" :data-phase="view?.phase">
    <template v-if="view">
      <header>
        <p class="queue-name">{{ modeName }}</p>
        <h1>{{ title }}</h1>
      </header>

      <template v-if="view.phase === 'ChampSelect'">
        <NCard v-if="view.championId" size="small">
          <div class="current-champion" :data-champion-id="view.championId">
            <LcuImage :src="championIconUri(view.championId)" class="champion-icon" aria-hidden="true" />
            <div>
              <p>{{ chinese ? '当前英雄' : 'Current champion' }}</p>
              <strong>{{ championName(view.championId) }}</strong>
            </div>
          </div>
        </NCard>
        <p v-else>{{ chinese ? '等待客户端分配英雄' : 'Waiting for the client to assign a champion' }}</p>
        <p v-if="view.remainingSeconds !== null" class="remaining-time">
          {{ chinese ? '选人剩余' : 'Time left' }} {{ duration(view.remainingSeconds) }}
        </p>
        <section v-if="view.bench.length" class="bench">
          <h2>{{ chinese ? '备战席' : 'Champion bench' }}</h2>
          <ul>
            <li v-for="id in view.bench" :key="id" :data-champion-id="id">
              <LcuImage :src="championIconUri(id)" class="champion-icon" aria-hidden="true" />
              <span>{{ championName(id) }}</span>
            </li>
          </ul>
        </section>
      </template>

      <dl v-else class="room-details">
        <template v-if="view.partySize !== null">
          <dt>{{ chinese ? '房间人数' : 'Party size' }}</dt>
          <dd class="party-size">{{ view.partySize }}</dd>
        </template>
        <template v-if="view.phase === 'Matchmaking' && view.queueSeconds !== null">
          <dt>{{ chinese ? '已匹配' : 'Time in queue' }}</dt>
          <dd class="queue-time">{{ duration(view.queueSeconds) }}</dd>
        </template>
        <template v-if="view.phase === 'Matchmaking' && view.estimatedSeconds !== null">
          <dt>{{ chinese ? '预计等待' : 'Estimated wait' }}</dt>
          <dd>{{ duration(view.estimatedSeconds) }}</dd>
        </template>
        <template v-if="view.phase === 'ReadyCheck' && view.readyResponse !== null">
          <dt>{{ chinese ? '我的确认状态' : 'Your response' }}</dt>
          <dd class="ready-response">{{ responseName }}</dd>
        </template>
      </dl>
    </template>
    <p v-else role="status">{{ syncError || (chinese ? '等待大乱斗房间或选人状态' : 'Waiting for an ARAM lobby or champion select') }}</p>
  </div>
</template>

<script setup lang="ts">
import LcuImage from '@renderer-shared/components/LcuImage.vue'
import { useInstance } from '@renderer-shared/shards'
import { useAppCommonStore } from '@renderer-shared/shards/app-common/store'
import { championIconUri } from '@renderer-shared/shards/league-client/game-data-assets'
import { useLeagueClientStore } from '@renderer-shared/shards/league-client/store'
import { PiniaMobxUtilsRenderer } from '@renderer-shared/shards/pinia-mobx-utils'
import { NCard } from 'naive-ui'
import { computed, onMounted, ref, shallowReactive } from 'vue'

type Phase = 'Lobby' | 'Matchmaking' | 'ReadyCheck' | 'ChampSelect'
type View = {
  phase: Phase
  gameMode: 'ARAM' | 'KIWI'
  partySize: number | null
  queueSeconds: number | null
  estimatedSeconds: number | null
  readyResponse: 'Accepted' | 'Declined' | 'None' | null
  championId: number | null
  bench: number[]
  remainingSeconds: number | null
}

const titles = {
  Lobby: ['组队房间', 'Party lobby'],
  Matchmaking: ['正在匹配', 'Matchmaking'],
  ReadyCheck: ['确认对局', 'Match found'],
  ChampSelect: ['选择英雄', 'Champion select']
}

const client = useLeagueClientStore()
const app = useAppCommonStore()
const sync = useInstance(PiniaMobxUtilsRenderer)
const state = shallowReactive<{ view: View | null }>({ view: null })
const view = computed(() => state.view)
const chinese = computed(() => app.settings.locale.startsWith('zh'))
const title = computed(() => view.value ? titles[view.value.phase][chinese.value ? 0 : 1] : '')
const modeName = computed(() => view.value?.gameMode === 'KIWI'
  ? (chinese.value ? '海克斯大乱斗' : 'ARAM Mayhem')
  : (chinese.value ? '极地大乱斗' : 'ARAM'))
const responseName = computed(() => {
  switch (view.value?.readyResponse) {
    case 'Accepted': return chinese.value ? '已接受' : 'Accepted'
    case 'Declined': return chinese.value ? '已拒绝' : 'Declined'
    default: return chinese.value ? '等待确认' : 'Waiting for confirmation'
  }
})
const syncError = ref('')

function championName(id: number) {
  return client.gameData.championName(id)
}

function duration(seconds: number) {
  const minutes = Math.floor(seconds / 60)
  const remainder = seconds % 60
  return minutes ? minutes + ':' + String(remainder).padStart(2, '0') : remainder + (chinese.value ? ' 秒' : ' s')
}

onMounted(async () => {
  try {
    await sync.sync('aux-activity-main', 'state', state)
  } catch {
    syncError.value = chinese.value ? '暂时无法读取大乱斗状态' : 'Could not read the ARAM status'
  }
})
</script>

<style scoped>
.aux-activity { height: 100%; overflow-y: auto; padding: 16px; }
header { margin-bottom: 14px; }
h1, h2, p, dl, dd { margin: 0; }
h1 { font-size: 18px; font-weight: 650; }
h2 { font-size: 13px; font-weight: 600; }
p, dl, li { font-size: 13px; }
.queue-name { opacity: .72; margin-bottom: 4px; }
.room-details { display: grid; grid-template-columns: 1fr auto; gap: 9px 12px; }
.room-details dt { opacity: .72; }
.current-champion { display: flex; align-items: center; gap: 10px; }
.current-champion p { opacity: .72; }
.champion-icon { width: 30px; height: 30px; flex: 0 0 auto; border-radius: 4px; object-fit: cover; }
.remaining-time, .bench { margin-top: 12px; }
.bench ul { list-style: none; margin: 8px 0 0; padding: 0; display: grid; grid-template-columns: repeat(2, minmax(0, 1fr)); gap: 8px; }
.bench li { display: flex; align-items: center; gap: 6px; }
.bench li span { overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
</style>
