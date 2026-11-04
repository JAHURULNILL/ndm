'use strict';
/* Tests for the REAL extension classifier module
 * (extension/fb-classify.js) — ported from the audit-232 prototype
 * suite (fb-proto/classify.test.js, 10 tests) plus the 2.3.2 WS-B
 * additions: the feed fallthrough closure, the per-video id, and
 * the recorder grouping key.
 *
 * ALL evidence here is FIXTURE evidence: the fixture is MODELLED
 * from public knowledge of Facebook's page structure, never
 * captured — live Facebook is unreachable from the build VM.
 * Run: node --test extension-tests/fb-classify.test.js */
const test = require('node:test');
const assert = require('node:assert');
const {
  classifyFacebook, parseDashManifest, classifyPerformanceEntry,
  facebookVideoId, fallbackTargetFor, streamGroupKey, offerableGroups,
} = require('../extension/fb-classify');
const F = require('./fb-fixture');

/* ---- the 10 ported prototype tests (fixture-modelled) ---- */

test('DASH manifest parses into sorted video reps + best audio', () => {
  const { videos, audios } = parseDashManifest(F.DASH_MANIFEST);
  assert.equal(videos.length, 3);
  assert.deepEqual(videos.map(v => v.height), [1080, 720, 360]);
  assert.equal(audios.length, 1);
  assert.equal(audios[0].bandwidth, 128000);
  assert.ok(videos[0].url.includes('1080p_modelled.mp4'));
  assert.ok(!videos[0].url.includes('&amp;'), 'BaseURL entities must be unescaped');
});

test('Comet object with dash_manifest yields DASH options, video+audio paired', () => {
  const { options } = classifyFacebook({ mainWorld: [F.COMET_VIDEO] });
  assert.equal(options.length, 3);
  assert.equal(options[0].height, 1080);
  assert.equal(options[0].kind, 'dash');
  assert.equal(options[0].confidence, 'high');
  assert.equal(options[0].streams.length, 2, 'video rep + best audio rep');
  assert.equal(options[0].streams[1].role, 'audio');
  assert.deepEqual(options.map(o => o.height), [1080, 720, 360]);
});

test('progressive-only Comet object yields HD + SD, deduped', () => {
  const { options } = classifyFacebook({ mainWorld: [F.COMET_VIDEO_PROGRESSIVE_ONLY] });
  assert.equal(options.length, 2);
  assert.equal(options[0].source, 'playable_url_quality_hd');
  assert.equal(options[0].streams.length, 1);
  assert.equal(options[0].streams[0].role, 'av');
  assert.match(options[0].label, /HD/);
  assert.match(options[1].label, /SD/);
});

test('DOM script JSON text (isolated-world readable) is found too', () => {
  const blob = JSON.stringify({ payload: F.COMET_VIDEO_PROGRESSIVE_ONLY });
  const { options } = classifyFacebook({ domJsonTexts: [blob] });
  assert.equal(options.length, 2);
});

test('performance entries: explicit token earns a height, unknown stays unknown, DASH segments emit NO option', () => {
  const { options, diagnostics } = classifyFacebook({ performanceEntries: F.PERFORMANCE_ENTRIES });
  const heights = options.map(o => o.height).sort((a, b) => a - b);
  assert.deepEqual(heights, [0, 720]);
  assert.equal(options.find(o => o.height === 720).confidence, 'low');
  assert.equal(options.find(o => o.height === 0).confidence, 'unknown');
  assert.ok(diagnostics.some(d => d.includes('DASH segment')), 'segment honesty diagnostic');
  assert.ok(!options.some(o => o.streams.some(s => s.url.endsWith('.png'))));
  assert.ok(!options.some(o => o.streams.some(s => s.url.includes('example.com'))), 'non-FB hosts rejected');
});

test('classifyPerformanceEntry rejects non-media and non-FB URLs', () => {
  assert.equal(classifyPerformanceEntry('https://video.x.fbcdn.net/image.jpg'), null);
  assert.equal(classifyPerformanceEntry('https://other.com/v.mp4'), null);
  const seg = classifyPerformanceEntry('https://video.x.fbcdn.net/dash/segment-1.m4s');
  assert.equal(seg.isDashSeg, true);
});

test('og:video meta is the last in-page rung and is labelled low confidence', () => {
  const { options } = classifyFacebook({ meta: { 'og:video': 'https://video.x.fbcdn.net/page.mp4', 'og:video:height': '480', 'og:video:type': 'video/mp4' } });
  assert.equal(options.length, 1);
  assert.equal(options[0].height, 480);
  assert.equal(options[0].confidence, 'low');
});

test('page-JSON options take precedence: performance + meta do not add rows', () => {
  const { options } = classifyFacebook({
    mainWorld: [F.COMET_VIDEO],
    performanceEntries: F.PERFORMANCE_ENTRIES,
    meta: { 'og:video': 'https://video.x.fbcdn.net/page.mp4' },
  });
  assert.equal(options.length, 3);
  assert.ok(options.every(o => o.source === 'dash_manifest'));
});

test('empty input yields no options and no crash', () => {
  const { options } = classifyFacebook({});
  assert.deepEqual(options, []);
  assert.deepEqual(classifyFacebook().options, []);
});

