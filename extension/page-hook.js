'use strict';
/* NDM MAIN-world page hook:
 * Extracts already-loaded video stream metadata (resolutions, exact byte
 * sizes, duration, thumbnail, title) directly from the page's player in <2ms
 * so clicking "Download with NDM" on YouTube & HTML5 players opens the
 * quality + MB table INSTANTLY with zero waiting.
 */
(function () {
  if (window.__ndmPageHookLoaded) return;
  window.__ndmPageHookLoaded = true;

  function fmtSize(bytes) {
    if (!bytes || bytes <= 0) return '';
    const KB = 1024;
    const MB = 1024 * 1024;
    const GB = 1024 * 1024 * 1024;
    if (bytes >= GB) return (bytes / GB).toFixed(2) + ' GB';
    if (bytes >= MB) {
      const m = bytes / MB;
      return (m >= 100 ? m.toFixed(0) : m.toFixed(1)) + ' MB';
    }
    if (bytes >= KB) return (bytes / KB).toFixed(0) + ' KB';
    return bytes + ' B';
  }

  function heightTag(h) {
    if (h >= 4320) return '8K Ultra HD';
    if (h >= 2160) return '4K Ultra HD';
    if (h >= 1440) return '2K QHD';
    if (h >= 1080) return 'Full HD';
    if (h >= 720) return 'HD';
    if (h >= 480) return 'SD';
    return '';
  }

  function getYouTubeVideoId(urlStr) {
    try {
      const u = new URL(urlStr || location.href);
      if (u.hostname.includes('youtube.com')) {
        if (u.pathname.startsWith('/shorts/')) {
          return u.pathname.split('/shorts/')[1]?.split(/[/?#]/)[0] || '';
        }
        return u.searchParams.get('v') || '';
      }
      if (u.hostname === 'youtu.be') {
        return u.pathname.slice(1).split(/[/?#]/)[0] || '';
      }
    } catch (e) {}
    return '';
  }

  function extractYouTubePlayerInfo() {
    if (!location.hostname.includes('youtube.com')) return null;
    const expectedId = getYouTubeVideoId(location.href);
    if (!expectedId) return null;

    let pr = null;
    try {
      const moviePlayer = document.getElementById('movie_player');
      if (moviePlayer && typeof moviePlayer.getPlayerResponse === 'function') {
        const cand = moviePlayer.getPlayerResponse();
        if (cand && cand.videoDetails && cand.videoDetails.videoId === expectedId) {
          pr = cand;
        }
      }
    } catch (e) {}

    if (!pr) {
      try {
        const flexy = document.querySelector('ytd-watch-flexy');
        const cand = flexy && (flexy.playerData || (flexy.data && flexy.data.playerResponse));
        if (cand && cand.videoDetails && cand.videoDetails.videoId === expectedId) {
          pr = cand;
        }
      } catch (e) {}
    }

    if (!pr) {
      try {
        const cand = window.ytInitialPlayerResponse;
        if (cand && cand.videoDetails && cand.videoDetails.videoId === expectedId) {
          pr = cand;
        }
      } catch (e) {}
    }

    if (!pr || !pr.videoDetails || !pr.streamingData) return null;

    const vd = pr.videoDetails;
    const sd = pr.streamingData;
    const duration = parseInt(vd.lengthSeconds || '0', 10) || 0;
    const title = vd.title || document.title.replace(/\s*-\s*YouTube$/i, '');
    const uploader = vd.author || '';
    const thumbs = (vd.thumbnail && vd.thumbnail.thumbnails) || [];
    const bestThumb = thumbs.length
      ? thumbs[thumbs.length - 1].url
      : `https://i.ytimg.com/vi/${expectedId}/hqdefault.jpg`;

    const allFormats = []
      .concat(sd.formats || [])
      .concat(sd.adaptiveFormats || []);
    if (!allFormats.length) return null;

    let bestAudioBytes = 0;
    let bestM4aBytes = 0;
    let bestAudioKbps = 128;
    let bestM4aKbps = 128;
    // A size is approximate when it was derived from bitrate ×
    // duration because the stream carried no contentLength —
    // consumers mark those with a "~" instead of claiming exact.
    let bestAudioApprox = false;
    let bestM4aApprox = false;
    // The best M4A format OBJECT: the audio stream every video
    // option pairs with (2.3.1 item 14 — the options carry the
    // page's own stream URLs, so the app's download can start
    // from this data without any resolver run).
    let bestM4aFormat = null;

    for (const f of allFormats) {
      const mime = (f.mimeType || '').toLowerCase();
      if (!mime.startsWith('audio/')) continue;
      const cl = parseInt(f.contentLength || '0', 10);
      const br = f.averageBitrate || f.bitrate || 0;
      const est = cl > 0 ? cl : duration > 0 && br > 0 ? Math.round((br * duration) / 8) : 0;
      const kbps = br > 0 ? Math.round(br / 1000) : 128;
      if (est > bestAudioBytes) {
        bestAudioBytes = est;
        bestAudioKbps = kbps;
        bestAudioApprox = cl <= 0;
      }
      if (mime.includes('mp4') && est > bestM4aBytes) {
        bestM4aBytes = est;
        bestM4aKbps = kbps;
        bestM4aApprox = cl <= 0;
        bestM4aFormat = f;
      }
    }
    if (bestAudioBytes === 0 && duration > 0) {
      bestAudioBytes = Math.round((128000 * duration) / 8);
      bestAudioApprox = true;
    }
    if (bestM4aBytes === 0) {
      bestM4aBytes = bestAudioBytes;
      bestM4aApprox = bestAudioApprox;
    }

    // Group video streams by height
    const byHeight = new Map();
    for (const f of allFormats) {
      const mime = (f.mimeType || '').toLowerCase();
      if (!mime.startsWith('video/')) continue;
      const h = parseInt(f.height || '0', 10);
      if (!h || h < 120) continue;
      const cl = parseInt(f.contentLength || '0', 10);
      const br = f.averageBitrate || f.bitrate || 0;
      const vBytes = cl > 0 ? cl : duration > 0 && br > 0 ? Math.round((br * duration) / 8) : 0;
      const vApprox = cl <= 0 && vBytes > 0;
      const isMp4 = mime.includes('mp4');
      const hasBundledAudio = !!(f.audioQuality || f.audioSampleRate);
      const fps = parseInt(f.fps || '0', 10) || 30;

      const prev = byHeight.get(h);
      // Prefer mp4 with known size, or higher bitrate within same container preference
      const score = (isMp4 ? 1000000000000 : 0) + (vBytes > 0 ? vBytes : br);
      if (!prev || score > prev.score) {
        byHeight.set(h, {
          height: h,
          vBytes,
          vApprox,
          hasBundledAudio,
          fps,
          score,
          format: f,
        });
      }
    }

    const heights = [...byHeight.keys()].sort((a, b) => b - a);
    if (!heights.length) return null;

    // The generated selectors all force ba[ext=m4a]: the audio a
    // download will actually carry is the largest M4A stream, so
    // size estimates pair with THAT — not the largest audio of any
    // codec (a high-bitrate Opus used to inflate every total).
    const options = [];
    for (const h of heights) {
      const item = byHeight.get(h);
      const totalBytes = item.vBytes > 0
        ? item.vBytes + (item.hasBundledAudio ? 0 : bestM4aBytes)
        : 0;
      const sizeText = fmtSize(totalBytes);
      const tag = heightTag(h);
      const fpsTag = item.fps > 30 ? ` ${item.fps}fps` : '';
      const label = tag ? `${h}p (${tag}${fpsTag})` : `${h}p${fpsTag}`;
      // fps lives in the label only — baking it into the note as
      // well printed it twice on every row.
      const noteParts = ['MP4'];
      if (sizeText) noteParts.push(sizeText);

      const opt = {
        selector: `bv*[height<=${h}][ext=mp4]+ba[ext=m4a]/b[height<=${h}][ext=mp4]/bv*[height<=${h}]+ba/b[height<=${h}]/b`,
        label,
        ext: 'mp4',
        note: noteParts.join(' • '),
        size_bytes: totalBytes,
        size_text: sizeText,
        size_approx:
          totalBytes > 0 &&
          (item.vApprox || (!item.hasBundledAudio && bestM4aApprox)),
        audio: false,
        height: h,
      };
      // 2.3.1 (item 14, browser-primary): carry the page's OWN
      // stream URLs for this quality — the same formats the
      // sizes above were computed from. A format whose response
      // entry has no direct URL (signatureCipher only) cannot
      // be handed over; the option then simply has no streams
      // and the app's yt-dlp fallback serves it, as before.
      const vf = item.format;
      if (vf && typeof vf.url === 'string' && /^https:\/\//.test(vf.url)) {
        const vStream = {
          url: vf.url,
          mimeType: (vf.mimeType || '').split(';')[0].trim(),
          qualityLabel: `${h}p`,
          hasAudio: item.hasBundledAudio,
          hasVideo: true,
        };
        if (item.hasBundledAudio) {
          opt.streams = [vStream];
        } else if (
          bestM4aFormat &&
          typeof bestM4aFormat.url === 'string' &&
          /^https:\/\//.test(bestM4aFormat.url)
        ) {
          opt.streams = [
            vStream,
            {
              url: bestM4aFormat.url,
              mimeType: (bestM4aFormat.mimeType || '').split(';')[0].trim(),
              qualityLabel: '',
              hasAudio: true,
              hasVideo: false,
            },
          ];
        }
      }
      options.push(opt);
    }

    // Both audio rows are m4a-first selectors (ba[ext=m4a]/…), so
    // the file lands as .m4a and the rows say M4A — an "MP3" label
    // here promised a conversion the app never performs. Their
    // size/kbps are the M4A stream's, not the largest audio's.
    const bestM4aSizeText = fmtSize(bestM4aBytes);
    const bestM4aApproxFlag = bestM4aBytes > 0 && bestM4aApprox;
    options.push({
      selector: 'ba[ext=m4a]/ba/b',
      label: 'Audio Only (M4A — High Quality)',
      ext: 'm4a',
      note: ['M4A', `${bestM4aKbps} kbps`, bestM4aSizeText].filter(Boolean).join(' • '),
      size_bytes: bestM4aBytes,
      size_text: bestM4aSizeText,
      size_approx: bestM4aApproxFlag,
      audio: true,
      height: 0,
    });

    options.push({
      selector: 'ba[ext=m4a]/ba',
      label: 'Audio Only (M4A — Original)',
      ext: 'm4a',
      note: ['M4A', 'AAC Audio', bestM4aSizeText].filter(Boolean).join(' • '),
      size_bytes: bestM4aBytes,
      size_text: bestM4aSizeText,
      size_approx: bestM4aApproxFlag,
      audio: true,
      height: 0,
    });

    return {
      videoId: expectedId,
      url: `https://www.youtube.com/watch?v=${expectedId}`,
      title,
      duration,
      thumbnail: bestThumb,
      uploader,
      options,
    };
  }

  function broadcastPlayerInfo() {
    const info = extractYouTubePlayerInfo();
    if (info) {
      window.postMessage(
        {
          source: 'ndm-page-hook',
          type: 'ndm-player-info',
          pageUrl: location.href,
          info,
        },
        '*'
      );
    }
  }

  window.addEventListener('message', e => {
    if (e.source !== window || !e.data || e.data.source !== 'ndm-content') return;
    if (e.data.type === 'ndm-request-player-info') {
      broadcastPlayerInfo();
    }
  });

  window.addEventListener('yt-navigate-finish', () => {
    setTimeout(broadcastPlayerInfo, 80);
  });
  document.addEventListener('loadedmetadata', () => {
    broadcastPlayerInfo();
  }, true);

  setTimeout(broadcastPlayerInfo, 50);

  /* ---------------------------------------------------------------- */
  /* Facebook (2.3.2, WS-B): the same page-data door as YouTube, on   */
  /* the page's OWN objects. Comet serialises its video data into    */
  /* the page (preload / data-sjs script JSON): each video object    */
  /* carries a dash_manifest and/or the progressive playable_url*    */
  /* fields. fb-classify.js (loaded before this hook in the MAIN     */
  /* world on facebook.com) classifies one object into picker        */
  /* options (audit ladder rungs 1–2); this hook only finds the      */
  /* objects, keys them by THEIR video id and bridges them to        */
  /* content.js — per video, never per page: a feed holds many        */
  /* videos, and one video's data must never be cached or offered     */
  /* under another's id. Objects with no derivable id on a page      */
  /* whose URL names no video are skipped, not guessed at.           */
  /* Field shapes are MODELLED (audit-232 §3) — live verification    */
  /* belongs on the user's machine; the VM cannot reach Facebook.    */
  /* ---------------------------------------------------------------- */
  const isFacebookPage = /(^|\.)facebook\.com$/.test(location.hostname);
  if (isFacebookPage && typeof NdmFbClassify !== 'undefined') {
    const FBC = NdmFbClassify;
    // A page-data quality whose stored group is somehow not live
    // app-side falls back to yt-dlp with the picker's own generic
    // best-MP4 selector (the DEFAULT_QUALITIES 'best' shape), so
    // the approved fallback order is unchanged.
    const FB_FALLBACK_SELECTOR =
      'bv*[ext=mp4]+ba[ext=m4a]/b[ext=mp4]/bv*+ba/b';
    const fbSent = new Map(); // videoId -> option signature already broadcast

    /* Classifier options -> the picker's option shape (the same
       fields the YouTube extractor above produces). Sizes are
       honestly absent: FB page data exposes no contentLength, so
       no size is claimed. The streams ride along (item 14's door):
       content.js sanitizes them and names their capture group. */
    function fbPickerOptions(classifierOptions) {
      const out = [];
      for (const o of classifierOptions || []) {
        const streams = [];
        for (const s of o.streams || []) {
          const st = {
            url: s.url,
            mimeType: s.mimeType || 'video/mp4',
            qualityLabel: '',
            hasAudio: false,
            hasVideo: false,
          };
          if (s.role === 'audio') {
            st.hasAudio = true;
          } else if (s.role === 'video') {
            st.hasVideo = true;
            st.qualityLabel = o.height ? `${o.height}p` : '';
          } else {
            // 'av' — one progressive file carrying both
            st.hasAudio = true;
            st.hasVideo = true;
            st.qualityLabel = o.height ? `${o.height}p` : '';
          }
          streams.push(st);
        }
        if (!streams.length) continue;
        out.push({
          selector: FB_FALLBACK_SELECTOR,
          label: o.label,
          ext: 'mp4',
          note: 'MP4',
          size_bytes: 0,
          size_text: '',
          size_approx: false,
          audio: false,
          height: o.height || 0,
          streams,
        });
      }
      return out;
    }

    /* Scan the page's serialised video objects, keyed by video
       id. Bounded: at most 24 candidate blobs, 1.5 MB each —
       a feed page carries many scripts and the scan runs on
       picker requests, so it must stay cheap. */
    function collectFbVideos() {
      const byId = new Map();
      const pageId = FBC.facebookVideoId(location.href);
      const addObject = obj => {
        const diag = [];
        const opts = FBC.optionsFromVideoObject(obj, diag);
        if (!opts.length) return;
        let id = '';
        if (typeof obj.id === 'string' && /^\d{5,}$/.test(obj.id)) id = obj.id;
        else if (typeof obj.id === 'number' && Number.isFinite(obj.id)) id = String(obj.id);
        if (!id) id = pageId; // a permalink page names its one video
        if (!id) return; // unkeyable on a feed: skipped, never guessed
        if (!byId.has(id)) byId.set(id, opts);
      };
      let scanned = 0;
      for (const s of document.querySelectorAll('script')) {
        if (scanned >= 24) break;
        const t = s.textContent || '';
        if (t.length < 40 || t.length > 1500000) continue;
        if (!t.includes('dash_manifest') && !t.includes('playable_url')) continue;
        scanned++;
        let parsed = null;
        try {
          parsed = JSON.parse(t);
        } catch (e) {}
        if (!parsed) continue;
        const objs = [];
        FBC.findVideoObjects(parsed, objs, new Set());
        for (const o of objs) addObject(o);
      }
      return byId;
    }

    function broadcastFbInfo(onlyId) {
      let byId;
      try {
        byId = collectFbVideos();
      } catch (e) {
        return;
      }
      for (const [id, classifierOptions] of byId) {
        if (onlyId && id !== onlyId) continue;
        const options = fbPickerOptions(classifierOptions);
        if (!options.length) continue;
        const sig = options
          .map(o => o.streams.map(s => s.url).join('|'))
          .join(';;');
        // Unprompted broadcasts dedupe by signature; an explicit
        // per-id request ALWAYS answers — the requester may have
        // loaded after the first broadcast and missed it.
        if (!onlyId && fbSent.get(id) === sig) continue;
        fbSent.set(id, sig);
        window.postMessage(
          {
            source: 'ndm-page-hook',
            type: 'ndm-fb-info',
            pageUrl: location.href,
            info: { videoId: id, options },
          },
          '*'
        );
      }
    }

    window.addEventListener('message', e => {
      if (e.source !== window || !e.data || e.data.source !== 'ndm-content') return;
      if (e.data.type === 'ndm-request-fb-info') {
        // Per click, per video: the picker names the id it is
        // opening for; rescan (feed posts load lazily) and answer
        // for that id — and refresh the others while here.
        broadcastFbInfo(String(e.data.videoId || ''));
      }
    });

    setTimeout(() => broadcastFbInfo(''), 300);
  }
})();
