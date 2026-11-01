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

      options.push({
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
      });
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
})();