test('SegmentTemplate-only representations are skipped, never emitted as URLs', () => {
  const mpd = `<MPD><Period><AdaptationSet mimeType="video/mp4"><SegmentTemplate media="seg-$RepresentationID$.m4s"/><Representation id="v1" bandwidth="100" width="640" height="360" mimeType="video/mp4"></Representation></AdaptationSet></Period></MPD>`;
  const { options, diagnostics } = classifyFacebook({ dashManifest: mpd });
  assert.equal(options.length, 0);
  assert.ok(diagnostics.some(d => d.includes('SegmentTemplate')));
});

/* ---- WS-B additions ---- */

test('facebookVideoId names exactly one video or nothing (fixture URLs)', () => {
  assert.equal(facebookVideoId('https://www.facebook.com/SomePage/videos/1234567890123456/'), '1234567890123456');
  assert.equal(facebookVideoId('https://www.facebook.com/videos/1234567890123456'), '1234567890123456');
  assert.equal(facebookVideoId('https://www.facebook.com/reel/9876543210987654'), '9876543210987654');
  assert.equal(facebookVideoId('https://www.facebook.com/watch/?v=1234567890123456'), '1234567890123456');
  // a feed / profile / shortlink names NO video — never an id
  assert.equal(facebookVideoId('https://www.facebook.com/'), '');
  assert.equal(facebookVideoId('https://www.facebook.com/SomePage'), '');
  assert.equal(facebookVideoId('https://fb.watch/abcDEF/'), '');
  assert.equal(facebookVideoId('https://www.youtube.com/watch?v=abcDEF123_-'), '');
  assert.equal(facebookVideoId(''), '');
});

test('fallthrough closure: an unresolved Facebook target is NEVER the page (feed) URL', () => {
  // resolveVideoUrl's last step routes through fallbackTargetFor:
  // no permalink/id/direct source found on a Facebook page means
  // "unidentified" (''), so the picker shows its honest state and
  // no probe of location.href can ever be fired from it.
  const feed = 'https://www.facebook.com/';
  assert.equal(fallbackTargetFor(true, feed), '');
  assert.equal(fallbackTargetFor(true, 'https://www.facebook.com/SomePage'), '');
  assert.equal(fallbackTargetFor(true, 'https://www.facebook.com/watch'), '');
  // off Facebook the page URL remains the standing last resort
  assert.equal(fallbackTargetFor(false, 'https://example.com/watch/42'), 'https://example.com/watch/42');
  assert.equal(fallbackTargetFor(false, ''), '');
});

test('grouping: two Facebook videos on one feed never merge into one group (fixture URLs)', () => {
  const feed = 'https://www.facebook.com/';
  const a1 = 'https://video.fdhk.example.fbcdn.net/v/t42.1790-2/111111111111111_720p_a.mp4?_nc_cat=100';
  const a2 = 'https://video.fdhk.example.fbcdn.net/v/t42.1790-2/111111111111111_audio.m4a?_nc_cat=100';
  const b1 = 'https://video.fdhk.example.fbcdn.net/v/t42.1790-2/222222222222222_720p_b.mp4?_nc_cat=100';
  const streams = [
    { url: a1, ts: 1, groupKey: streamGroupKey(feed, a1) },
    { url: b1, ts: 2, groupKey: streamGroupKey(feed, b1) },
    { url: a2, ts: 3, groupKey: streamGroupKey(feed, a2) },
  ];
  assert.equal(streams[0].groupKey, 'fb:111111111111111');
  assert.equal(streams[1].groupKey, 'fb:222222222222222');
  const groups = offerableGroups(streams);
  assert.equal(groups.length, 2, 'one group per video');
  const ga = groups.find(g => g.key === 'fb:111111111111111');
  const gb = groups.find(g => g.key === 'fb:222222222222222');
  assert.deepEqual(ga.streams.map(s => s.url), [a1, a2], 'video A keeps only its own streams');
  assert.deepEqual(gb.streams.map(s => s.url), [b1]);
  assert.equal(ga.latestTs, 3);
});

test('grouping: a Facebook stream with an UNKNOWN key is not offered at all (fixture URLs)', () => {
  const feed = 'https://www.facebook.com/';
  // no numeric video token in the file name, feed page names no
  // video either → the key is unknown and the stream must vanish
  // from every offered group rather than join a feed-wide pool
  const orphan = 'https://video.fdhk.example.fbcdn.net/v/t42.1790-2/tokenless_modelled.mp4?_nc_cat=100';
  assert.equal(streamGroupKey(feed, orphan), '');
  const groups = offerableGroups([{ url: orphan, ts: 1, groupKey: streamGroupKey(feed, orphan) }]);
  assert.deepEqual(groups, []);
});

test('grouping: a Facebook permalink page keys its streams by the page id (fixture URLs)', () => {
  const page = 'https://www.facebook.com/SomePage/videos/1234567890123456/';
  const s = 'https://video.fdhk.example.fbcdn.net/v/t42.1790-2/tokenless_modelled.mp4?_nc_cat=100';
  assert.equal(streamGroupKey(page, s), 'fb:1234567890123456');
});

test('grouping: off Facebook the standing page key is unchanged', () => {
  assert.equal(
    streamGroupKey('https://example.com/watch/42', 'https://cdn.example.com/movie.mp4'),
    'page'
  );
  const groups = offerableGroups([
    { url: 'https://cdn.example.com/movie.mp4', ts: 5, groupKey: 'page' },
  ]);
  assert.equal(groups.length, 1);
  assert.equal(groups[0].key, 'page');
});
