import { describe, expect, it } from 'vitest';

import {
  DEFAULT_TVG_ID_SOURCE,
  encodeSegment,
  discoverPath,
  guideUrl,
  hdhrUrl,
  playlistUrl,
  streamUrl,
  xtreamServer,
} from './connectUrls.js';

const BASE = 'http://dollet-relay:9191';

describe('encodeSegment', () => {
  /**
   * The exact bytes `urlencode` in `crates/dollet-server/src/api/mod.rs`
   * produces, asserted there too. These are the names that break it:
   * `encodeURIComponent` leaves `(` and `)` alone, so `Kids (2)` would be
   * offered under a path the server's own lineup never advertises.
   */
  it('produces the same bytes the server puts in its own lineup URLs', () => {
    expect(encodeSegment('Living Room')).toBe('Living%20Room');
    expect(encodeSegment('Kids (2)')).toBe('Kids%20%282%29');
  });

  it('leaves RFC 3986 unreserved characters alone, tilde included', () => {
    expect(encodeSegment('aZ09-_.~')).toBe('aZ09-_.~');
  });

  it('encodes the rest of what encodeURIComponent would keep', () => {
    expect(encodeSegment("!'()*")).toBe('%21%27%28%29%2A');
  });

  it('encodes non-ASCII as its UTF-8 bytes in uppercase hex', () => {
    expect(encodeSegment('Kanál')).toBe('Kan%C3%A1l');
  });
});

describe('the HDHomeRun tuner address', () => {
  it('ends in a slash, because Plex appends discover.json to it', () => {
    expect(hdhrUrl(BASE)).toBe('http://dollet-relay:9191/hdhr/');
  });

  it('covers all four tuner scopes', () => {
    expect(hdhrUrl(BASE, { channelProfile: 'Living Room' })).toBe(
      'http://dollet-relay:9191/hdhr/Living%20Room/',
    );
    expect(hdhrUrl(BASE, { outputProfile: 3 })).toBe(
      'http://dollet-relay:9191/hdhr/output_profile/3/',
    );
    expect(hdhrUrl(BASE, { channelProfile: 'Kids (2)', outputProfile: 3 })).toBe(
      'http://dollet-relay:9191/hdhr/Kids%20%282%29/output_profile/3/',
    );
  });

  it('asks for its identity on the same prefix, relative to this browser', () => {
    expect(discoverPath()).toBe('/hdhr/discover.json');
    expect(discoverPath({ channelProfile: 'Living Room', outputProfile: 3 })).toBe(
      '/hdhr/Living%20Room/output_profile/3/discover.json',
    );
  });
});

describe('the guide URL', () => {
  it('scopes to a channel profile by path segment', () => {
    expect(guideUrl(BASE)).toBe('http://dollet-relay:9191/output/epg');
    expect(guideUrl(BASE, { channelProfile: 'Living Room' })).toBe(
      'http://dollet-relay:9191/output/epg/Living%20Room',
    );
  });

  it('names a guide id source only when it is not the default', () => {
    // The server's `TvgIdSource::from_query` falls back to the channel number
    // for anything it does not recognise, so spelling the default out adds a
    // parameter that changes nothing.
    expect(guideUrl(BASE, { tvgIdSource: DEFAULT_TVG_ID_SOURCE })).toBe(
      'http://dollet-relay:9191/output/epg',
    );
    expect(guideUrl(BASE, { tvgIdSource: 'gracenote' })).toBe(
      'http://dollet-relay:9191/output/epg?tvg_id_source=gracenote',
    );
  });
});

describe('the playlist URL', () => {
  it('is bare when every toggle is at the server default', () => {
    expect(playlistUrl(BASE, { cachedLogos: true, direct: false })).toBe(
      'http://dollet-relay:9191/output/m3u',
    );
  });

  it('carries one parameter per toggle that differs from the default', () => {
    expect(playlistUrl(BASE, { outputProfile: 2 })).toBe(
      'http://dollet-relay:9191/output/m3u?output_profile=2',
    );
    expect(playlistUrl(BASE, { direct: true })).toBe(
      'http://dollet-relay:9191/output/m3u?direct=true',
    );
    expect(playlistUrl(BASE, { cachedLogos: false })).toBe(
      'http://dollet-relay:9191/output/m3u?cachedlogos=false',
    );
    expect(playlistUrl(BASE, { tvgIdSource: 'tvg_id' })).toBe(
      'http://dollet-relay:9191/output/m3u?tvg_id_source=tvg_id',
    );
  });

  it('combines a profile segment with every toggle at once', () => {
    expect(
      playlistUrl(BASE, {
        channelProfile: 'Kids (2)',
        outputProfile: 2,
        direct: true,
        cachedLogos: false,
        tvgIdSource: 'gracenote',
      }),
    ).toBe(
      'http://dollet-relay:9191/output/m3u/Kids%20%282%29' +
        '?output_profile=2&direct=true&cachedlogos=false&tvg_id_source=gracenote',
    );
  });
});

describe('the single-channel stream URL', () => {
  it('is what gets pasted into VLC', () => {
    expect(streamUrl(BASE, 'bbbbbbbb-0000-4000-8000-000000001000')).toBe(
      'http://dollet-relay:9191/proxy/ts/stream/bbbbbbbb-0000-4000-8000-000000001000',
    );
  });

  it('carries an output profile when one is asked for', () => {
    expect(streamUrl(BASE, 'abc', 4)).toBe(
      'http://dollet-relay:9191/proxy/ts/stream/abc?output_profile=4',
    );
  });
});

describe('the Xtream server fields', () => {
  it('splits the base the way a player form asks for it', () => {
    expect(xtreamServer('http://dollet-relay:9191')).toEqual({
      server: 'http://dollet-relay',
      port: '9191',
    });
  });

  it('spells out the port the scheme implies, because the field is mandatory', () => {
    expect(xtreamServer('https://tv.example')).toEqual({
      server: 'https://tv.example',
      port: '443',
    });
    expect(xtreamServer('http://tv.example')).toEqual({
      server: 'http://tv.example',
      port: '80',
    });
  });

  it('reports nothing rather than guessing at an unparseable base', () => {
    expect(xtreamServer('not a url')).toBeNull();
  });
});
