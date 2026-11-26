<template>
  <div class="flex items-center gap-2">
    <NButton size="tiny" type="primary" :disabled="disabled" @click="open">
      {{ t('settings.shortcutSelector.select') }}
    </NButton>
    <span>{{ shortcutId || t('settings.shortcutSelector.unset') }}</span>
    <NModal v-model:show="show" @after-enter="capture?.focus()">
      <NCard class="w-sm max-w-[90vw]" :title="english ? 'Game exit shortcut' : '结束游戏快捷键'">
        <p class="mb-3">
          {{ english ? 'Use two modifiers with a letter or number, or a modifier with an F key.' : '字母或数字需搭配两个修饰键，F 键可搭配一个修饰键。' }}
        </p>
        <input
          ref="capture"
          :aria-label="english ? 'Shortcut keys' : '快捷键组合'"
          class="w-full rounded border border-gray-500 bg-transparent px-3 py-2"
          readonly
          :value="draft || ''"
          @keydown="record"
        />
        <div class="mt-4 flex justify-end gap-2">
          <NButton size="small" :disabled="!shortcutId || saving" @click="submit(null)">
            {{ english ? 'Clear' : '清除' }}
          </NButton>
          <NButton size="small" type="primary" :disabled="!draft" :loading="saving" @click="submit(draft)">
            {{ english ? 'Save' : '保存' }}
          </NButton>
        </div>
      </NCard>
    </NModal>
  </div>
</template>

<script setup lang="ts">
import { useTranslation } from 'i18next-vue'
import { NButton, NCard, NModal, useMessage } from 'naive-ui'
import { ref, useTemplateRef } from 'vue'

const props = defineProps<{
  shortcutId: string | null
  disabled: boolean
  english: boolean
  save: (shortcut: string | null) => Promise<unknown>
}>()
const { t } = useTranslation()
const message = useMessage()
const capture = useTemplateRef<HTMLInputElement>('capture')
const show = ref(false)
const saving = ref(false)
const draft = ref<string | null>(null)

function open() {
  draft.value = props.shortcutId
  show.value = true
}

function record(event: KeyboardEvent) {
  if (event.metaKey) return
  const modifiers = [
    event.ctrlKey && 'Control',
    event.altKey && 'Alt',
    event.shiftKey && 'Shift'
  ].filter(Boolean)
  const alphanumeric = /^(?:Key|Digit)([A-Z0-9])$/.exec(event.code)?.[1]
  const functionKey = /^F(?:[1-9]|1[01])$/.test(event.code) ? event.code : null
  const key = modifiers.length >= 2 ? alphanumeric ?? functionKey : functionKey
  if (!modifiers.length || !key || (modifiers.length === 1 && event.altKey && key === 'F4')) return
  event.preventDefault()
  draft.value = [...modifiers, key].join('+')
}

async function submit(value: string | null) {
  if (saving.value) return
  saving.value = true
  try {
    await props.save(value)
    show.value = false
  } catch (error) {
    const failure = error as { message?: string }
    message.error(failure.message || (props.english ? 'Could not save the shortcut.' : '无法保存快捷键。'))
  } finally {
    saving.value = false
  }
}
</script>
