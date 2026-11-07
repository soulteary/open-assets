'use strict';
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');
const {test} = require('node:test');
const html = fs.readFileSync(path.join(__dirname, '../open_assets/static/index.html'), 'utf8');
const script = fs.readFileSync(path.join(__dirname, '../open_assets/static/app.js'), 'utf8');
class Element {
  constructor() { this.children = []; this.text = ''; }
  set textContent(value) { this.text = String(value); this.children = []; }
  get textContent() { return this.text + this.children.map(child => child.textContent).join(' '); }
  set innerHTML(value) { throw new Error('HTML injection is forbidden'); }
  append(...children) { this.children.push(...children); }
  replaceChildren(...children) { this.text = ''; this.children = children; }
  setAttribute(name, value) { this[name] = value; }
}
function setup() {
  const elements = new Map();
  const document = {getElementById(id) { assert.ok(html.includes('id="' + id + '"'), 'Missing HTML element: ' + id); if (!elements.has(id)) elements.set(id, new Element()); return elements.get(id); }, createElement() { return new Element(); }};
  const context = vm.createContext({document, fetch: () => new Promise(() => {}), setInterval: () => {}});
  vm.runInContext(script, context);
  return {context, get: id => document.getElementById(id)};
}
function fixture() {
  return {collected_at: Date.now() / 1000, age_seconds: 0, stale: false,
    host: {cpu_count: 2, cpu_percent: 1, memory: {used: 1, available: 2, total: 3}, swap: {used: 0}, load: [0,0,0], uptime_seconds: 100, os: 'Fixture', kernel: 'fixture'},
    disks: [{label: '数据', available: 10e9, total: 20e9, used_percent: 50, inode_used_percent: 20}], components: [], certificates: [], collection_seconds: 1,
    capacity: {level: 'ok', alerts: []}, storage: {status: 'ok', cleanup_service: {status: 'success'}, repositories: [{key: 'assets', versions: 3, full_copies: 3, image_copies: 3, partial_versions: 0, unusual_entries: 0, local_bytes: 4096, git_bytes: 1024, version_bytes: 2048, trash_bytes: 0, staging_bytes: 0, publication_required_bytes: 2147483648, measured_at: Date.now()/1000, measurement_status: 'ok', measurement_stale: false, policy: {enabled: true, keep_last: 5, keep_days: 30, min_age_hours: 24, rollback_versions: 2, pinned_count: 1, max_delete_versions: 10, max_delete_bytes: 268435456, archive_jobs_after_days: 30, max_archive_jobs: 100}, cleanup: {status: 'complete', completed_at: Date.now()/1000, claimed_versions: 1, removed_directories: 2, archived_jobs: 4}}]}};
}
test('repository counts, policies and last complete cleanup render as text', () => {
  const ui = setup(); ui.context.render(fixture());
  assert.match(ui.get('repositories').textContent, /3 版/);
  assert.match(ui.get('repositories').textContent, /4.0 KiB/);
  assert.match(ui.get('repositories').textContent, /最近 5 版、30 天/);
  assert.match(ui.get('repositories').textContent, /删除暂存目录 2 个/);
  assert.match(ui.get('capacity-notice').textContent, /容量检查正常/);
});
test('critical capacity, stale snapshot and cleanup failure never report current success', () => {
  const ui = setup(), data = fixture();
  data.capacity = {level: 'critical', alerts: [{code: 'publication-reserve', key: 'assets', required_bytes: 2147483648}]};
  data.storage.cleanup_service.status = 'failed'; ui.context.render(data);
  assert.match(ui.get('capacity-notice').textContent, /容量不足/);
  assert.match(ui.get('cleanup-notice').textContent, /最近定时清理失败/);
  assert.match(ui.get('repositories').textContent, /完整执行于/);
  data.stale = true; ui.context.render(data);
  assert.doesNotMatch(ui.get('capacity-notice').textContent, /容量检查正常/);
  assert.match(ui.get('capacity-notice').textContent, /快照已过期/);
});
test('unavailable values are not rendered as zero and repository names cannot inject HTML', () => {
  const ui = setup(), data = fixture();
  data.storage.repositories = [{key: '<img src=x onerror=alert(1)>', measurement_status: 'unavailable', cleanup: {status: 'unavailable'}}];
  data.capacity = {level: 'unknown', alerts: []}; ui.context.render(data);
  assert.match(ui.get('repositories').textContent, /<img src=x onerror=alert\(1\)>/);
  assert.match(ui.get('repositories').textContent, /统计不可用/);
  assert.doesNotMatch(ui.get('repositories').textContent, /0 版/);
  assert.match(ui.get('repositories').textContent, /策略不可用/);
});

test('failed refresh replaces a previous green capacity notice', async () => {
  const ui = setup(); ui.context.render(fixture());
  ui.context.fetch = async () => { throw new Error('offline fixture'); };
  await ui.context.update();
  assert.doesNotMatch(ui.get('capacity-notice').textContent, /容量检查正常/);
  assert.match(ui.get('capacity-notice').textContent, /无法取得最新状态/);
  assert.equal(ui.get('error').hidden, false);
});
