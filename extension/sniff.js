'use strict';
/* NDM — media sniffing helpers (pure functions, shared by the
 * background worker; unit-testable in node).
 *
 * This is the IDM trick, generalized: the browser itself has already
 * fetched the page's streams by the time the user clicks, so the
 * extension watches the tab's network answers and remembers anything
 * that looks like a downloadable stream — an HLS/DASH manifest or a
 * plain media file. The panel can then offer those instantly instead
 * of paying a fresh extraction. DRM and fully custom loaders stay on
 * the probe path; nothing here pretends otherwise.
 */

function ndmPathOf(url) {
  try {
    return new URL(url).pathname.toLowerCase();
  } catch (e) {
    return '';
  }
}

/* Classify one observed response: 'hls' | 'dash' | 'file' | null.
 * Segment traffic is deliberately excluded — a DASH/HLS player's
 * chunk requests are not downloads; the MANIFEST is the download. */
function ndmClassify(url, contentType) {
  if (!url || !/^https?:\/\//i.test(url)) return null;
  const ct = (contentType || '').toLowerCase();
  const path = ndmPathOf(url);
  if (!path) return null;
  if (ct.includes('mpegurl') || path.endsWith('.m3u8')) return 'hls';
  if (ct.includes('dash+xml') || path.endsWith('.mpd')) return 'dash';
  // segment-shaped traffic, whatever its extension claims to be
  if (path.endsWith('.m4s') || path.endsWith('.ts')
    || path.includes('/segment') || path.includes('segment-')
    || /[?&]sq=\d/.test(url)) return null;
  // YouTube serves its video as DASH segments from googlevideo; the
  // segment URLs alone do not make an honest download offer (they
  // expire and arrive split) — YouTube stays on the probe path
  let host = '';
  try { host = new URL(url).hostname.toLowerCase(); } catch (e) { /* keep '' */ }
  if (host.endsWith('googlevideo.com')) return null;
  const mediaExt = /\.(mp4|webm|mkv|mov|m4v|mp3|m4a|aac|ogg|opus|wav|flac)$/.test(path);
  if (mediaExt) return 'file';
  if (ct.startsWith('video/') || ct.startsWith('audio/')) return 'file';
  return null;
}

/* Parse an HLS master playlist into its variant ladder. A media
 * playlist (no STREAM-INF) yields one "as served" option pointing at
 * the playlist itself. Relative variant URIs are resolved. */
function ndmParseHls(text, baseUrl) {
  const hasAudio = /#EXT-X-MEDIA:[^\n]*TYPE=AUDIO/.test(text);
  // the default audio rendition's own playlist: yt-dlp cannot pair
  // a separate HLS audio rendition with the video variant on its
  // own (verified against the generic extractor), so the app
  // downloads this alongside and muxes the two with ffmpeg
  let audioUri = '';
  const mediaLines = text.match(/#EXT-X-MEDIA:[^\n]*TYPE=AUDIO[^\n]*/g) || [];
  const pick = mediaLines.find(l => /DEFAULT=YES/.test(l)) || mediaLines[0];
  if (pick) {
    const m = pick.match(/URI="([^"]+)"/);
    if (m) {
      audioUri = m[1];
      try { audioUri = new URL(audioUri, baseUrl).href; } catch (e) { /* keep raw */ }
    }
  }
  if (!/#EXT-X-STREAM-INF/.test(text)) {
    return { master: false, hasAudio: false, audioUri: '', options: [{ height: 0, bandwidth: 0, uri: baseUrl }] };
  }
  const byHeight = new Map();
  const lines = text.split(/\r?\n/);
  for (let i = 0; i < lines.length; i++) {
    const line = lines[i];
    if (!line.startsWith('#EXT-X-STREAM-INF')) continue;
    const bw = parseInt((line.match(/BANDWIDTH=(\d+)/) || [])[1] || '0', 10);
    const res = line.match(/RESOLUTION=(\d+)x(\d+)/);
    const height = res ? parseInt(res[2], 10) : 0;
    // the variant URI is the next non-empty, non-tag line
    let uri = '';
    for (let j = i + 1; j < lines.length; j++) {
      const u = lines[j].trim();
      if (!u) continue;
      if (!u.startsWith('#')) uri = u;
      break;
    }
    if (!uri) continue;
    let abs = uri;
    try { abs = new URL(uri, baseUrl).href; } catch (e) { /* keep raw */ }
    const prev = byHeight.get(height);
    if (!prev || bw > prev.bandwidth) {
      byHeight.set(height, { height, bandwidth: bw, uri: abs });
    }
  }
  const options = [...byHeight.values()]
    .sort((a, b) => b.height - a.height || b.bandwidth - a.bandwidth);
  return { master: true, hasAudio, audioUri, options };
}

/* Parse a DASH MPD into its video quality ladder (heights +
 * bandwidths). Regex-based on purpose: MV3 service workers have no
 * DOMParser. Audio presence = any audio AdaptationSet. */
function ndmParseMpd(text, baseUrl) {
  const hasAudio = /<AdaptationSet[^>]*(mimeType="audio\/|contentType="audio")/.test(text)
    || /<Representation[^>]*mimeType="audio\//.test(text);
  const byHeight = new Map();
  const setRe = /<AdaptationSet[^>]*>([\s\S]*?)<\/AdaptationSet>/g;
  let setMatch;
  let sawVideoSet = false;
  while ((setMatch = setRe.exec(text)) !== null) {
    const setTag = setMatch[0].slice(0, setMatch[0].indexOf('>') + 1);
    const isVideo = /mimeType="video\/|contentType="video"/.test(setTag);
    const body = setMatch[1];
    const repRe = /<Representation[^>]*>/g;
    let rep;
    while ((rep = repRe.exec(body)) !== null) {
      const tag = rep[0];
      const height = parseInt((tag.match(/height="(\d+)"/) || [])[1] || '0', 10);
      const bw = parseInt((tag.match(/bandwidth="(\d+)"/) || [])[1] || '0', 10);
      const repVideo = isVideo || /mimeType="video\//.test(tag) || height > 0;
      if (!repVideo) continue;
      sawVideoSet = true;
      const prev = byHeight.get(height);
      if (!prev || bw > prev.bandwidth) {
        byHeight.set(height, { height, bandwidth: bw, uri: baseUrl });
      }
    }
  }
  const options = [...byHeight.values()]
    .filter(o => o.height > 0)
    .sort((a, b) => b.height - a.height || b.bandwidth - a.bandwidth);
  return { master: sawVideoSet, hasAudio, options };
}

function ndmFmtBytes(n) {
  if (!n && n !== 0) return '';
  if (n < 1024) return n + ' B';
  if (n < 1024 * 1024) return (n / 1024).toFixed(1) + ' KB';
  if (n < 1024 * 1024 * 1024) return (n / (1024 * 1024)).toFixed(1) + ' MB';
  return (n / (1024 * 1024 * 1024)).toFixed(2) + ' GB';
}

const NDM_SNIFF = { classify: ndmClassify, parseHls: ndmParseHls, parseMpd: ndmParseMpd, fmtBytes: ndmFmtBytes };
if (typeof module !== 'undefined' && module.exports) module.exports = NDM_SNIFF;
