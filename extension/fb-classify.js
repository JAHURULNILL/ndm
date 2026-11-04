'use strict';
/* NDM Facebook classification module (2.3.2, WS-B) — the shipped
 * port of the audit-232 prototype (~/workspace/ndm-audit232/fb-proto/,
 * 10/10 fixture tests there; the same tests run against THIS file
 * from extension-tests/).
 *
 * MODELLED, NOT CAPTURED: every input shape here is modelled from
 * public knowledge of Facebook's page structure (Comet preload
 * JSON field names, DASH manifests served to the desktop player,
 * fbcdn URL patterns). No live Facebook data was used — FB is
 * unreachable from the build VM. Field presence/frequency on the
 * real logged-in feed is unproven until checked on the user's
 * laptop; the honesty bounds below are the contract either way.
 *
 * Pure, no dependencies, no DOM access — one file, four homes:
 *   - MAIN world on facebook.com (before page-hook.js): the hook
 *     classifies the page's own video objects into picker options.
 *   - Isolated world (before content.js): the pure FB URL/target
 *     decisions content.js routes through (video id, the feed
 *     fallthrough closure).
 *   - background.js service worker (importScripts): the recorder's
 *     per-video grouping key.
 *   - node tests (extension-tests/) via the module.exports guard.
 *
 * Input (all fields optional):
 * {
 *   mainWorld: [ <parsed page-JSON objects, any depth> ],   // main-world hook only
 *   dashManifest: '<MPD xml string>',                       // wherever obtained
 *   performanceEntries: [{ name, initiatorType }],          // Resource Timing
 *   meta: { 'og:video': url, 'og:video:width': '1280', ... }// isolated-world DOM
 *   domJsonTexts: [ '<script text>', ... ]                  // isolated-world DOM scripts
 * }
 * Output: { options: [ { label, height, kind, container, source,
 *                        confidence, streams: [{url, role, mimeType}] } ],
 *           diagnostics: [string] }
 */

