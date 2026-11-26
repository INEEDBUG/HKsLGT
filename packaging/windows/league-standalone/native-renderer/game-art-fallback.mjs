// Resolve only public Riot icon identifiers; never forward client URLs or player IDs.
export function gameArtFallback(localUrl, champions) {
  if (!localUrl) return null
  const profile = /^http:\/\/league-art\.localhost\/lol-game-data\/assets\/v1\/profile-icons\/(0|[1-9]\d{0,9})\.jpg$/.exec(localUrl)?.[1]
  if (profile !== undefined && Number(profile) <= 0xffff_ffff) {
    return `https://game.gtimg.cn/images/lol/act/img/profileicon/${profile}.png`
  }
  const id = /^http:\/\/league-art\.localhost\/lol-game-data\/assets\/v1\/champion-icons\/([1-9]\d*)\.png$/.exec(localUrl)?.[1]
  const alias = id && champions[id]?.alias
  return typeof alias === 'string' && /^[A-Za-z0-9]{1,64}$/.test(alias)
    ? `https://game.gtimg.cn/images/lol/act/img/champion/${alias}.png`
    : null
}
