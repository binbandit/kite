const fs = require('node:fs');
const assert = require('node:assert/strict');
const test = require('node:test');
const path = require('node:path');
const lines = fs.readFileSync(path.join(__dirname, '../workflows/tidy-pullfrog-reviews.yml'), 'utf8').split('\n');
const start = lines.findIndex(line => /^ +script: \|$/.test(line));
assert.notEqual(start, -1, 'Missing inline cleanup script');
const indent = ' '.repeat(lines[start].indexOf('script:') + 2);
const scriptLines = [];
for (const line of lines.slice(start + 1)) {
  if (line.trim() && !line.startsWith(indent)) break;
  scriptLines.push(line.slice(indent.length));
}
const script = scriptLines.join('\n');
const AsyncFunction = Object.getPrototypeOf(async function() {}).constructor;
const review = (id, state = 'APPROVED', extra = {}) => ({
  id: String(id), databaseId: id, state, submittedAt: new Date(id * 1000).toISOString(),
  body: 'Unrecognized review format', isMinimized: false, viewerCanMinimize: true, commit: { oid: 'head' },
  author: { __typename: 'Bot', login: 'pullfrog' }, ...extra,
});
async function run({reviews, threads = [], head = 'head', automatic = false, pages = []}) {
  const changed = [];
  const queried = [];
  const github = {
    rest: { pulls: { list: 'list' } },
    paginate: { iterator: async function* () { for (const data of pages) yield { data }; } },
    graphql: async (query, {cursor, id, number}) => {
      if (query.includes('mutation')) { changed.push(id); return {}; }
      queried.push(number);
      const field = query.includes('reviews(first:') ? 'reviews' : 'reviewThreads';
      if (field === 'reviews') assert.match(query, /\bbody\b/, 'Review query must request body');
      const all = field === 'reviews' ? reviews : threads;
      const offset = cursor ? Number(cursor) : 0;
      const next = offset + 100;
      return { repository: { pullRequest: { headRefOid: head,
        [field]: { nodes: all.slice(offset, next), pageInfo: {
          hasNextPage: next < all.length, endCursor: String(next),
        } },
      } } };
    },
  };
  const context = { repo: { owner: 'test', repo: 'test' },
    eventName: automatic ? 'workflow_run' : 'workflow_dispatch', payload: {
      inputs: { pull_request: '18' }, workflow_run: { run_started_at: '2026-09-29T10:00:00Z' },
    } };
  const core = { info() {}, warning() {}, summary: { addRaw() { return this; }, async write() {} } };
  await new AsyncFunction('github', 'context', 'core', script)(github, context, core);
  return { changed, queried };
}
// Mirrors Pullfrog's default format and the sequence that accumulated on yip #26.
const summary = (verdict = 'ℹ️ No new issues.', finding = '') =>
  `> ${verdict}\n\n**Reviewed changes** Follow-up tests.\n\n` +
  `<!--\nPullfrog review metadata. These findings were written against abc123.\n-->\n` +
  `${finding}\n<!-- PULLFROG_DIVIDER_DO_NOT_REMOVE_PLZ -->\n<sup>Pullfrog</sup>`;
const thread = (id, isResolved = false) => ({
  isResolved, comments: {nodes: [{pullRequestReview: {id: String(id)}}]},
});

test('PR26: collapse updates and resolved findings while a body-only finding remains', async () => {
  const reviews = [
    review(1, 'COMMENTED', {body: summary('[!IMPORTANT]', '### ⚠️ Missing coverage\nAdd the remaining screens.')}),
    review(2, 'COMMENTED', {body: summary()}),
    review(3, 'COMMENTED', {body: ''}),
    review(4, 'COMMENTED', {body: summary()}),
    review(5, 'COMMENTED', {body: summary('[!IMPORTANT]')}),
    review(6, 'COMMENTED', {body: summary()}),
    review(7, 'COMMENTED', {body: ''}),
    review(8, 'COMMENTED', {body: summary()}),
  ];
  assert.deepEqual((await run({reviews, threads: [thread(1, true), thread(5, true)]})).changed,
    ['2', '3', '4', '5', '6', '7']);
});

