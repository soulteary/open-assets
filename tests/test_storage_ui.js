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
  assert.match(ui.get('object-retention-notice').textContent,/数据无法刷新/);
});

test('object totals show complete measurements, pending and failed scans keep explicit status', () => {
  const ui = setup(), data = fixture();
  data.object_storage = {status: 'ok', repositories: [{key: 'assets', status: 'ok', objects: 1005, bytes: 4096, started_at: Date.now()/1000-10, measured_at: Date.now()/1000, stale: false}]};
  ui.context.render(data);
  assert.match(ui.get('repositories').textContent, /1005 个 \/ 4.0 KiB/);
  assert.match(ui.get('repositories').textContent, /对象逻辑大小，非底层磁盘占用/);
  data.object_storage.repositories[0].status = 'scanning';
  ui.context.render(data);
  assert.match(ui.get('repositories').textContent, /分页采集中，显示上次完整结果/);
  assert.match(ui.get('repositories').textContent, /1005 个/);
  data.object_storage.repositories[0].status = 'unavailable';
  ui.context.render(data);
  assert.match(ui.get('repositories').textContent, /最近采集失败/);
  data.object_storage.repositories = [{key: 'assets', status: 'scanning', stale: true}];
  ui.context.render(data);
  assert.match(ui.get('repositories').textContent, /尚无完整统计/);
  assert.doesNotMatch(ui.get('repositories').textContent, /0 个 \/ 0 B/);
});

function retentionFixture() {
  const data = fixture(), now = Date.now()/1000;
  const plan = {mode:'preview',status:'complete',reason:'preview-only',started_at:now-2,finished_at:now,eligible_versions:4,eligible_objects:20,eligible_bytes:8192,candidate_versions:2,candidate_objects:10,candidate_bytes:4096,skip_counts:{'bytes-limit':2},skipped_versions:2,unknown_objects:1,removed_versions:0,removed_objects:0,removed_bytes:0,protection_counts:{current:1,rollback:2,pinned:1},stale:false,policy_changed:false};
  data.object_retention = {status:'ok',service:{status:'success'},repositories:[{key:'assets',result_status:'ok',policy:{enabled:false,exclusive_writer:false,keep_last:10,keep_days:90,min_age_hours:24,rollback_versions:2,pinned_count:1,max_delete_versions:2,max_delete_objects:100,max_delete_bytes:268435456},attempt:plan,last_plan:plan,last_preview:plan}]};
  return data;
}
test('object retention preview never appears as actual deletion and policy/candidate counts render', () => {
  const ui = setup(), data = retentionFixture(); ui.context.render(data);
  const text = ui.get('object-retention-repositories').textContent;
  assert.match(text,/实际删除未启用/); assert.match(text,/4 版 \/ 8.0 KiB/);
  assert.match(text,/单轮候选 2 版 \/ 10 个 \/ 4.0 KiB/);
  assert.match(text,/仅预览，未调用对象删除/); assert.match(text,/逻辑字节上限：2 版/);
  assert.match(text,/最近完整执行：暂无完整记录/);
  assert.doesNotMatch(text,/本次已确认删除/);
});
test('object retention failed apply retains last success and warns about partial deletion', () => {
  const ui = setup(), data = retentionFixture(), row = data.object_retention.repositories[0];
  row.last_apply = {...row.last_plan,mode:'apply',reason:'applied',removed_versions:1,removed_objects:3,removed_bytes:1024};
  row.attempt = {mode:'apply',status:'failed',reason:'storage-error',started_at:Date.now()/1000,partial_possible:true,removed_objects:1,removed_bytes:128};
  data.object_retention.service.status = 'failed';ui.context.render(data);
  assert.match(ui.get('object-retention-notice').textContent,/定时进程失败/);
  const text = ui.get('object-retention-repositories').textContent;
  assert.match(text,/可能有部分对象已删除/);assert.match(text,/已确认 1 个 \/ 128 B/);
  assert.match(text,/最近完整执行.*已确认删除 1 版 \/ 3 个/);
  assert.doesNotMatch(text,/本次未进入删除阶段/);
});
test('busy, configuration changes and corrupt or missing planning data are explicit', () => {
  const ui = setup(), data = retentionFixture(), row = data.object_retention.repositories[0];
  row.attempt = {mode:'preview',status:'busy',reason:'busy',started_at:Date.now()/1000};
  row.last_plan.policy_changed = true;row.last_plan.stale = true;ui.context.render(data);
  let text = ui.get('object-retention-repositories').textContent;
  assert.match(text,/锁忙，本次跳过/);assert.match(text,/配置已变更，需重新预览/);
  row.last_plan = undefined;row.invalid_fields = ['last_plan','last_apply'];ui.context.render(data);
  text = ui.get('object-retention-repositories').textContent;
  assert.match(text,/规划记录损坏或不可用/);assert.match(text,/最近完整执行：记录损坏/);
  assert.doesNotMatch(text,/0 版 \/ 0 B/);
});
