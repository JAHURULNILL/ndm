'use strict';
/* MODELLED fixture — built from public knowledge of Facebook page
 * structure (Comet preload field names, FB DASH MPD shape, fbcdn
 * URL patterns). NOT captured live data; no real signed URLs.
 * Live Facebook is unreachable from the build VM, so every test
 * using this fixture is FIXTURE evidence only. */

const DASH_MANIFEST = `<?xml version="1.0" encoding="UTF-8"?>
<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="static" mediaPresentationDuration="PT125S">
 <Period>
  <AdaptationSet mimeType="video/mp4" codecs="avc1.640028">
   <Representation id="v1080" bandwidth="3500000" width="1920" height="1080" codecs="avc1.640028" mimeType="video/mp4">
    <BaseURL>https://video.fdhk.example.fbcdn.net/v/t42.1790-2/1080p_modelled.mp4?_nc_cat=100&amp;oh=MODELLED</BaseURL>
   </Representation>
   <Representation id="v720" bandwidth="1800000" width="1280" height="720" codecs="avc1.64001f" mimeType="video/mp4">
    <BaseURL>https://video.fdhk.example.fbcdn.net/v/t42.1790-2/720p_modelled.mp4?_nc_cat=100&amp;oh=MODELLED</BaseURL>
   </Representation>
   <Representation id="v360" bandwidth="600000" width="640" height="360" codecs="avc1.42001e" mimeType="video/mp4">
    <BaseURL>https://video.fdhk.example.fbcdn.net/v/t42.1790-2/360p_modelled.mp4?_nc_cat=100&amp;oh=MODELLED</BaseURL>
   </Representation>
  </AdaptationSet>
  <AdaptationSet mimeType="audio/mp4" codecs="mp4a.40.2">
   <Representation id="a128" bandwidth="128000" codecs="mp4a.40.2" mimeType="audio/mp4">
    <BaseURL>https://video.fdhk.example.fbcdn.net/v/t42.1790-2/audio_modelled.m4a?_nc_cat=100&amp;oh=MODELLED</BaseURL>
   </Representation>
  </AdaptationSet>
 </Period>
</MPD>`;

const COMET_VIDEO = {
  data: {
    video: {
      id: '1234567890123456',
      playable_url: 'https://video.fdhk.example.fbcdn.net/v/t42.1790-2/sd_modelled.mp4?_nc_cat=100&oh=MODELLED',
      playable_url_quality_hd: 'https://video.fdhk.example.fbcdn.net/v/t42.1790-2/hd_modelled.mp4?_nc_cat=100&oh=MODELLED',
      browser_native_hd_url: 'https://video.fdhk.example.fbcdn.net/v/t42.1790-2/hd_modelled.mp4?_nc_cat=100&oh=MODELLED',
      browser_native_sd_url: 'https://video.fdhk.example.fbcdn.net/v/t42.1790-2/sd_modelled.mp4?_nc_cat=100&oh=MODELLED',
      dash_manifest: DASH_MANIFEST,
    },
  },
};

const COMET_VIDEO_PROGRESSIVE_ONLY = {
  require: [[ 'VideoPlayerOz', null, null, [{
    video: {
      id: '9876543210987654',
      playable_url: 'https://video.fdhk.example.fbcdn.net/v/t42.1790-2/sd2_modelled.mp4?_nc_cat=100&oh=MODELLED',
      playable_url_quality_hd: 'https://video.fdhk.example.fbcdn.net/v/t42.1790-2/hd2_modelled.mp4?_nc_cat=100&oh=MODELLED',
    },
  }]]],
};

const PERFORMANCE_ENTRIES = [
  { name: 'https://video.fdhk.example.fbcdn.net/v/t42.1790-2/123456789012345_720p_modelled.mp4?_nc_cat=100', initiatorType: 'fetch' },
  { name: 'https://video.fdhk.example.fbcdn.net/v/t42.1790-2/123456789012345_unknown_modelled.mp4?_nc_cat=100', initiatorType: 'xmlhttprequest' },
  { name: 'https://video.fdhk.example.fbcdn.net/o1/v/t2/f2/m69/dash/segment-3.m4s?_nc_cat=100', initiatorType: 'fetch' },
  { name: 'https://static.example.fbcdn.net/rsrc.php/v4/image.png', initiatorType: 'img' },
  { name: 'https://example.com/not-fb.mp4', initiatorType: 'video' },
];

module.exports = { DASH_MANIFEST, COMET_VIDEO, COMET_VIDEO_PROGRESSIVE_ONLY, PERFORMANCE_ENTRIES };
