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
  findVideoObjects, videoObjectId, keyOptionsByVideoId,
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
  // …but a dedicated permalink page names ONE video, so it is a
  // valid target, not a feed (field bug F-1: bare Reel pages
  // were declared unidentified on the video's own page)
  assert.equal(
    fallbackTargetFor(true, 'https://www.facebook.com/reel/2501272810378759'),
    'https://www.facebook.com/reel/2501272810378759');
  assert.equal(
    fallbackTargetFor(true, 'https://www.facebook.com/SomePage/videos/1234567890'),
    'https://www.facebook.com/SomePage/videos/1234567890');
  assert.equal(
    fallbackTargetFor(true, 'https://www.facebook.com/watch?v=1234567890'),
    'https://www.facebook.com/watch?v=1234567890');
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

test('W2-1: an unknown page never keys an FB CDN stream as page', () => {
  const s = 'https://video.fdhk.example.fbcdn.net/v/t42.1790-2/111111111111111_720p_a.mp4?_nc_cat=100';
  const tokenless = 'https://video.fdhk.example.fbcdn.net/v/t42.1790-2/tokenless_modelled.mp4?_nc_cat=100';
  // SW-restart shape: page URL unknown/empty — the key is
  // UNKNOWN ('') even when the file name carries a token:
  // the recorder must not vouch for a video it cannot tie to
  // any known page, so the capture is not offered at all.
  assert.equal(streamGroupKey('', s), '');
  assert.equal(streamGroupKey('', tokenless), '', 'tokenless FB stream on an unknown page stays unkeyed');
  // the audit harness case: never 'page' for an FB CDN stream
  assert.notEqual(streamGroupKey('', s), 'page');
  assert.notEqual(streamGroupKey('', tokenless), 'page');
  // unknown page + non-FB stream keeps the standing page key
  assert.equal(streamGroupKey('', 'https://cdn.example.com/movie.mp4'), 'page');
});

test('W2-7: facebookVideoId matches core — all-digit segments, raw watch param', () => {
  // digit PREFIX segments name nothing (core returns None)
  assert.equal(facebookVideoId('https://www.facebook.com/videos/12345extra/'), '');
  assert.equal(facebookVideoId('https://www.facebook.com/reel/9876xyz'), '');
  // percent-encoded watch digits are not digits in the raw param
  assert.equal(facebookVideoId('https://www.facebook.com/watch/?v=%31%32%33'), '');
  // canonical shapes still agree with core
  assert.equal(facebookVideoId('https://www.facebook.com/SomePage/videos/12345/'), '12345');
  assert.equal(facebookVideoId('https://www.facebook.com/videos/12345'), '12345');
  assert.equal(facebookVideoId('https://www.facebook.com/watch/?v=12345&x=1'), '12345');
});

test('W2-6: DASH BaseURLs get scheme validation — only https is emitted as a stream', () => {
  const mpd = `<MPD><Period><AdaptationSet mimeType="video/mp4"><Representation id="v1" bandwidth="100" width="640" height="360" mimeType="video/mp4"><BaseURL>http://evil.example/steal.mp4</BaseURL></Representation><Representation id="v2" bandwidth="200" width="1280" height="720" mimeType="video/mp4"><BaseURL>https://video.x.fbcdn.net/ok.mp4</BaseURL></Representation></AdaptationSet></Period></MPD>`;
  const { videos } = parseDashManifest(mpd);
  assert.equal(videos.find(v => v.id === 'v1').url, '');
  assert.equal(videos.find(v => v.id === 'v2').url, 'https://video.x.fbcdn.net/ok.mp4');
  const { options } = classifyFacebook({ dashManifest: mpd });
  assert.ok(!options.some(o => o.streams.some(s => s.url.includes('evil.example'))));
  assert.ok(options.some(o => o.streams.some(s => s.url.includes('ok.mp4'))));
});

test('W2-5: findVideoObjects survives a 10k-deep blob (no RangeError, partial results)', () => {
  let deep = { playable_url: 'https://video.x.fbcdn.net/deep.mp4' };
  for (let i = 0; i < 10000; i++) deep = { child: deep };
  const out = [];
  assert.doesNotThrow(() => findVideoObjects(deep, out, new Set()));
  assert.doesNotThrow(() => classifyFacebook({ mainWorld: [deep] }));
});

