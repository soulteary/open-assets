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
  addEventListener(type, callback) { (this.listeners ||= {})[type] = callback; }
  focus() { this.focused = true; }
  fire(type, event={}) { this.listeners[type](event); }
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

test('restoration verification, explicit rollback and uncertain failures stay distinct', () => {
  const ui = setup(), data = fixture();
  data.restorations = {status:'ok', repositories:[{key:'assets',result_status:'ok',attempt:{status:'complete',phase:'complete',started_at:1,finished_at:2,objects_verified:true,verified_objects:2,files:2,with_local:true,local_verified:true,local_full_rebuilt:true,local_images_rebuilt:true,rollback_requested:false,activated:false}}]};
  ui.context.render(data);
  assert.match(ui.get('restore-repositories').textContent,/对象校验通过（当时）/);
  assert.match(ui.get('restore-repositories').textContent,/两份本地副本校验通过/);
  assert.match(ui.get('restore-repositories').textContent,/未请求显式回滚/);
  const a=data.restorations.repositories[0].attempt;
  Object.assign(a,{status:'failed',phase:'activate',rollback_requested:true,activation_uncertain:true,uncertain:true,scope_changed:true});
  ui.context.render(data);
  assert.match(ui.get('restore-repositories').textContent,/恢复失败/);
  assert.match(ui.get('restore-repositories').textContent,/切换状态不确定/);
  assert.match(ui.get('restore-repositories').textContent,/可能已有部分写入/);
  assert.match(ui.get('restore-repositories').textContent,/存储配置已变化/);
  Object.assign(a,{status:'complete',activated:true,activation_uncertain:false,uncertain:false});ui.context.render(data);
  assert.match(ui.get('restore-repositories').textContent,/显式回滚完成（当时）/);
});
test('missing legacy or unreadable restoration records never imply success', async () => {
  const ui=setup(),data=fixture();
  data.restorations={status:'ok',repositories:[{key:'none',result_status:'never'},{key:'bad',result_status:'unavailable'},{key:'legacy',result_status:'ok',attempt:{status:'running',phase:'git',started_at:1,uncertain:true}}]};
  ui.context.render(data);
  assert.match(ui.get('restore-repositories').textContent,/尚无恢复记录/);
  assert.match(ui.get('restore-repositories').textContent,/不能确认最近结果/);
  assert.match(ui.get('restore-repositories').textContent,/旧报告未记录本地模式/);
  assert.doesNotMatch(ui.get('restore-repositories').textContent,/校验通过|回滚完成/);
  ui.context.fetch=async()=>{throw Error('offline')};await ui.context.update();
  assert.match(ui.get('restore-notice').textContent,/无法刷新/);
});

test('restore checks distinguish current selection, barriers and unresolved evidence', () => {
  const ui=setup(),data=fixture();
  const a={status:'complete',diagnosis:'verified',started_at:1,finished_at:2,source_status:'running',files:2,verified_objects:2,current_is_target:false,published_is_target:false,checks:{objects:{status:'ok',reason:'verified'},full:{status:'ok',reason:'verified'},images:{status:'ok',reason:'verified'},references:{status:'ok',reason:'other-current'},barrier:{status:'ok',reason:'restored'}}};
  data.restoration_checks={status:'ok',repositories:[{key:'assets',result_status:'ok',attempt:a}]};
  ui.context.render(data);
  let text=ui.get('restore-check-repositories').textContent;
  assert.match(text,/核对通过（当时）/);assert.match(text,/原恢复：未结案/);assert.match(text,/其他版本当前在用/);assert.doesNotMatch(text,/显式回滚完成/);
  Object.assign(a,{diagnosis:'issues',references_changed:true,source_changed:true,stale:true});a.checks.barrier={status:'blocked',reason:'complete'};
  ui.context.render(data);text=ui.get('restore-check-repositories').textContent;
  assert.match(text,/核对发现问题/);assert.match(text,/仍有已删除屏障/);assert.match(text,/原恢复报告已变化/);assert.match(text,/current\/published 已变化/);assert.match(text,/超过 24 小时/);
});
test('unavailable and interrupted check reports do not imply successful reconciliation', async () => {
  const ui=setup(),data=fixture();
  data.restoration_checks={status:'ok',repositories:[{key:'none',result_status:'never'},{key:'bad',result_status:'unavailable'},{key:'running',result_status:'ok',attempt:{status:'running',diagnosis:'unresolved',started_at:1,observation_unavailable:true,checks:{}}}]};
  ui.context.render(data);const text=ui.get('restore-check-repositories').textContent;
  assert.match(text,/尚无核对记录/);assert.match(text,/不能确认结果/);assert.match(text,/核对未结案/);assert.match(text,/无法比较当前报告/);assert.doesNotMatch(text,/核对通过/);
  ui.context.fetch=async()=>{throw Error('offline')};await ui.context.update();assert.match(ui.get('restore-check-notice').textContent,/无法刷新/);
});

