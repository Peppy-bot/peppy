import assert from 'node:assert/strict';
import { test } from 'node:test';
import { parseReleaseHtml } from './releaseHtmlLoader.ts';

test('parses an Atom release and removes its rendered header', () => {
  const source = `<entry>
  <published>2026-07-23T00:00:00Z</published>
  <updated>2026-07-24T00:00:00Z</updated>
  <summary>Fixes &amp; improvements</summary>
  <content type="html">&lt;article&gt;
    &lt;header&gt;&lt;h1&gt;v1.2.3&lt;/h1&gt;&lt;/header&gt;
    &lt;h2&gt;Changes&lt;/h2&gt;
  &lt;/article&gt;</content>
</entry>`;

  const release = parseReleaseHtml(source, new URL('file:///tmp/v1.2.3.html'));

  assert.deepEqual(release.data, {
    version: '1.2.3',
    date: new Date('2026-07-23T00:00:00Z'),
    updated: new Date('2026-07-24T00:00:00Z'),
    description: 'Fixes & improvements',
  });
  assert.equal(release.body.trim(), '<h2>Changes</h2>');
});

test('uses the updated date and content emphasis when optional Atom fields are absent', () => {
  const source = `<entry>
  <updated>2026-07-24T00:00:00Z</updated>
  <content type="html">&lt;article&gt;
    &lt;header&gt;&lt;p&gt;&lt;em&gt;Fallback description&lt;/em&gt;&lt;/p&gt;&lt;/header&gt;
    &lt;p&gt;Release body&lt;/p&gt;
  &lt;/article&gt;</content>
</entry>`;

  const release = parseReleaseHtml(source, new URL('file:///tmp/v2.0.0.html'));

  assert.equal(release.data.date.toISOString(), '2026-07-24T00:00:00.000Z');
  assert.equal(release.data.description, 'Fallback description');
  assert.equal(release.body.trim(), '<p>Release body</p>');
});

test('rejects an invalid release date with the source path', () => {
  const source = `<entry>
  <updated>not-a-date</updated>
  <summary>Invalid date</summary>
  <content type="html"></content>
</entry>`;

  assert.throws(
    () => parseReleaseHtml(source, new URL('file:///tmp/v3.0.0.html')),
    /Invalid Atom updated date "not-a-date" in \/tmp\/v3\.0\.0\.html/
  );
});

test('rejects malformed Atom XML with its location', () => {
  const source = '<entry><updated>2026-07-24T00:00:00Z</entry>';

  assert.throws(
    () => parseReleaseHtml(source, new URL('file:///tmp/v4.0.0.html')),
    /Invalid Atom XML in \/tmp\/v4\.0\.0\.html at line 1, column/
  );
});
