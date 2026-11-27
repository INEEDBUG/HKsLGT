// Public icon requests use fixed hosts and validated identifiers or filenames.
// Client origins, account identifiers, queries and headers stay local.
const LOCAL_ORIGIN = 'http://league-art.localhost/'
const ASSET_NAMESPACE = 'lol-game-data/assets/'
const TENCENT_IMAGES = 'https://game.gtimg.cn/images/lol/act/img/'
const RIOT_PATCH_IMAGES = 'https://ddragon.leagueoflegends.com/cdn/16.23.1/img/'
const RIOT_IMAGES = 'https://ddragon.leagueoflegends.com/cdn/img/'

// Names from Riot's summoner and rune-style data. Tencent uses the same names.
const spellNames = {
  1: 'SummonerBoost',
  3: 'SummonerExhaust',
  4: 'SummonerFlash',
  6: 'SummonerHaste',
  7: 'SummonerHeal',
  11: 'SummonerSmite',
  12: 'SummonerTeleport',
  13: 'SummonerMana',
  14: 'SummonerDot',
  21: 'SummonerBarrier',
  32: 'SummonerSnowball'
}

const runeStyleImages = {
  8000: '7201_Precision.png',
  8100: '7200_Domination.png',
  8200: '7202_Sorcery.png',
  8300: '7203_Whimsy.png',
  8400: '7204_Resolve.png'
}

function artworkPath(url) {
  if (!url?.startsWith(LOCAL_ORIGIN) || url.length > LOCAL_ORIGIN.length + 1024) return ''
  const path = url.slice(LOCAL_ORIGIN.length)
  if (!path.toLowerCase().startsWith(ASSET_NAMESPACE)) return ''
  let segments
  try {
    segments = path.slice(ASSET_NAMESPACE.length).split('/').map(decodeURIComponent)
  } catch {
    return ''
  }
  const valid = segments.every((segment) => /^[A-Za-z0-9_()'-][ A-Za-z0-9_.()'-]{0,254}$/.test(segment)
    && !/[. ]$/.test(segment))
  return valid ? segments.join('/') : ''
}

function numberedArt(kind, id, path) {
  if (!Number.isInteger(id) || id < 1 || id > 0xffff_ffff) return []
  switch (kind) {
    case 'item':
      return [TENCENT_IMAGES + 'item/' + id + '.png', RIOT_PATCH_IMAGES + 'item/' + id + '.png']
    case 'spell': {
      const name = spellNames[id]
      return name ? [TENCENT_IMAGES + 'spell/' + name + '.png', RIOT_PATCH_IMAGES + 'spell/' + name + '.png'] : []
    }
    case 'perk': {
      const urls = [TENCENT_IMAGES + 'rune/' + id + '.png']
      if (/^v1\/perk-images\/.+\.png$/i.test(path)) {
        const riotPath = path.slice(3).split('/').map(encodeURIComponent).join('/')
        urls.push(RIOT_IMAGES + riotPath)
      }
      return urls
    }
    case 'perk-style': {
      const image = runeStyleImages[id]
      return image ? [TENCENT_IMAGES + 'rune/' + image, RIOT_IMAGES + 'perk-images/Styles/' + image] : []
    }
    default:
      return []
  }
}

export function gameArtFallbacks(localUrl, champions, kind, id) {
  const path = artworkPath(localUrl)
  if (kind) return numberedArt(kind, id, path)

  const profile = /^v1\/profile-icons\/(0|[1-9]\d{0,9})\.jpg$/i.exec(path)?.[1]
  if (profile !== undefined && Number(profile) <= 0xffff_ffff) {
    const urls = [
      TENCENT_IMAGES + 'profileicon/' + profile + '.png',
      RIOT_PATCH_IMAGES + 'profileicon/' + profile + '.png'
    ]
    if (profile !== '0') urls.push(TENCENT_IMAGES + 'profileicon/0.png')
    return urls
  }

  const championId = /^v1\/champion-icons\/([1-9]\d{0,9})\.png$/i.exec(path)?.[1]
  if (championId !== undefined && Number(championId) <= 0xffff_ffff) {
    const alias = champions[championId]?.alias
    return typeof alias === 'string' && /^[A-Za-z0-9]{1,64}$/.test(alias)
      ? [TENCENT_IMAGES + 'champion/' + alias + '.png']
      : []
  }

  const lowercase = path.toLowerCase()
  const augmentFolder = /^assets\/ux\/(?:(?:cherry|kiwi)\/augments\/icons|jade\/s3icons\/samesized)\//.test(lowercase)
  const kiwiParticle = /^assets\/maps\/particles\/kiwi\/(?:magicmissile|selfdestruct)_small\.png$/.test(lowercase)
  const filename = lowercase.slice(lowercase.lastIndexOf('/') + 1)
  if ((augmentFolder || kiwiParticle) && /^[a-z0-9_.-]+_small\.png$/.test(filename)) {
    return [TENCENT_IMAGES + 'rune/' + filename]
  }
  return []
}