test('closure results keep original uncertainty and current selection explicit', () => {
  const ui=setup(),data=fixture();
  const a={status:'closed',reason:'verified',started_at:1,finished_at:2,source_status:'failed',source_uncertain:true,current_is_target:false,published_is_target:false};
  data.restoration_closures={status:'ok',repositories:[{key:'assets',result_status:'ok',attempt:a}]};ui.context.render(data);
  let text=ui.get('restore-close-repositories').textContent;
  assert.match(text,/已显式结案（当时）/);assert.match(text,/原报告的不确定状态保持原样/);assert.match(text,/保留其他当前版本/);assert.match(text,/未自动切换版本或解除屏障/);
  a.source_changed=true;a.references_changed=true;ui.context.render(data);text=ui.get('restore-close-repositories').textContent;
  assert.match(text,/此结案不涵盖新尝试/);assert.match(text,/结案是历史记录/);
  Object.assign(a,{status:'refused',reason:'check-failed'});ui.context.render(data);assert.match(ui.get('restore-close-repositories').textContent,/结案被拒绝/);assert.match(ui.get('restore-close-repositories').textContent,/重新核对未通过/);
});
test('uncompleted and unreadable closure audits do not fall back to success', async () => {
  const ui=setup(),data=fixture();data.restoration_closures={status:'ok',repositories:[{key:'never',result_status:'never'},{key:'bad',result_status:'unavailable'},{key:'interrupted',result_status:'ok',attempt:{status:'running',started_at:1}}]};ui.context.render(data);
  const text=ui.get('restore-close-repositories').textContent;assert.match(text,/尚无结案记录/);assert.match(text,/不能确认结果/);assert.match(text,/不能视为结案/);assert.doesNotMatch(text,/已显式结案/);
  ui.context.fetch=async()=>{throw Error('offline')};await ui.context.update();assert.match(ui.get('restore-close-notice').textContent,/无法刷新/);
});

test('closure audit growth suggests preview archival without hiding unfinished records', () => {
  const ui=setup(),data=fixture();data.restoration_closures={status:'ok',repositories:[{key:'near',result_status:'ok',online_attempts:450,archive_hint:'near-limit',attempt:{status:'running',started_at:1}},{key:'over',result_status:'unavailable',online_attempts_at_least:513,archive_hint:'scan-limit'}]};
  ui.context.render(data);const text=ui.get('restore-close-repositories').textContent;
  assert.match(text,/在线审计 450 条/);assert.match(text,/restore-audit-archive 预览归档/);assert.match(text,/未结案记录仍需保留/);assert.match(text,/不能视为结案/);
});