test('W2-5: findVideoObjects bounds a 50k-wide walk and keeps what it found first', () => {
  const wide = { first: { playable_url: 'https://video.x.fbcdn.net/wide.mp4' } };
  for (let i = 0; i < 50000; i++) wide['k' + i] = { n: i };
  const out = [];
  assert.doesNotThrow(() => findVideoObjects(wide, out, new Set()));
  assert.equal(out.length, 1);
  assert.equal(out[0].playable_url, 'https://video.x.fbcdn.net/wide.mp4');
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

/* ---- 2.3.5 audit F3: page-scan keying is identity-only ---- */

test('videoObjectId: only a digit id the object carries itself names it', () => {
  assert.equal(videoObjectId({ id: '1234567890123456' }), '1234567890123456');
  assert.equal(videoObjectId({ id: 1234567890123456 }), '1234567890123456');
  assert.equal(videoObjectId({ id: '123' }), '', 'too short to be an FB id');
  assert.equal(videoObjectId({ id: '12345extra' }), '', 'junk suffix names nothing');
  assert.equal(videoObjectId({ id: '' }), '');
  assert.equal(videoObjectId({}), '');
  assert.equal(videoObjectId(null), '');
  assert.equal(videoObjectId({ id: NaN }), '');
});

test('F3: an object keys under its OWN id; first carrier wins', () => {
  const main = F.COMET_VIDEO.data.video;
  const byId = keyOptionsByVideoId([main]);
  assert.deepEqual([...byId.keys()], ['1234567890123456']);
  assert.equal(byId.get('1234567890123456').length, 3, 'the DASH options of the fixture video');

  const first = {
    id: '5555555555555555',
    playable_url: 'https://video.fdhk.example.fbcdn.net/v/t42.1790-2/first_modelled.mp4?_nc_cat=100',
  };
  const second = {
    id: '5555555555555555',
    playable_url: 'https://video.fdhk.example.fbcdn.net/v/t42.1790-2/second_modelled.mp4?_nc_cat=100',
  };
  const dup = keyOptionsByVideoId([first, second]);
  assert.ok(dup.get('5555555555555555')[0].streams[0].url.includes('first_modelled'));
});

test('F3: an id-less sibling NEVER claims the page video identity', () => {
  // The permalink-page shape the old fallback mis-keyed: the
  // page's recommendations are serialised beside the main
  // video, and an id-less sibling appearing FIRST in scan
  // order used to be keyed to the page id — the picker then
  // offered the sibling's streams as the page video's.
  const sibling = {
    playable_url: 'https://video.fdhk.example.fbcdn.net/v/t42.1790-2/sibling_sd_modelled.mp4?_nc_cat=100',
    playable_url_quality_hd: 'https://video.fdhk.example.fbcdn.net/v/t42.1790-2/sibling_hd_modelled.mp4?_nc_cat=100',
  };
  const main = F.COMET_VIDEO.data.video;
  const byId = keyOptionsByVideoId([sibling, main]);
  assert.deepEqual([...byId.keys()], ['1234567890123456'], 'only the main video is keyed');
  const urls = byId.get('1234567890123456').flatMap(o => o.streams.map(s => s.url));
  assert.ok(urls.some(u => u.includes('1080p_modelled')), 'the main video keeps its own streams');
  assert.ok(!urls.some(u => u.includes('sibling')), 'no sibling stream under the page identity');
});

test('F3: an id-less object alone is keyed NOWHERE (never guessed)', () => {
  const orphan = {
    playable_url: 'https://video.fdhk.example.fbcdn.net/v/t42.1790-2/orphan_modelled.mp4?_nc_cat=100',
  };
  assert.equal(keyOptionsByVideoId([orphan]).size, 0);
  assert.equal(keyOptionsByVideoId([]).size, 0);
  assert.equal(keyOptionsByVideoId().size, 0);
});

test('F3: an id with no playable streams claims nothing; a later carrier can', () => {
  const bare = { id: '7777777777777777' };
  const full = {
    id: '7777777777777777',
    playable_url: 'https://video.fdhk.example.fbcdn.net/v/t42.1790-2/full_modelled.mp4?_nc_cat=100',
  };
  assert.equal(keyOptionsByVideoId([bare]).size, 0);
  const byId = keyOptionsByVideoId([bare, full]);
  assert.ok(byId.get('7777777777777777')[0].streams[0].url.includes('full_modelled'));
});