var NdmFbClassify = (() => {

const VIDEO_FIELDS = [
  'playable_url', 'playable_url_quality_hd',
  'browser_native_hd_url', 'browser_native_sd_url',
];

function isHttpUrl(s) { return typeof s === 'string' && /^https:\/\//.test(s); }

function isFacebookHostname(h) {
  h = String(h || '').toLowerCase();
  return h === 'facebook.com' || h.endsWith('.facebook.com') || h === 'fb.watch';
}

/* The numeric video id a Facebook URL names, or '' when the URL
   names no specific video (a feed, a profile, a shortlink). Only
   the shapes that identify ONE video count: /videos/<id>,
   /reel/<id> (also as /<page>/videos/<id>), watch?v=<digits>. */
function facebookVideoId(urlStr) {
  let u;
  try { u = new URL(String(urlStr || '')); } catch (e) { return ''; }
  if (!isFacebookHostname(u.hostname)) return '';
  // Aligned to core's facebook_video_id exactly (W2-7): a path
  // segment must be ALL digits (a digit prefix like
  // /videos/12345extra/ names nothing), and the watch param is
  // read RAW — percent-encoded digits are not digits, so no
  // decoding divergence from core's raw query_param.
  const segs = u.pathname.split('/').filter(s => s !== '');
  for (let i = 0; i + 1 < segs.length; i++) {
    if ((segs[i] === 'videos' || segs[i] === 'reel') && /^\d+$/.test(segs[i + 1])) {
      return segs[i + 1];
    }
  }
  const rawQuery = (u.search || '').replace(/^\?/, '');
  for (const pair of rawQuery.split('&')) {
    const eq = pair.indexOf('=');
    if (eq > 0 && pair.slice(0, eq) === 'v') {
      const v = pair.slice(eq + 1);
      if (/^\d+$/.test(v)) return v;
      break;
    }
  }
  return '';
}

/* The feed fallthrough closure (audit-232 §2, correctness-
   critical): resolveVideoUrl() reaches its last step only when
   no per-video permalink, no direct source and no sniffed stream
   identified the clicked video. On any other site the page URL
   is the honest last resort (a dedicated page IS its video).
   On Facebook the page URL is a FEED of many videos — returning
   it would probe the feed page as if it were the video, and a
   wrong download could wear the right post's title. So on
   Facebook the unresolved target is '' — "unidentified" — and
   the picker shows its honest could-not-identify state instead
   of probing anything. One exception: a page URL that itself
   names ONE video (facebookVideoId non-empty — a dedicated
   /reel/<id>, /videos/<id> or watch?v=<id> page) is not a feed,
   so it stays a valid target; without this, bare Reel pages
   whose permalink the caller's patterns missed landed here and
   were declared unidentified on their own page (field bug F-1). */
function fallbackTargetFor(isFacebookPage, pageUrl) {
  if (!isFacebookPage) return pageUrl || '';
  return facebookVideoId(pageUrl) ? pageUrl : '';
}

/* Recursively collect objects that look like one FB video:
   they carry a dash_manifest and/or playable_url* fields.
   Bounded (W2-5): depth and node budgets shared across one
   top-level walk — a pathological blob stops that walk
   gracefully (partial results, no RangeError) instead of
   aborting the caller's whole scan. */
const FIND_MAX_DEPTH = 200;
const FIND_MAX_NODES = 20000;
function findVideoObjects(root, out, seen, _state, _depth) {
  if (!root || typeof root !== 'object') return out;
  const state = _state || { nodes: 0 };
  const depth = _depth || 0;
  if (depth > FIND_MAX_DEPTH || ++state.nodes > FIND_MAX_NODES) return out;
  if (seen.has(root)) return out;
  seen.add(root);
  const hasDash = typeof root.dash_manifest === 'string' && root.dash_manifest.includes('<MPD');
  const hasPlayable = VIDEO_FIELDS.some(f => isHttpUrl(root[f]));
  if (hasDash || hasPlayable) out.push(root);
  for (const v of Object.values(root)) {
    if (state.nodes > FIND_MAX_NODES) break;
    if (v && typeof v === 'object') findVideoObjects(v, out, seen, state, depth + 1);
    else if (typeof v === 'string' && v.length > 200 && v.includes('dash_manifest')) {
      // Comet data-sjs blobs sometimes double-encode the payload.
      try { findVideoObjects(JSON.parse(v), out, seen, state, depth + 1); } catch (e) {}
    }
  }
  return out;
}

/* Minimal MPD parser (regex-based; manifests are machine-generated
   and flat enough for this). Returns representations grouped into
   video[] / audio[] with their BaseURL (or SegmentTemplate media
   template, flagged template:true — a template is NOT a directly
   downloadable URL and is never emitted as one). */
function parseDashManifest(xml) {
  const videos = [], audios = [];
  if (typeof xml !== 'string' || !xml.includes('<MPD')) return { videos, audios };
  const sets = xml.match(/<AdaptationSet[\s\S]*?<\/AdaptationSet>/g) || [];
  for (const set of sets) {
    const setMime = (set.match(/mimeType="([^"]+)"/) || [])[1] || '';
    const reps = set.match(/<Representation[\s\S]*?(?:<\/Representation>|\/>)/g) || [];
    for (const rep of reps) {
      const attr = name => {
        const m = rep.match(new RegExp(name + '="([^"]*)"'));
        return m ? m[1] : '';
      };
      const mime = attr('mimeType') || setMime;
      const baseRaw = (rep.match(/<BaseURL>([^<]+)<\/BaseURL>/) || [])[1] || '';
      // W2-6: only https BaseURLs are streams at all — the same
      // test the YouTube extractor in this file applies per
      // format. An http/other-scheme BaseURL is dropped here,
      // at the layer that mints the URL, not left for a
      // downstream consumer to remember to check.
      const base = baseRaw && isHttpUrl(baseRaw.replace(/&amp;/g, '&'))
        ? baseRaw
        : '';
      const segTpl = (set.match(/<SegmentTemplate[^>]*media="([^"]+)"/) || [])[1] || '';
      const r = {
        id: attr('id'),
        bandwidth: parseInt(attr('bandwidth') || '0', 10) || 0,
        width: parseInt(attr('width') || '0', 10) || 0,
        height: parseInt(attr('height') || '0', 10) || 0,
        codecs: attr('codecs'),
        mimeType: mime,
        url: base ? base.replace(/&amp;/g, '&') : '',
        template: !base && !!segTpl,
        templatePattern: segTpl,
      };
      if (mime.startsWith('audio/')) audios.push(r);
      else if (mime.startsWith('video/')) videos.push(r);
    }
  }
  videos.sort((a, b) => b.height - a.height || b.bandwidth - a.bandwidth);
  audios.sort((a, b) => b.bandwidth - a.bandwidth);
  return { videos, audios };
}

function heightLabel(h) {
  if (!h) return '';
  if (h >= 2160) return `${h}p (4K Ultra HD)`;
  if (h >= 1440) return `${h}p (2K QHD)`;
  if (h >= 1080) return `${h}p (Full HD)`;
  if (h >= 720) return `${h}p (HD)`;
  if (h >= 480) return `${h}p (SD)`;
  return `${h}p`;
}

/* Options from one Comet video object (main-world shape). */
function optionsFromVideoObject(obj, diag) {
  const options = [];
  if (typeof obj.dash_manifest === 'string') {
    const { videos, audios } = parseDashManifest(obj.dash_manifest);
    const bestAudio = audios[0] || null;
    for (const v of videos) {
      if (!v.url) {
        diag.push(`dash representation ${v.id || '?'} has SegmentTemplate only — not directly downloadable, skipped`);
        continue;
      }
      const streams = [{ url: v.url, role: 'video', mimeType: v.mimeType || 'video/mp4' }];
      if (bestAudio && bestAudio.url) streams.push({ url: bestAudio.url, role: 'audio', mimeType: bestAudio.mimeType || 'audio/mp4' });
      options.push({
        label: heightLabel(v.height) || `DASH ${Math.round(v.bandwidth / 1000)} kbps`,
        height: v.height || 0,
        kind: 'dash',
        container: 'MP4',
        source: 'dash_manifest',
        confidence: 'high',
        streams,
      });
    }
    if (options.length) return options; // DASH wins over progressive for the same object
    diag.push('dash_manifest present but yielded no downloadable representations');
  }
  // Progressive (combined audio+video) URLs.
  const hd = obj.playable_url_quality_hd || obj.browser_native_hd_url;
  const sd = obj.playable_url || obj.browser_native_sd_url;
  if (isHttpUrl(hd)) {
    options.push({
      // FB's "HD" progressive rendition is historically 720p;
      // the exact height is NOT in the field — labelled as such.
      label: 'HD (progressive, ~720p — exact height not exposed)',
      height: 720, kind: 'progressive', container: 'MP4',
      source: obj.playable_url_quality_hd ? 'playable_url_quality_hd' : 'browser_native_hd_url',
      confidence: 'medium',
      streams: [{ url: hd, role: 'av', mimeType: 'video/mp4' }],
    });
  }
  if (isHttpUrl(sd) && sd !== hd) {
    options.push({
      label: 'SD (progressive, ~360p — exact height not exposed)',
      height: 360, kind: 'progressive', container: 'MP4',
      source: obj.playable_url ? 'playable_url' : 'browser_native_sd_url',
      confidence: 'medium',
      streams: [{ url: sd, role: 'av', mimeType: 'video/mp4' }],
    });
  }
  return options;
}

/* What ONE fbcdn / videodelivery performance entry can honestly say.
   Returns null for non-media, else a classified fragment.
   Quality inference is deliberately stingy: only explicit tokens
   in the URL earn a height; everything else is unknown. */
function classifyPerformanceEntry(url) {
  if (!isHttpUrl(url)) return null;
  let u;
  try { u = new URL(url); } catch (e) { return null; }
  const host = u.hostname.toLowerCase();
  const path = u.pathname.toLowerCase();
  const isFbCdn = host.endsWith('fbcdn.net') || host.includes('videodelivery') || host.endsWith('facebook.com');
  if (!isFbCdn) return null;
  const isDashSeg = path.endsWith('.m4s') || path.includes('/dash/') || /segment[-_]?\d/.test(path);
  const isMp4 = path.endsWith('.mp4');
  if (!isDashSeg && !isMp4) return null;
  // explicit quality tokens seen in FB CDN file names: _720p_ , -480p. , _hd_/_sd_
  let height = 0, token = '';
  const pm = path.match(/[_-](\d{3,4})p(?:[_-]|\.)/);
  if (pm) { height = parseInt(pm[1], 10); token = pm[0]; }
  else if (/[_-]hd(?:[_-]|\.)/.test(path)) { height = 720; token = 'hd'; }
  else if (/[_-]sd(?:[_-]|\.)/.test(path)) { height = 360; token = 'sd'; }
  // a video-id-ish token to group segments of one video: FB file
  // names lead with a long numeric id; absent that, no grouping.
  const idm = path.match(/\/(\d{8,})[_-]/);
  return {
    url, isDashSeg, isMp4, height, token,
    videoToken: idm ? idm[1] : '',
    mimeType: isMp4 ? 'video/mp4' : 'video/iso.segment',
  };
}

function optionsFromPerformanceEntries(entries, diag) {
  const frags = (entries || []).map(e => classifyPerformanceEntry(e && e.name)).filter(Boolean);
  if (!frags.length) return [];
  const options = [];
  const byKey = new Map();
  for (const f of frags) {
    const key = (f.videoToken || 'untokenised') + '|' + f.height + '|' + (f.isDashSeg ? 'dash' : 'mp4');
    if (!byKey.has(key)) byKey.set(key, []);
    byKey.get(key).push(f);
  }
  for (const [key, group] of byKey) {
    const h = group[0].height;
    if (group[0].isDashSeg) {
      diag.push(`performance: ${group.length} DASH segment entr${group.length === 1 ? 'y' : 'ies'} (${key}) — segments are not whole streams; no download option emitted from segments alone`);
      continue; // honest: a segment URL is not a downloadable file
    }
    options.push({
      label: h ? heightLabel(h) : 'Progressive MP4 (quality not inferable from URL)',
      height: h, kind: 'progressive', container: 'MP4',
      source: 'performance', confidence: h ? 'low' : 'unknown',
      streams: [{ url: group[0].url, role: 'av', mimeType: 'video/mp4' }],
    });
  }
  return options;
}

function optionsFromMeta(meta, diag) {
  if (!meta) return [];
  const url = meta['og:video:secure_url'] || meta['og:video:url'] || meta['og:video'];
  if (!isHttpUrl(url)) return [];
  const h = parseInt(meta['og:video:height'] || '0', 10) || 0;
  diag.push('og:video describes the page\'s own video only — on a feed URL it does not identify a feed item');
  return [{
    label: h ? heightLabel(h) : 'Page video (og:video, quality from meta only)',
    height: h, kind: 'progressive',
    container: (meta['og:video:type'] || '').includes('mp4') ? 'MP4' : 'MP4',
    source: 'og:video', confidence: 'low',
    streams: [{ url, role: 'av', mimeType: 'video/mp4' }],
  }];
}

function classifyFacebook(input) {
  const inp = input || {};
  const diag = [];
  let options = [];

  // 1. main-world page JSON (highest value) + DOM script JSON (same fields, if present as text)
  const objects = [];
  const seen = new Set();
  for (const o of inp.mainWorld || []) findVideoObjects(o, objects, seen);
  for (const t of inp.domJsonTexts || []) {
    if (typeof t !== 'string' || !t.includes('dash_manifest') && !t.includes('playable_url')) continue;
    try { findVideoObjects(JSON.parse(t), objects, seen); } catch (e) { diag.push('a DOM script blob looked like video JSON but did not parse'); }
  }
  for (const o of objects) options.push(...optionsFromVideoObject(o, diag));
  if (inp.dashManifest) {
    const fake = { dash_manifest: inp.dashManifest };
    options.push(...optionsFromVideoObject(fake, diag));
  }

  // 2. performance entries (never override page-JSON options)
  if (!options.length) options.push(...optionsFromPerformanceEntries(inp.performanceEntries, diag));
  else if (inp.performanceEntries) diag.push('performance entries present but page-JSON options already cover the video — entries not used');

  // 3. og:video meta (page-level only)
  if (!options.length) options.push(...optionsFromMeta(inp.meta, diag));

  // dedupe by stream URL set, sort by height desc
  const seenKeys = new Set();
  options = options.filter(o => {
    const k = o.streams.map(s => s.url).join('|');
    if (seenKeys.has(k)) return false;
    seenKeys.add(k); return true;
  }).sort((a, b) => b.height - a.height);

  return { options, diagnostics: diag };
}

/* ------------------------------------------------------------------ */
/* Recorder grouping key (audit-232 §2 prerequisite): on Facebook   */
/* one page holds MANY videos, so a captured stream may only be     */
/* offered under the key of ITS video — the page's own permalink    */
/* id when the page is one video, else the video token in the       */
/* stream's own fbcdn file name. No key derivable → '' (unknown):   */
/* the recorder must not offer or group such a stream at all,       */
/* because under a feed identity it could pair with another         */
/* video's streams. Off Facebook, a page is its own identity —      */
/* the standing 'page' key, exactly today's behaviour.              */
/* ------------------------------------------------------------------ */

/* The video token in an fbcdn file name: FB media file names lead
   with a long numeric id (the same token classifyPerformanceEntry
   reports for classified media). Grouping needs the token even
   for entries the quality classifier does not classify (an .m4a
   audio part is still its video's stream), so the extraction
   stands alone here — host-gated to FB's CDN shapes, and '' when
   no token exists: no token, no grouping, never a guess. */
function fbcdnVideoToken(url) {
  if (!isHttpUrl(url)) return '';
  let u;
  try { u = new URL(url); } catch (e) { return ''; }
  const host = u.hostname.toLowerCase();
  if (!(host.endsWith('fbcdn.net') || host.includes('videodelivery'))) return '';
  const idm = u.pathname.toLowerCase().match(/\/(\d{8,})[_-]/);
  return idm ? idm[1] : '';
}

function isFbCdnUrl(url) {
  if (!isHttpUrl(url)) return false;
  let u;
  try { u = new URL(url); } catch (e) { return false; }
  const host = u.hostname.toLowerCase();
  return host.endsWith('fbcdn.net') || host.includes('videodelivery');
}

function streamGroupKey(pageUrl, streamUrl) {
  let pageHost = '';
  try { pageHost = new URL(String(pageUrl || '')).hostname; } catch (e) {}
  if (!isFacebookHostname(pageHost)) {
    // W2-1: an UNKNOWN page (empty/unparseable — e.g. after an
    // MV3 service-worker restart emptied the tab→page map) must
    // never stamp a made-up 'page' identity on an FB CDN
    // stream: different feed videos would merge into one
    // unkeyed group. Unknown page + FB CDN stream = unknown
    // key (''), so the capture is not offered or merged.
    if (!pageHost && isFbCdnUrl(streamUrl)) return '';
    return 'page';
  }
  const fromPage = facebookVideoId(pageUrl);
  if (fromPage) return 'fb:' + fromPage;
  const token = fbcdnVideoToken(streamUrl);
  if (token) return 'fb:' + token;
  return '';
}

/* Partition recorded streams (each already stamped with a
   groupKey by streamGroupKey) into the groups that may be
   offered: one entry per key, streams in record order, and the
   newest stream time per group for "the video just played"
   picks. Streams whose key is '' (unknown) appear in NO group —
   that exclusion is the point of the key. */
function offerableGroups(streams) {
  const byKey = new Map();
  for (const s of streams || []) {
    const key = s && typeof s.groupKey === 'string' ? s.groupKey : '';
    if (!key) continue;
    if (!byKey.has(key)) byKey.set(key, { key, streams: [], latestTs: 0 });
    const g = byKey.get(key);
    g.streams.push(s);
    const ts = Number(s.ts) || 0;
    if (ts > g.latestTs) g.latestTs = ts;
  }
  return [...byKey.values()];
}

return {
  classifyFacebook, parseDashManifest, findVideoObjects,
  classifyPerformanceEntry, optionsFromVideoObject,
  facebookVideoId, isFacebookHostname, fallbackTargetFor,
  streamGroupKey, offerableGroups, fbcdnVideoToken, isFbCdnUrl,
};

})();

if (typeof module !== 'undefined' && module.exports) {
  module.exports = NdmFbClassify;
}