test('keep unresolved inline feedback, even on informational or approved reviews', async () => {
  for (const state of ['COMMENTED', 'APPROVED']) {
    assert.deepEqual((await run({reviews: [review(1, state, {body: summary()}), review(2)],
      threads: [thread(1)]})).changed, []);
  }
});

test('keep body findings of every severity, malformed formats, and unanchored warnings', async () => {
  for (const body of [
    summary('ℹ️ Suggestions', '### ℹ️ Nitpicks\nUpdate documentation.'),
    summary('[!CAUTION]', '### 🚨 Migration order\nMigrate first.'),
    summary('[!IMPORTANT]'),
    '> ℹ️ Custom text with an actionable concern.',
    summary().replace('Pullfrog review metadata.', 'Custom metadata.'),
    summary().replace('<!-- PULLFROG_DIVIDER_DO_NOT_REMOVE_PLZ -->', ''),
    summary().replace('**Reviewed changes**', 'New body-only feedback'),
    summary().replace('<!--\nPullfrog review metadata.', '### ⚠️ Migration order\nMigrate first.\n\n<!--\nPullfrog review metadata.'),
    summary().replace('<!--\nPullfrog review metadata.', '<details><summary>Finding</summary>Fix this.</details>\n\n<!--\nPullfrog review metadata.'),
    undefined,
  ]) {
    assert.deepEqual((await run({reviews: [review(1, 'COMMENTED', {body}),
      review(2, 'COMMENTED', {body: summary()})]})).changed, []);
  }
});

test('keep blocking reviews until a later approval covers the current head', async () => {
  const reviews = [review(1, 'CHANGES_REQUESTED', {body: summary()}), review(2)];
  assert.deepEqual((await run({reviews, head: 'new-head'})).changed, []);
  assert.deepEqual((await run({reviews})).changed, ['1']);
});

test('empty thread replies do not displace the latest substantive summary', async () => {
  const reviews = [review(1, 'COMMENTED', {body: summary()}),
    review(2, 'COMMENTED', {body: summary()}), review(3, 'COMMENTED', {body: ''})];
  assert.deepEqual((await run({reviews})).changed, ['1']);
});

test('current-head approval retires unknown summaries; stale approval does not', async () => {
  const reviews = [review(1, 'COMMENTED'), review(2)];
  assert.deepEqual((await run({reviews})).changed, ['1']);
  assert.deepEqual((await run({reviews, head: 'new-head'})).changed, []);
});

test('unknown ownership of an open thread keeps all summaries visible', async () => {
  assert.deepEqual((await run({reviews: [review(1), review(2)],
    threads: [{isResolved: false, comments: {nodes: []}}]})).changed, []);
});

test('ignore people, pending reviews, and already minimized summaries', async () => {
  const reviews = [review(1, 'APPROVED', {author: {__typename: 'User', login: 'pullfrog'}}),
    review(2, 'PENDING'), review(3, 'APPROVED', {isMinimized: true}), review(4)];
  assert.deepEqual((await run({reviews})).changed, []);
});

test('paginate reviews and threads before deciding what is superseded', async () => {
  const reviews = Array.from({length: 102}, (_, i) => review(i + 1));
  assert.equal((await run({reviews})).changed.length, 101);
  const threads = Array.from({length: 101}, (_, i) => thread(1, i < 100));
  assert.deepEqual((await run({reviews: [review(1), review(2)], threads})).changed, []);
});

test('automatic cleanup includes recently merged PRs and stops at older updates', async () => {
  const result = await run({reviews: [review(1), review(2)], automatic: true, pages: [[
    {number: 18, state: 'closed', updated_at: '2026-09-29T10:01:00Z'},
    {number: 17, state: 'open', updated_at: '2026-09-28T10:01:00Z'},
  ]]});
  assert.deepEqual([...new Set(result.queried)], [18]);
});
