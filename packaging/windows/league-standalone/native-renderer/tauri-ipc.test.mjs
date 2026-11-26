import assert from 'node:assert/strict'
import test from 'node:test'

import { invokeLeagueCall, leaguePlayerRead, listenLeagueEvents } from './tauri-ipc.ts'

function installTauri(t, api) {
  globalThis.__TAURI__ = api
  t.after(() => delete globalThis.__TAURI__)
}

test('the match history client preserves the profile, an empty page and a read failure', async (t) => {
  const player = '6f541254-473e-4ddb-a9b2-e96908061989'
  const profile = { puuid: player, gameName: '练习玩家', tagLine: '10001', profileIconId: 407 }
  const page = { platformId: 'HN1', games: { games: [] } }
  const replies = [
    { success: true, data: profile },
    { success: true, data: page },
    { success: false, error: { message: '暂时无法读取战绩，请重试。' } }
  ]
  const calls = []
  installTauri(t, {
    core: {
      async invoke(command, payload) {
        calls.push([command, payload])
        return replies.shift()
      }
    }
  })

  const profileUrl = `/lol-summoner/v2/summoners/puuid/${player}`
  const historyUrl = `/lol-match-history/v1/products/lol/${player}/matches`
  assert.equal((await leaguePlayerRead({ method: 'get', url: profileUrl })).data, profile)
  const params = { begIndex: 0, endIndex: 19 }
  assert.equal((await leaguePlayerRead({ method: 'get', url: historyUrl, params })).data, page)
  await assert.rejects(leaguePlayerRead({ method: 'get', url: historyUrl, params }), /暂时无法读取战绩/)
  await assert.rejects(leaguePlayerRead({ method: 'post', url: profileUrl }))
  assert.deepEqual(calls.map(([command, payload]) => [command, payload.namespace, payload.method, ...payload.args]), [
    ['league_call', 'player-history-main', 'read', profileUrl, null],
    ['league_call', 'player-history-main', 'read', historyUrl, params],
    ['league_call', 'player-history-main', 'read', historyUrl, params]
  ])
})

test('forwards calls exclusively through league_call and preserves each response envelope', async (t) => {
  const calls = []
  const responses = [
    { success: true, data: { enabled: true } },
    { success: false, error: { message: 'Settings unavailable' }, isAxiosError: true }
  ]
  installTauri(t, {
    core: {
      async invoke(command, payload) {
        calls.push({ command, payload })
        return responses[calls.length - 1]
      }
    }
  })

  assert.deepEqual(await invokeLeagueCall('settings-main', 'set', 'feature', false), responses[0])
  assert.deepEqual(await invokeLeagueCall('app-common-main', 'getVersion'), responses[1])
  assert.deepEqual(calls, [
    {
      command: 'league_call',
      payload: { namespace: 'settings-main', method: 'set', args: ['feature', false] }
    },
    {
      command: 'league_call',
      payload: { namespace: 'app-common-main', method: 'getVersion', args: [] }
    }
  ])
})

test('propagates invoke rejection to the caller', async (t) => {
  const denied = new Error('Unknown League command')
  installTauri(t, {
    core: {
      async invoke() {
        throw denied
      }
    }
  })

  await assert.rejects(invokeLeagueCall('invalid', 'missing'), (error) => error === denied)
})

test('awaits a current-window subscription and removes it once', async (t) => {
  const received = []
  const names = []
  let deliver
  let detach
  let finishRemoval
  let removals = 0
  installTauri(t, {
    webviewWindow: {
      getCurrentWebviewWindow() {
        return {
          listen(name, callback) {
            names.push(name)
            deliver = callback
            return new Promise((resolve) => {
              detach = () =>
                resolve(() => {
                  removals += 1
                  return new Promise((complete) => {
                    finishRemoval = complete
                  })
                })
            })
          }
        }
      }
    }
  })

  let registered = false
  const pending = listenLeagueEvents((...args) => received.push(args)).then((unsubscribe) => {
    registered = true
    return unsubscribe
  })
  assert.equal(registered, false)
  assert.deepEqual(names, ['league-ipc-event'])
  detach()
  const unsubscribe = await pending

  deliver({ payload: null })
  deliver({ payload: { namespace: 'test', eventName: 'bad', args: 'invalid' } })
  deliver({ payload: { namespace: 'test', eventName: 'valid', args: [false, null, 'data'] } })
  let removed = false
  const removal = unsubscribe().then(() => {
    removed = true
  })
  await unsubscribe()
  deliver({ payload: { namespace: 'test', eventName: 'late', args: [] } })
  assert.equal(removed, false)
  assert.deepEqual(received, [['test', 'valid', false, null, 'data']])
  assert.equal(removals, 1)
  finishRemoval()
  await removal
  assert.equal(removed, true)
})