test('tabs support mouse and keyboard selection and preserve state across refresh', () => {
  const ui=setup();
  assert.equal(ui.get('panel-storage').hidden,false);assert.equal(ui.get('panel-audit').hidden,true);
  assert.equal(ui.get('tab-storage')['aria-selected'],'true');assert.equal(ui.get('tab-audit').tabIndex,-1);
  ui.get('tab-audit').fire('click');assert.equal(ui.get('panel-audit').hidden,false);assert.equal(ui.get('panel-storage').hidden,true);
  ui.context.render(fixture());assert.equal(ui.get('panel-audit').hidden,false);
  let prevented=false;ui.get('tab-audit').fire('keydown',{key:'ArrowRight',preventDefault(){prevented=true;}});
  assert.equal(prevented,true);assert.equal(ui.get('tab-operations').focused,true);assert.equal(ui.get('panel-operations').hidden,false);
  ui.get('tab-operations').fire('keydown',{key:'Home',preventDefault(){}});assert.equal(ui.get('panel-storage').hidden,false);
  ui.get('tab-storage').fire('keydown',{key:'End',preventDefault(){}});assert.equal(ui.get('panel-operations').hidden,false);
  assert.ok(html.indexOf('磁盘与文件系统')<html.indexOf('role="tablist"'));assert.ok(html.indexOf('证书有效期')<html.indexOf('role="tablist"'));
  assert.equal(detailPanels(ui).filter(id=>ui.get(id).hidden===false).length,1);
});
function detailPanels(){return ['storage','policy','restore','check','audit','operations'].map(k=>'panel-'+k);}
test('audit cached locations and protection reasons stay separate from actual archival results', () => {
  const ui=setup(),data=fixture();
  data.restoration_audits={status:'ok',repositories:[{key:'assets',status:'ok',stale:false,measured_at:1,online:{records:20,bytes:4096,disk_bytes:8192},archive:{records:7,bytes:1024,disk_bytes:4096},retired:{records:1,bytes:0,disk_bytes:4096},candidate_records:3,candidate_bytes:128,eligible_records:5,invalid_records:1,protection_counts:{unfinished:2,'keep-last':20,'unresolved-source':1},archival:{status:'ok',attempt:{status:'failed',started_at:1,finished_at:2,candidate_records:3,archived_records:1,protected_records:22,keep_last:1,min_age_days:0,max_records:3,partial_possible:true,failure_reason:'io'},last_complete:{finished_at:1,archived_records:2}}}]};
  ui.context.render(data);const text=ui.get('audit-repositories').textContent;
  assert.match(text,/20 条/);assert.match(text,/7 条/);assert.match(text,/逻辑 0 B/);assert.match(text,/磁盘 4.0 KiB/);assert.match(text,/未结案 2 条/);assert.match(text,/不确定证据未覆盖 1 条/);assert.match(text,/实际归档失败/);assert.match(text,/已完成 1/);assert.match(text,/上次完整执行/);assert.match(text,/清理部分暂存/);
});
test('missing, stale and busy audit measurements never render fabricated zeros', async () => {
  const ui=setup(),data=fixture();data.restoration_audits={status:'ok',repositories:[{key:'new',status:'never',stale:true,archival:{status:'never'}},{key:'busy',status:'busy',stale:true,measured_at:1,online:{records:2,bytes:1,disk_bytes:1},archival:{status:'unavailable'}}]};
  ui.context.render(data);const text=ui.get('audit-repositories').textContent;assert.match(text,/尚无完整采样/);assert.match(text,/锁忙跳过/);assert.match(text,/旧数据/);assert.match(text,/归档结果不可用/);assert.doesNotMatch(text,/0 条/);
  ui.context.fetch=async()=>{throw Error('offline')};await ui.context.update();assert.match(ui.get('audit-notice').textContent,/无法刷新/);
});

test('hidden detail failures stay visible as navigable attention hints', () => {
  const ui=setup(),data=fixture();data.restorations={status:'ok',repositories:[{key:'assets',result_status:'ok',attempt:{status:'failed',phase:'activate',started_at:1,uncertain:true}}]};
  ui.context.render(data);assert.equal(ui.get('panel-restore').hidden,true);assert.equal(ui.get('detail-attention').hidden,false);assert.match(ui.get('tab-restore').textContent,/提示/);assert.match(ui.get('detail-attention-links').textContent,/恢复未完成/);
  const link=ui.get('detail-attention-links').children.find(n=>n.textContent.startsWith('恢复结果'));
  link.fire('click');assert.equal(ui.get('panel-restore').hidden,false);assert.equal(ui.get('tab-restore').focused,true);
  assert.match(ui.get('restore-repositories').textContent,/阶段：切换引用/);
});
test('help text is collapsible and gives safe next steps without changing operations', () => {
  assert.ok((html.match(/<details class="section-help">/g)||[]).length>=6);
  assert.match(html,/check-restore KEY SHA/);assert.match(html,/几个面板可能对应不同提交/);
  assert.ok(html.indexOf('detail-attention')<html.indexOf('role="tablist"'));
});
test('tab fragments survive reload and use only local navigation', () => {
  const ui=setup();ui.context.location={hash:'#check'};const calls=[];
  ui.context.history={replaceState(_state,_title,value){calls.push(value);ui.context.location.hash=value;}};
  ui.context.initializeDetailTabs();assert.equal(ui.get('panel-check').hidden,false);
  ui.get('tab-audit').fire('click');assert.equal(ui.context.location.hash,'#audit');assert.deepEqual(calls,['#audit']);
});
