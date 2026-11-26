// Only champion art has a public fallback. No client URL or player information is forwarded.
export function championPortraitFallback(localUrl, champions) {
  if (!localUrl) return null
  const id = /^http:\/\/league-art\.localhost\/lol-game-data\/assets\/v1\/champion-icons\/([1-9]\d*)\.png$/.exec(localUrl)?.[1]
  const alias = id && champions[id]?.alias
  return typeof alias === 'string' && /^[A-Za-z0-9]{1,64}$/.test(alias)
    ? `https://game.gtimg.cn/images/lol/act/img/champion/${alias}.png`
    : null
}
