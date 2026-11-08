'use strict';
const byId = id => document.getElementById(id);
const el = (tag, text, cls) => { const n = document.createElement(tag); if (text !== undefined) n.textContent = text; if (cls) n.className = cls; return n; };
const bytes = n => n == null ? '—' : n >= 1073741824 ? (n / 1073741824).toFixed(2) + ' GiB' : n >= 1048576 ? (n / 1048576).toFixed(1) + ' MiB' : n >= 1024 ? (n / 1024).toFixed(1) + ' KiB' : n + ' B';
const pct = n => n == null ? '—' : Number(n).toFixed(1) + '%';
const date = n => n ? new Date(n * 1000).toLocaleString() : '—';
function item(title, detail, value, warning) {
  const n = el('div', undefined, 'item');
  n.append(el('strong', title), el('span', detail, 'muted'), el('b', value, warning ? 'warning' : ''));
  return n;
}
function storageCell(title, lines = []) {
  const td = el('td'); td.append(el('div', title));
  for (const line of lines) td.append(el('small', line, 'muted storage-detail'));
  return td;
}
function capacityText(alert, data) {
  const label = data.disks[alert.disk]?.label || '';
  const thresholds = data.capacity?.thresholds || {};
  const messages = {
    'space-warning': label + '：空间使用率达到 ' + thresholds.INFO_DISK_WARN_PERCENT + '% 提示阈值',
    'space-critical': label + '：空间使用率达到 ' + thresholds.INFO_DISK_CRITICAL_PERCENT + '% 严重阈值',
    'inode-warning': label + '：inode 使用率达到 ' + thresholds.INFO_INODE_WARN_PERCENT + '% 提示阈值',
    'inode-critical': label + '：inode 使用率达到 ' + thresholds.INFO_INODE_CRITICAL_PERCENT + '% 严重阈值',
    'free-warning': label + '：可用空间低于 ' + bytes(thresholds.INFO_DISK_WARN_FREE_BYTES),
    'free-critical': label + '：可用空间低于 ' + bytes(thresholds.INFO_DISK_CRITICAL_FREE_BYTES),
    'publication-reserve': alert.key + '：可用空间低于发布所需预留 ' + bytes(alert.required_bytes),
    'inode-unavailable': label + '：无法取得 inode 使用率',
    'storage-unavailable': '部分仓库统计无法更新，请核对下表的采样时间',
    'storage-stale': '仓库占用采样已过期，不能据此判断当前容量',
    'invalid-thresholds': '容量阈值配置无效，请检查采集配置'
  };
  return messages[alert.code] || '容量数据不可用，请检查采集状态';
}
const resourceLabels = {git_repository_bytes:'仓库 Git 缓存容量',git_total_bytes:'全局 Git 缓存容量',git_fetch_bytes:'单次 Git 拉取增长',git_min_free_bytes:'Git 磁盘可用空间',git_timeout_seconds:'Git 命令时限',git_process_memory_bytes:'Git 进程地址空间',git_output_bytes:'Git 元数据输出',scan_entries:'预算扫描目录项',unsafe_resource_entry:'Git 缓存存在不安全条目',queue_active_records:'活动队列数量',queue_total_records:'队列合计数量',queue_reserved_bytes:'队列预留字节'};
const doctorReason = value => ({ok:'最近执行成功',running:'正在执行','not-enabled':'未启用',failed:'执行失败','condition-skipped':'条件不满足，已跳过','never-or-unknown':'尚无可确认的执行记录',overdue:'超期未执行',unavailable:'状态不可读',fresh:'采样有效','never-or-invalid':'尚无有效样本','clock-skew':'采样时间超前',stale:'样本已过期','collector-busy':'采集锁忙','collector-scanning':'分页采集中','collector-never':'尚未采集','collector-unavailable':'采集失败','unavailable-or-scope-changed':'缓存不可用或配置范围变化'})[value]||'状态待核对';
const doctorCacheLabels = {'local-versions':'本地版本',objects:'对象统计',audits:'恢复审计',resources:'Git 与队列'};
function renderDoctor(data) {
  const source=data.diagnostics||{}, sample=source.report, notice=byId('doctor-notice');
  const old=data.stale||source.stale||source.status!=='ok'||['failed','unavailable','condition-skipped','not-installed','never'].includes(source.service?.status);
  const containers=[byId('doctor-checks'),byId('doctor-required'),byId('doctor-optional'),byId('doctor-caches')];
  for(const node of containers)node.replaceChildren();
  notice.className=old||sample?.status!=='ok'?'warning':'muted';
  if(!sample){notice.textContent='诊断结果不可用，不能判断为健康；请检查诊断采集定时器和日志。';return;}
  notice.textContent='诊断采样于 '+date(source.measured_at)+' · 年龄 '+source.age_seconds+' 秒 · 采集间隔 '+source.interval_seconds+' 秒 · '+(old?'旧结果 / 待复查；'+(source.service?.status==='failed'?'采集服务失败':source.status==='running'?'最近采集未完成':source.status==='unavailable'?'最近采集失败':'结果过期或采集状态异常'):sample.status==='ok'?'采样时检查通过':'采样时存在待处理项');
  for(const row of sample.checks||[])containers[0].append(el('p',row.check+'：'+(row.status==='ok'?'通过':'待处理')+'；'+row.hint,row.status==='ok'?'muted':'warning'));
  for(const row of sample.timers||[]){const tr=el('tr');const target=row.required?containers[1]:containers[2];
    const timing=row.age_seconds==null?'暂无完成年龄':'完成于采样前 '+row.age_seconds+' 秒 / 上限 '+row.max_age_seconds+' 秒';
    tr.append(storageCell(row.timer,[({active:'调度启用',inactive:'调度未启用',failed:'调度失败'})[row.active]||'调度状态待核对',row.required?'必需':'可选']),storageCell(doctorReason(row.reason),[timing,...(row.result?[row.result+(row.exit_code!=null?' · 退出码 '+row.exit_code:'')]:[])]),storageCell(row.hint));
    if(old||row.status==='attention')tr.className='warning';target.append(tr);
  }
  for(const row of sample.caches||[]){const tr=el('tr');tr.append(storageCell(doctorCacheLabels[row.cache]||row.cache,[row.key||'全局']),storageCell(doctorReason(row.reason),[row.age_seconds==null?'暂无有效采样年龄':'诊断时 '+row.age_seconds+' 秒 / 上限 '+row.max_age_seconds+' 秒']),storageCell(row.hint));if(old||row.status!=='ok')tr.className='warning';containers[3].append(tr);}
}
function renderResources(data) {
  const source=data.resources||{}, notice=byId('resource-notice'), global=byId('resource-global'), rows=byId('resource-repositories'), events=byId('resource-events');
  global.replaceChildren();rows.replaceChildren();events.replaceChildren();
  const valid=source.queue&&source.budget&&source.measured_at, old=data.stale||source.stale||source.status!=='ok'||source.service?.status==='failed';
  notice.className=old||(source.alerts||[]).length?'warning':'muted';
  notice.textContent=valid?'采样于 '+date(source.measured_at)+' · 间隔 '+source.interval_seconds+' 秒 · 提示阈值 '+source.warning_percent+'%'+(old?' · 旧数据 / 待复查；'+(source.service?.status==='failed'?'定时采集服务失败':({busy:'发布或队列锁忙',unavailable:'最近采集失败'})[source.status]||'采样已过期'):' · 当前采样有效'):'资源统计不可用，不能按零占用判断；请检查采集定时器。';
  if(valid){
    const q=source.queue,b=source.budget;
    for(const [label,used,budget,format] of [['活动记录',q.active_records,b.queue_active_records,String],['合计文件记录',q.total_records,b.queue_total_records,String],['队列预留',q.reserved_bytes,b.queue_reserved_bytes,bytes],['Git 缓存合计',source.git_bytes,b.git_total_bytes,bytes]])global.append(item(label,'预算 '+format(budget)+' · 使用率 '+pct(used/budget*100),format(used),used*100>=budget*source.warning_percent));
    global.append(el('p','队列实际逻辑字节 '+bytes(q.bytes)+' · 活动 '+q.locations.jobs+' / 归档 '+q.locations['job-archive']+' / 隔离 '+q.locations.quarantine,'muted'));
    for(const alert of source.alerts||[])global.append(el('p',(alert.key?alert.key+'：':'')+(resourceLabels[alert.code]||'资源预算')+(alert.level==='critical'?'已达到或超过预算':'接近预算')+(old?'（旧采样，仅供复核）':''),'warning'));
    for(const row of source.repositories||[]){const tr=el('tr');tr.append(storageCell(row.key),storageCell(bytes(row.bytes)),storageCell(bytes(b.git_repository_bytes)+' / '+pct(row.bytes/b.git_repository_bytes*100)));if(row.bytes*100>=b.git_repository_bytes*source.warning_percent||old)tr.className='warning';rows.append(tr);}
  }
  events.append(el('h3','最近超限记录（历史）'));
  if(source.event_status==='unavailable')events.append(el('p','超限记录不可用，不能确认最近原因。','warning'));
  else if(!Object.keys(source.events||{}).length)events.append(el('p','暂无已记录的超限事件；不能据此证明从未超限。','muted'));
  for(const kind of ['queue','git']){const e=source.events?.[kind];if(e)events.append(el('p',(kind==='queue'?'队列':'Git')+'：'+(resourceLabels[e.code]||'未知限制')+' · '+date(e.at)+(e.key?' · '+e.key:'')+'；请结合当前采样复核。'));}
}
function renderStorage(data) {
  const notice = byId('capacity-notice'); notice.replaceChildren();
  let level = data.stale ? 'unknown' : data.capacity?.level || 'unknown';
  const staleStorage = data.storage?.status !== 'ok' || (data.storage?.repositories || []).some(row => row.measurement_stale || row.measurement_status !== 'ok');
  if (level === 'ok' && staleStorage) level = 'warning';
  notice.className = 'capacity-notice capacity-' + level;
  notice.setAttribute('role', level === 'critical' ? 'alert' : 'status');
  notice.append(el('strong', ({ok: '容量检查正常', warning: '容量提示', critical: '容量不足或接近上限', unknown: '容量状态待确认'})[level] || '容量状态待确认'));
  if (data.stale) notice.append(el('p', '状态快照已过期，下方数据不能作为实时容量依据。'));
  else if (!data.capacity) notice.append(el('p', '等待采集器提供容量数据。'));
  else {
    const alerts = data.capacity.alerts || [];
    for (const alert of alerts.slice(0, 6)) notice.append(el('p', capacityText(alert, data)));
    if (alerts.length > 6) notice.append(el('p', '另有 ' + (alerts.length - 6) + ' 项提示，请核对各仓库的发布预留和文件系统使用情况。'));
    if (staleStorage && !alerts.some(alert => alert.code.startsWith('storage-'))) notice.append(el('p', '仓库统计不可用或过期，请按下表采样时间复查。'));
    if (level === 'ok') notice.append(el('p', '当前文件系统空间、inode 与发布预留均未触发阈值。此提示不替代外部容量告警。'));
  }
  const storage = data.storage || {};
  const service = storage.cleanup_service?.status || 'unavailable';
  const cleanupNotice = byId('cleanup-notice');
  cleanupNotice.textContent = '本地：' + ({'condition-skipped': '启动条件未满足，本地清理服务未运行。', failed: '最近定时清理失败；下表保留上次完整执行结果，请检查服务日志。', running: '定时清理正在运行；下表显示上次完整执行结果。', success: '定时进程上次正常退出；忙碌跳过也会正常退出，请以各仓库的完成时间和结果为准。', never: '定时清理尚未执行。', 'not-installed': '尚未安装定时清理服务；手工执行结果仍会在下表显示。', unavailable: '无法读取定时清理服务状态。'})[service];
  cleanupNotice.className = service === 'failed' || service === 'unavailable' ? 'warning' : 'muted';
  const rows = storage.repositories || [];
  const table = byId('repositories'); table.replaceChildren();
  if (!rows.length) {
    const tr = el('tr'), td = storageCell('暂时无法读取仓库统计'); td.colSpan = 6; tr.append(td); table.append(tr);
  }
  for (const row of rows) {
    const tr = el('tr');
    const old = data.stale || row.measurement_stale || row.measurement_status !== 'ok';
    if (old || row.policy_status === 'unavailable' || row.partial_versions || row.unusual_entries || (data.capacity?.alerts || []).some(alert => alert.key === row.key)) tr.className = 'warning';
    tr.append(storageCell(row.key, [row.measured_at ? '本地统计于 ' + date(row.measured_at) + (old ? '（旧数据 / 需复查）' : '') : '本地统计不可用']));
    tr.append(storageCell(row.versions == null ? '—' : row.versions + ' 版', ['完整快照 ' + (row.full_copies ?? '—') + ' / 图片副本 ' + (row.image_copies ?? '—'), '未配对 ' + (row.partial_versions ?? '—') + ' · 异常条目 ' + (row.unusual_entries ?? '—')]));
    tr.append(storageCell(bytes(row.local_bytes), ['版本 ' + bytes(row.version_bytes) + ' · Git ' + bytes(row.git_bytes), '清理暂存 ' + bytes(row.trash_bytes) + ' · 发布暂存 ' + bytes(row.staging_bytes), '发布预留 ' + bytes(row.publication_required_bytes)]));
    const objects = (data.object_storage?.repositories || []).find(record => record.key === row.key) || {};
    const objectOld = data.stale || objects.stale || objects.status !== 'ok';
    const objectLabels = {scanning: '分页采集中，显示上次完整结果', unavailable: '最近采集失败，需复查', never: '等待首次完整采集'};
    const objectDetails = [objects.measured_at ? '完整统计于 ' + date(objects.measured_at) + (objectOld ? '（旧数据 / 需复查）' : '') : '尚无完整统计', objectLabels[objects.status] || (objects.measured_at ? '采集开始于 ' + date(objects.started_at) : '对象统计不可用'), '对象逻辑大小，非底层磁盘占用'];
    const objectCell = storageCell(objects.objects == null ? '—' : objects.objects + ' 个 / ' + bytes(objects.bytes), objectDetails);
    if (objectOld || objects.objects == null) objectCell.className = 'warning';
    tr.append(objectCell);
    const policy = row.policy;
    tr.append(!policy || row.policy_status === 'unavailable' ? storageCell('策略不可用') : storageCell(policy.enabled ? '自动清理已启用' : '实际清理未启用', ['最近 ' + policy.keep_last + ' 版、' + policy.keep_days + ' 天（取并集）', '回滚 ' + policy.rollback_versions + ' 版 · 固定 ' + policy.pinned_count + ' 版 · 最少 ' + policy.min_age_hours + ' 小时', '每轮上限 ' + policy.max_delete_versions + ' 版 / ' + bytes(policy.max_delete_bytes) + '（逻辑）', '任务 ' + policy.archive_jobs_after_days + ' 天后归档，上限 ' + policy.max_archive_jobs + ' 份']));
    const cleanup = row.cleanup || {};
    const labels = {complete: '完整执行于 ' + date(cleanup.completed_at), disabled: '上次执行时未启用', never: '尚无执行记录', 'not-in-last-run': '最近一次未包含该仓库', unavailable: '结果不可用'};
    const details = cleanup.status === 'complete' ? ['移入清理暂存 ' + cleanup.claimed_versions + ' 版', '删除暂存目录 ' + cleanup.removed_directories + ' 个 · 归档任务 ' + cleanup.archived_jobs + ' 份'] : [];
    tr.append(storageCell(labels[cleanup.status] || '结果不可用', details)); table.append(tr);
  }
}
const retentionReasons = {
  'started': '尚未结案，请结合定时服务状态判断是否仍在运行',
  'preview-only': '仅预览，未调用对象删除', 'disabled': '策略未启用，本次未调用对象删除',
  'no-candidates': '没有可执行的候选，本次未删除', 'applied': '实际执行已完整结束',
  'budget-skipped': '部分版本因单轮策略或请求预算跳过', 'busy': '发布、入队或清理锁忙，本次跳过',
  'exclusive-writer-required': '未声明独占写入，执行已阻止', 'versioned-bucket': '桶启用或暂停了版本控制，不支持清理',
  'time-budget': '超过时间预算', 'scan-limit': '超过扫描、请求或元数据条目上限',
  'changed-objects': '对象、清单或收据不一致，需人工核对', 'permission-denied': '存储权限或凭据校验失败',
  'storage-error': '存储 API 失败', 'unsafe-state': '引用、任务、配置或元数据不安全',
  'io-error': '网络或本地 I/O 失败', 'unknown-error': '未分类错误，请检查私有服务日志'
};
const protectionLabels = {'current':'当前', 'rollback':'回滚', 'pinned':'固定', 'previous-job':'任务前一版本', 'keep-last':'最近数量', 'recent':'保留时间', 'no-current-reference':'无当前引用', 'missing-manifest':'无清单', 'incomplete-or-extra-objects':'对象不完整或额外对象', 'manifest-size-mismatch':'大小不符', 'job-pending':'待执行任务', 'job-running':'运行任务', 'job-images_prepared':'待切换任务', 'job-failed':'失败任务'};
function retentionHistory(row, name, label) {
  if (row.result_status === 'unavailable') return label + '：记录不可用，无法判断历史执行';
  if ((row.invalid_fields || []).includes(name)) return label + '：记录损坏 / 不可用';
  const value = row[name];
  if (!value) return label + '：暂无完整记录';
  return label + '：' + date(value.finished_at) + (name === 'last_apply' ? ' · 已确认删除 ' + value.removed_versions + ' 版 / ' + value.removed_objects + ' 个 / ' + bytes(value.removed_bytes) : ' · 仅预览') + ' · ' + (retentionReasons[value.reason] || '状态待核对') + (value.policy_changed ? '（配置已变更）' : value.stale ? '（历史记录，已过期）' : '');
}
const detailTabs = ['storage','resources','policy','restore','check','audit','diagnostics','operations'];
let activeDetailTab = 'storage';
const detailTabLabels = {storage:'仓库容量',resources:'Git 与队列',policy:'对象保留',restore:'恢复结果',check:'状态核对',audit:'结案与审计',diagnostics:'部署诊断',operations:'运维记录'};
const phaseLabel = value => ({validate:'前提检查',git:'读取 Git',export:'导出内容','local-preflight':'本地预检查',preflight:'对象预检查',upload:'上传对象',verify:'对象读回校验','local-install':'本地副本安装',activate:'切换引用',complete:'流程结束',objects:'对象核对',full:'完整副本核对',images:'图片副本核对',references:'引用核对',barrier:'清理屏障核对'})[value] || '未识别阶段';
function selectDetailTab(key, focus = false) {
  if (!detailTabs.includes(key)) return;
  activeDetailTab = key;
  for (const name of detailTabs) {
    const button=byId('tab-'+name), selected=name===key;
    button.setAttribute('aria-selected',String(selected)); button.tabIndex=selected?0:-1;
    byId('panel-'+name).hidden=!selected;
  }
  if (globalThis.location && globalThis.history && globalThis.location.hash !== '#'+key) globalThis.history.replaceState(null,'','#'+key);
  if(focus)byId('tab-'+key).focus();
}
function initializeDetailTabs() {
  for(const name of detailTabs){const button=byId('tab-'+name);
    button.addEventListener('click',()=>selectDetailTab(name));
    button.addEventListener('keydown',event=>{
      const index=detailTabs.indexOf(name);
      const target=event.key==='ArrowRight'?detailTabs[(index+1)%detailTabs.length]:event.key==='ArrowLeft'?detailTabs[(index+detailTabs.length-1)%detailTabs.length]:event.key==='Home'?detailTabs[0]:event.key==='End'?detailTabs[detailTabs.length-1]:null;
      if(target){event.preventDefault();selectDetailTab(target,true);}
    });
  }
  const fromHash = globalThis.location?.hash.slice(1);
  selectDetailTab(detailTabs.includes(fromHash) ? fromHash : activeDetailTab);
  globalThis.addEventListener?.('hashchange',()=>{const key=globalThis.location?.hash.slice(1);if(detailTabs.includes(key))selectDetailTab(key);});
}
function repairResolved(row,data) {
  const healthy=(data.components||[]).some(c=>c.service===row.service&&c.state==='running'&&c.health==='healthy'&&['ok','healthy','not-probed'].includes(c.probe));
  return !data.stale&&row.result==='failed'&&healthy&&(data.repairs||[]).some(r=>r.service===row.service&&r.time>row.time&&r.time<=data.collected_at-data.collection_seconds&&r.result==='restarted');
}
function recordState(row,kind,data={}) {
  if(data.stale)return 'active';
  if(row.result_status==='never')return 'none';
  if(row.result_status!=='ok'||!row.attempt)return 'active';
  if(['active','resolved','historical','none'].includes(row.reminder?.state))return row.reminder.state;
  const a=row.attempt,stable=!['scope_changed','source_changed','manifest_changed','observation_unavailable'].some(k=>a[k]);
  if(kind==='check'&&a.status==='complete'&&a.diagnosis==='verified'&&stable&&!a.source_uncertain)return a.references_changed||a.stale?'historical':'none';
  if(kind==='close'&&a.status==='closed'&&stable)return 'resolved';
  if(kind==='restore'&&a.status==='complete'&&!a.uncertain&&!a.scope_changed)return 'none';
  return 'active';
}
function recordGuidance(tr,row,kind,data) {
  const state=recordState(row,kind,data),a=row.attempt||{},key=/^[a-z][a-z0-9-]{0,47}$/.test(row.key)?row.key:'KEY';
  const td=tr.children[tr.children.length-1];
  const command='sudo ./scripts/open-assets ',check=command+'check-restore '+key+' FULL_SHA';
  let title='',notes=[],commands=[];
  if(state==='resolved'){
    title=row.reminder?.reason==='later-verified'?'已处理：后续核对通过，原失败记录保留':'已处理：已显式结案，原始记录保留';
    notes=['结案覆盖对应原恢复，不表示当前数据永久有效；无需重复结案。'];
  }else if(state==='historical'){
    title=row.reminder?.reason==='close-not-needed'?'历史记录：原恢复已完成，无需结案':'历史记录：当时核对通过，不代表当前状态';
    notes=['不是新的失败，也不能据此宣称已修复。需要确认当前状态时可重新核对。'];commands=[check];
  }else if(state==='active'){
    title='待处理：先核对证据，再决定操作';
    if(data.stale||row.result_status!=='ok'||!row.attempt){notes=['先运行 doctor，检查 Info 采集日志和报告读取错误；不要用旧结果判断已修复。'];commands=[command+'doctor'];}
    else if(row.reminder?.reason==='needs-close'||(kind==='check'&&a.diagnosis==='verified'&&a.source_uncertain&&!['stale','references_changed','source_changed','scope_changed','manifest_changed','observation_unavailable'].some(k=>a[k]))){
      notes=['数据核对通过，但原恢复仍未结案或不确定。人工复核后显式结案；命令会重新完整核对，不直接采纳缓存。'];commands=[command+'close-restore '+key+' FULL_SHA --reason "填写人工复核原因"'];
    }else{
      notes=['先重新核对对应版本，逐项查看失败原因。锁忙时等待已有任务结束；不要删除锁或原始报告。'];commands=[check];
      if(kind==='check'&&['objects','full','images','barrier'].some(k=>a.checks?.[k]&&a.checks[k].status!=='ok')){
        notes.push('确认对象或副本缺失后可从 Git 恢复；遇到内容冲突先调查，不强制覆盖。默认不切换版本。');commands.push(command+'restore-objects '+key+' FULL_SHA --with-local');
      }
      if(kind==='check'&&a.checks?.references?.status!=='ok')notes.push('current/published 不一致时先检查可信清单和切换记录，不手工改写其中一个文件来消除提示。');
      if(kind==='close')notes.push('核对通过且原恢复符合结案条件后，才运行 close-restore；已完整完成的恢复无需结案。');
      if(a.scope_changed||a.source_changed||a.manifest_changed)notes.push('配置或原始证据已变化，先确认应处理的那次恢复；旧结案不覆盖新尝试。');
    }
  }
  if(title)td.append(el('strong',title,'record-state'));
  for(const note of notes)td.append(el('small',note,'storage-detail'));
  for(const text of commands)td.append(el('code',text,'action-command'));
  if(commands.some(c=>c.includes('FULL_SHA')))td.append(el('small','FULL_SHA 须替换为这条报告对应的完整提交：在服务器 publisher/object-restores、restore-checks 或 restore-closures 对应仓库的私有报告中核对，不能直接使用当前 HEAD。','storage-detail'));
  if(state==='active')tr.className='warning';else if(state==='resolved'||state==='historical')tr.className='record-resolved';else tr.className='';
}
function renderDetailAttention(data) {
  const issues={};
  const any=(name,predicate)=>(data[name]?.repositories||[]).some(predicate);
  if(data.storage?.status!=='ok'||any('storage',r=>r.measurement_stale||r.measurement_status!=='ok'||r.partial_versions||r.unusual_entries)||any('object_storage',r=>r.stale||r.status!=='ok'))issues.storage='本地或对象统计待更新，或存在未配对/异常条目';
  if(data.resources?.status!=='ok'||data.resources?.stale||(data.resources?.alerts||[]).length||data.resources?.event_status==='unavailable'||data.resources?.service?.status==='failed')issues.resources='Git 或队列接近预算，或采样需要复核';
  if(data.object_retention?.status!=='ok'||any('object_retention',r=>r.result_status==='unavailable'||['failed','busy','running'].includes(r.attempt?.status)||r.last_plan?.stale||(r.invalid_fields||[]).length))issues.policy='对象规划有失败、跳过、未完成或过期记录';
  if(data.restorations?.status!=='ok'||any('restorations',r=>recordState(r,'restore',data)==='active'))issues.restore='恢复未完成、不确定或结果待核对；查看记录中的下一步操作';
  if(data.restoration_checks?.status!=='ok'||any('restoration_checks',r=>recordState(r,'check',data)==='active'))issues.check='存在未解决的核对或结案事项；查看记录中的下一步操作';
  if(data.restoration_audits?.status!=='ok'||any('restoration_audits',r=>r.status!=='ok'||r.stale||r.changed_since_sample||r.invalid_records||(r.retired?.records||0)>0||r.archival?.status==='unavailable'||(r.archival?.attempt&&r.archival.attempt.status!=='complete'))||any('restoration_closures',r=>r.archive_hint==='near-limit'||r.archive_hint==='scan-limit'||recordState(r,'close',data)==='active'))issues.audit='审计容量或结案仍需处理；先核对，再预览归档';
  if((data.repairs||[]).some(r=>r.result==='failed'&&!repairResolved(r,data))||(data.performance?.scenarios||[]).some(r=>r.errors>0))issues.operations='健康恢复或限量测试记录中存在失败';
  if(data.diagnostics?.status!=='ok'||data.diagnostics?.stale||data.diagnostics?.report?.status!=='ok'||['failed','unavailable','condition-skipped','not-installed','never'].includes(data.diagnostics?.service?.status))issues.diagnostics='诊断存在待处理项，或采集结果不可用/过期';
  const box=byId('detail-attention'),links=byId('detail-attention-links');links.replaceChildren();box.hidden=!Object.keys(issues).length;
  for(const key of detailTabs){const button=byId('tab-'+key);button.textContent=detailTabLabels[key]+(issues[key]?' · 提示':'');button.className=issues[key]?'tab-attention':'';
    if(issues[key]){const link=el('button',detailTabLabels[key]+'：'+issues[key]);link.type='button';link.addEventListener('click',()=>selectDetailTab(key,true));links.append(link);}}
}
function renderAuditStorage(data) {
  const source=data.restoration_audits||{}, notice=byId('audit-notice');
  notice.textContent=data.stale||source.status!=='ok'?'审计缓存不可用或主机快照过期，不能确认当前容量。':'容量默认每 15 分钟采样；最近实际归档结果来自独立记录，刷新和 tab 切换不扫描归档。';
  notice.className=data.stale||source.status!=='ok'?'warning':'muted';
  const table=byId('audit-repositories');table.replaceChildren();const rows=source.repositories||[];
  if(!rows.length){const tr=el('tr'),td=storageCell('暂无审计统计缓存');td.colSpan=6;tr.append(td);table.append(tr);}
  const protections={invalid:'坏记录 / 冲突',unfinished:'未结案','keep-last':'最近保留','unresolved-source':'不确定证据未覆盖',recent:'保留期内','batch-limit':'本轮数量上限'};
  for(const row of rows){const tr=el('tr'),old=row.stale||row.status!=='ok'||row.changed_since_sample;
    tr.append(storageCell(row.key,[row.measured_at?'采样 '+date(row.measured_at)+(old?'（旧数据 / 需复查）':''):'尚无完整采样','采集 '+({ok:'正常',busy:'锁忙跳过',unavailable:'失败',never:'尚未采样'})[row.status||'never'],...(row.changed_since_sample?['容量采样早于最近归档结果，等待下一次采样']:[])]));
    for(const location of ['online','archive','retired']){const value=row[location];tr.append(storageCell(value?value.records+' 条':'—',value?['逻辑 '+bytes(value.bytes),'磁盘 '+bytes(value.disk_bytes)]:['未取得完整统计']));}
    const lines=[];for(const [name,count] of Object.entries(row.protection_counts||{}))if(count)lines.push((protections[name]||'未知保护')+' '+count+' 条');
    if(row.invalid_records)lines.push('完整性异常 '+row.invalid_records+' 条，保留原证据');
    tr.append(storageCell(row.candidate_records==null?'规划不可用':'本轮候选 '+row.candidate_records+' 条 / '+bytes(row.candidate_bytes),['默认可归档总数 '+(row.eligible_records??'—')+' 条',...lines]));
    const archive=row.archival||{}, a=archive.attempt, details=[];
    if(a){details.push('开始 '+date(a.started_at));if(a.finished_at)details.push('结束 '+date(a.finished_at));details.push('候选 '+a.candidate_records+' / 已完成 '+a.archived_records+' / 保护 '+a.protected_records);details.push('参数：最近 '+a.keep_last+' 条 / '+a.min_age_days+' 天 / 单轮 '+a.max_records+' 条');if(a.partial_possible)details.push('可能已生成归档或移动/清理部分暂存，请核对后重试');for(const [name,count] of Object.entries(a.protection_counts||{}))if(count)details.push('本次保护：'+(protections[name]||'未知保护')+' '+count+' 条');if(a.failure_reason)details.push('失败原因：'+({missing:'文件缺失',permission:'权限不足',timeout:'超时',mismatch:'文件或证据不一致',io:'读写失败',storage:'存储请求失败',interrupted:'中断',unavailable:'无法确认'})[a.failure_reason]);if(archive.last_complete&&a.status!=='complete')details.push('上次完整执行 '+date(archive.last_complete.finished_at)+' · '+archive.last_complete.archived_records+' 条');}
    tr.append(storageCell(archive.status==='never'?'尚无实际归档记录':archive.status!=='ok'?'归档结果不可用':({running:'归档未结案 / 可能中断',complete:'实际归档完整结束',failed:'实际归档失败'})[a?.status]||'结果未知',details));
    if(old||row.invalid_records||archive.status==='unavailable'||(a&&a.status!=='complete')||(row.retired?.records||0)>0){tr.className='warning';const td=tr.children[tr.children.length-1],key=/^[a-z][a-z0-9-]{0,47}$/.test(row.key)?row.key:'KEY';td.append(el('small',old?'先刷新审计统计：sudo systemctl start open-assets-audits.service':'先校验已有归档，再预览候选；未结案和不确定证据继续保留。','storage-detail'));td.append(el('code','sudo ./scripts/open-assets restore-audit-verify '+key,'action-command'),el('code','sudo ./scripts/open-assets restore-audit-archive '+key,'action-command'));}table.append(tr);
  }
}
function renderRestoreClosures(data) {
  const source=data.restoration_closures || {},notice=byId('restore-close-notice');
  notice.textContent=data.stale || source.status!=='ok' ? '结案摘要不可用或快照过期，请核对采集时间。' : '每仓库最近一次结案尝试；私有审计保留全部尝试，网页不提供结案操作。';
  notice.className=data.stale || source.status!=='ok' ? 'warning' : 'muted';
  const table=byId('restore-close-repositories');table.replaceChildren();
  const rows=source.repositories || [];
  if(!rows.length){const tr=el('tr'),td=storageCell('暂无结案摘要');td.colSpan=5;tr.append(td);table.append(tr);}
  const reasons={verified:'重新核对通过并显式结案','source-unavailable':'原恢复报告缺失、损坏或范围已变',ineligible:'原恢复不是未结案或状态不确定','check-failed':'重新核对未通过','references-missing':'没有一致的在用 current/published','evidence-changed':'核对期间证据或屏障发生变化',io:'审计或文件读写失败',permission:'权限不足',timeout:'超时',interrupted:'中断',unavailable:'未能确认',missing:'证据缺失',mismatch:'证据不一致',storage:'存储请求失败'};
  for(const row of rows){const tr=el('tr'),a=row.attempt;tr.append(storageCell(row.key));
    if(row.result_status!=='ok'||!a){const td=storageCell(row.result_status==='never'?'尚无结案记录':row.archive_hint==='scan-limit'?'在线审计超过读取上限；请用 restore-audit-archive 预览归档，未结案记录仍需保留':'结案报告不可用，不能确认结果');td.colSpan=4;tr.append(td);if(row.result_status!=='never')tr.className='warning';}
    else{
      tr.append(storageCell(({closed:'已显式结案（当时）',refused:'结案被拒绝',failed:'结案失败',running:'结案未完成 / 未结案'})[a.status]||'未知',['开始 '+date(a.started_at),a.finished_at?'结束 '+date(a.finished_at):'未记录结束时间']));
      tr.append(storageCell(a.source_status?({running:'原报告未结案',failed:'原报告失败',complete:'原报告完整结束'})[a.source_status]:'原证据未确认',[a.source_uncertain?'原报告的不确定状态保持原样':'原恢复报告不被改写','审计原因与原始快照仅在私有目录保存']));
      tr.append(storageCell(a.status==='closed'?'结案前重新核对通过':'未确认满足结案条件',[a.current_is_target===true?'核对时目标版本在用':a.current_is_target===false?'核对时保留其他当前版本':'目标是否在用：未知','未自动切换版本或解除屏障']));
      const notes=[];if(row.online_attempts!=null)notes.push('在线审计 '+row.online_attempts+' 条 / 512 上限');if(row.archive_hint==='near-limit')notes.push('接近读取上限，可用 restore-audit-archive 预览归档');if(a.source_changed)notes.push('原恢复报告已变化，此结案不涵盖新尝试');if(a.scope_changed)notes.push('存储范围已变化');if(a.manifest_changed)notes.push('清单已变化');if(a.references_changed)notes.push('current/published 已变化；结案是历史记录');if(a.observation_unavailable)notes.push('无法比较当前证据，请人工核对');if(a.status==='running')notes.push('可能仍在执行或进程已中断，不能视为结案');
      tr.append(storageCell(reasons[a.reason]||'未记录最终结果',notes));
      if(a.status!=='closed'||a.source_changed||a.scope_changed||a.manifest_changed||a.references_changed||a.observation_unavailable)tr.className='warning';
    }recordGuidance(tr,row,'close',data);if(row.archive_hint==='near-limit'||row.archive_hint==='scan-limit'){tr.className='warning';const td=tr.children[tr.children.length-1];td.append(el('code','sudo ./scripts/open-assets restore-audit-archive '+(/^[a-z][a-z0-9-]{0,47}$/.test(row.key)?row.key:'KEY'),'action-command'));td.append(el('small','先预览归档，保护未结案证据；确认候选后再按文档决定是否执行。','storage-detail'));}table.append(tr);
  }
}
function renderRestoreChecks(data) {
  const source = data.restoration_checks || {}, notice = byId('restore-check-notice');
  notice.textContent = data.stale || source.status !== 'ok' ? '核对摘要不可用或快照过期，请检查采集时间。' : '每仓库最近启动的核对，目标可能不同于最近恢复；不会自动重新核验。';
  notice.className = data.stale || source.status !== 'ok' ? 'warning' : 'muted';
  const table = byId('restore-check-repositories'); table.replaceChildren();
  const rows = source.repositories || [];
  if (!rows.length) { const tr=el('tr'),td=storageCell('暂无核对摘要');td.colSpan=5;tr.append(td);table.append(tr); }
  const reasons = {'not-checked':'未检查',verified:'校验通过', 'target-current':'目标当前在用','other-current':'其他版本当前在用','not-published':'尚未发布',none:'无删除屏障',deleting:'仍有删除中屏障',complete:'仍有已删除屏障',restored:'恢复屏障已标记解除',missing:'缺失',permission:'权限不足',timeout:'超时',mismatch:'内容、元数据或引用不一致',io:'读写失败',storage:'存储请求失败',interrupted:'中断',unavailable:'无法确认'};
  const check = (a,name) => { const c=a.checks?.[name]; return c ? (({ok:'通过',blocked:'阻止',failed:'失败',unavailable:'待确认'})[c.status] || '未知') + ' · ' + (reasons[c.reason] || '未知原因') : '未检查'; };
  for (const row of rows) {
    const tr=el('tr'),a=row.attempt;tr.append(storageCell(row.key));
    if (row.result_status !== 'ok' || !a) { const td=storageCell(row.result_status === 'never' ? '尚无核对记录' : '核对报告不可用，不能确认结果');td.colSpan=4;tr.append(td);if(row.result_status!=='never')tr.className='warning'; }
    else {
      tr.append(storageCell(a.status === 'running' ? '核对未结案' : a.diagnosis === 'verified' && a.status === 'complete' ? '核对通过（当时）' : '核对发现问题 / 未完整完成', ['开始 '+date(a.started_at),a.finished_at ? '结束 '+date(a.finished_at) : '未记录结束时间','原恢复：'+({missing:'无报告',unavailable:'无法读取',running:'未结案',failed:'失败',complete:'完整结束'})[a.source_status || 'unavailable'] + (a.source_uncertain ? '（存在不确定状态，原报告未改写）' : '')]));
      tr.append(storageCell('对象：'+check(a,'objects'),['已读回 '+(a.verified_objects ?? '—')+' / '+(a.files ?? '—'),'public：'+check(a,'full'),'图片：'+check(a,'images')]));
      tr.append(storageCell(check(a,'references'),['current 指向目标：'+(a.current_is_target === true ? '是' : a.current_is_target === false ? '否' : '未知'),'published 指向目标：'+(a.published_is_target === true ? '是' : a.published_is_target === false ? '否' : '未知')]));
      const notes=[];
      if(a.scope_changed)notes.push('存储范围已变化，需要重新核对');
      if(a.source_changed)notes.push('原恢复报告已变化，需要重新核对');
      if(a.manifest_changed)notes.push('清单已变化，需要重新核对');
      if(a.references_changed)notes.push('current/published 已变化，旧引用结论不代表现状');
      if(a.observation_unavailable)notes.push('无法比较当前报告、清单或引用，请人工核对');
      if(a.stale)notes.push('核对已超过 24 小时，不能据此确认当前状态');
      if(a.status==='running')notes.push('可能仍在执行或已中断，未确认完成');
      notes.push('不改写原恢复结论、不自动修复');
      tr.append(storageCell(check(a,'barrier'),notes));
      if(a.diagnosis!=='verified'||a.status!=='complete'||a.source_uncertain||a.scope_changed||a.source_changed||a.manifest_changed||a.references_changed||a.observation_unavailable||a.stale)tr.className='warning';
    }
    recordGuidance(tr,row,'check',data);
    table.append(tr);
  }
}
function renderRestorations(data) {
  const source = data.restorations || {}, notice = byId('restore-notice');
  notice.textContent = data.stale || source.status !== 'ok' ? '恢复报告快照不可用或过期，请核对采集时间。' : '来自持久化报告；默认恢复不切换当前版本。';
  notice.className = data.stale || source.status !== 'ok' ? 'warning' : 'muted';
  const table = byId('restore-repositories'); table.replaceChildren();
  const rows = source.repositories || [];
  if (!rows.length) { const tr = el('tr'), td = storageCell('暂无可用恢复摘要'); td.colSpan = 5; tr.append(td); table.append(tr); }
  for (const row of rows) {
    const tr = el('tr'), a = row.attempt;
    tr.append(storageCell(row.key));
    if (row.result_status !== 'ok' || !a) {
      const td = storageCell(row.result_status === 'never' ? '尚无恢复记录' : '报告不可用，不能确认最近结果'); td.colSpan = 4; tr.append(td);
      if (row.result_status !== 'never') tr.className = 'warning';
    } else {
      tr.append(storageCell(({complete:'恢复完整结束', failed:'恢复失败', running:'运行中 / 尚未结案'})[a.status] || '未知', ['开始 ' + date(a.started_at), a.finished_at ? '结束 ' + date(a.finished_at) : '未记录结束时间', '阶段：' + phaseLabel(a.phase)]));
      tr.append(storageCell(a.objects_verified ? '对象校验通过（当时）' : '对象尚未完整校验', ['已读回 ' + (a.verified_objects ?? '—') + ' / ' + (a.files ?? '—'), '上传 ' + (a.uploaded_objects ?? '—') + ' · 修复 ' + (a.repaired_objects ?? '—')]));
      tr.append(storageCell(a.with_local === false ? '未要求本地恢复' : a.local_verified ? '两份本地副本校验通过（当时）' : a.with_local ? '本地校验未完成' : '旧报告未记录本地模式', ['本次安装：public ' + (a.local_full_rebuilt === true ? '是' : a.local_full_rebuilt === false ? '否' : '未知') + ' / 图片 ' + (a.local_images_rebuilt === true ? '是' : a.local_images_rebuilt === false ? '否' : '未知')]));
      const notes = [];
      if (a.status === 'failed') notes.push('失败原因：' + ({validation:'校验或前提不满足',io:'文件或读写失败',permission:'权限不足',missing:'文件缺失',timeout:'超时',git:'Git 操作失败',storage:'存储请求失败',interrupted:'人工中断',unknown:'未分类异常'})[a.failure_reason || 'unknown'] + '；详细原因请核对私有报告');
      if (a.uncertain) notes.push('状态不确定：可能已有部分写入、安装或临时切换，请人工核对');
      if (a.scope_changed) notes.push('存储配置已变化，此结果不证明当前存储有效');
      tr.append(storageCell(a.activation_uncertain ? '切换状态不确定' : a.activated ? '显式回滚完成（当时）' : a.rollback_requested ? '请求显式回滚，未确认完成' : a.rollback_requested === false ? '未请求显式回滚' : '旧报告未记录回滚模式', notes));
      if (a.status !== 'complete' || a.uncertain || a.scope_changed) tr.className = 'warning';
    }
    recordGuidance(tr,row,'restore',data);
    table.append(tr);
  }
}
function renderObjectRetention(data) {
  const source = data.object_retention || {}, notice = byId('object-retention-notice');
  const service = source.service?.status || 'unavailable';
  const states = {'condition-skipped':'定时服务启动条件未满足，该次未启动进程；手工尝试另见各仓库。', success:'定时进程上次正常退出；可能是预览或锁忙跳过，执行模式以各仓库记录为准。', failed:'对象保留定时进程失败；旧完整结果仍保留，不能据此判断本次未删除。', running:'对象保留定时进程运行中；下表显示历史完整规划及最新尝试。', never:'定时进程尚未执行。', 'not-installed':'尚未安装对象保留定时服务。', unavailable:'无法读取对象保留定时服务状态。'};
  notice.textContent = (data.stale ? '状态快照已过期。' : '') + (source.status === 'unavailable' ? '对象保留配置或摘要不可用。' : '') + (states[service] || states.unavailable);
  notice.className = data.stale || ['failed','unavailable'].includes(service) || source.status === 'unavailable' ? 'warning' : 'muted';
  const table = byId('object-retention-repositories'); table.replaceChildren();
  const rows = source.repositories || [];
  if (!rows.length) { const tr = el('tr'), td = storageCell('尚无可用对象保留摘要'); td.colSpan = 5; tr.append(td); table.append(tr); }
  for (const row of rows) {
    const tr = el('tr'), policy = row.policy, plan = row.last_plan, attempt = row.attempt;
    const old = data.stale || plan?.stale || row.result_status !== 'ok';
    if (old || (row.invalid_fields || []).length || ['failed','busy','running'].includes(attempt?.status)) tr.className = 'warning';
    tr.append(storageCell(row.key));
    tr.append(!policy ? storageCell('策略不可用') : storageCell(policy.enabled ? policy.exclusive_writer ? '策略启用 · 已声明独占写入' : '策略启用 · 未声明独占写入' : '实际删除未启用', ['最近 ' + policy.keep_last + ' 版 / ' + policy.keep_days + ' 天（取并集）· 最少 ' + policy.min_age_hours + ' 小时', '回滚保护 ' + policy.rollback_versions + ' 版 · 两类固定 SHA 去重 ' + policy.pinned_count + ' 版', '单轮 ' + policy.max_delete_versions + ' 版 / ' + policy.max_delete_objects + ' 个 / ' + bytes(policy.max_delete_bytes), '仍需专用 IAM 与显式 --apply']));
    const candidateLines = plan ? ['单轮候选 ' + plan.candidate_versions + ' 版 / ' + plan.candidate_objects + ' 个 / ' + bytes(plan.candidate_bytes), (plan.mode === 'preview' ? '预览规划于 ' : '执行前规划于 ') + date(plan.finished_at), plan.policy_changed ? '配置已变更，需重新预览' : old ? '历史采样，需复查' : '规划快照，不是实时剩余量', '预算排除或跳过 ' + plan.skipped_versions + ' 版 · 未知对象 ' + plan.unknown_objects + ' 个', ...Object.entries(plan.skip_counts || {}).map(([reason,count]) => ({'versions-limit':'版本数上限', 'objects-limit':'对象数上限', 'bytes-limit':'逻辑字节上限', 'request-budget':'请求预算不足'})[reason] + '：' + count + ' 版'), ...Object.entries(plan.protection_counts || {}).map(([reason,count]) => (protectionLabels[reason] || '其他保护') + ' ' + count)] : [row.result_status === 'unavailable' || (row.invalid_fields || []).includes('last_plan') ? '规划记录损坏或不可用' : '等待完整规划，不用零代替未知'];
    tr.append(storageCell(plan ? plan.eligible_versions + ' 版 / ' + bytes(plan.eligible_bytes) + '（' + plan.eligible_objects + ' 个）' : '—', candidateLines));
    const attemptLines = attempt ? [(attempt.mode === 'preview' ? '预览' : '执行请求') + ' · ' + date(attempt.started_at), retentionReasons[attempt.reason] || '状态待核对'] : [(row.invalid_fields || []).includes('attempt') ? '最近尝试记录损坏 / 不可用' : row.result_status === 'unavailable' ? '尝试记录不可用' : '尚无有效尝试记录'];
    if (attempt?.mode === 'apply' && attempt.status === 'complete') attemptLines.push('本次已确认删除 ' + attempt.removed_versions + ' 版 / ' + attempt.removed_objects + ' 个 / ' + bytes(attempt.removed_bytes));
    if (attempt?.partial_possible) attemptLines.push('可能有部分对象已删除，未结案；已确认 ' + (attempt.removed_objects ?? '—') + ' 个 / ' + bytes(attempt.removed_bytes) + '，也可能有响应未确认的删除');
    else if (attempt?.status === 'failed') attemptLines.push('本次未进入删除阶段');
    tr.append(storageCell(({complete:'完整结束', failed:'失败', busy:'跳过', running:'尝试未结案'})[attempt?.status] || '未知', attemptLines));
    tr.append(storageCell('完整历史结果', [retentionHistory(row, 'last_preview', '最近完整预览'), retentionHistory(row, 'last_apply', '最近完整执行')]));
    table.append(tr);
  }
}
function render(data) {
  byId('freshness').textContent = (data.stale ? '采集已过期 · ' : '更新于 ') + date(data.collected_at) + ' · ' + data.age_seconds + ' 秒前';
  byId('freshness').className = data.stale ? 'warning' : '';
  renderStorage(data);
  renderResources(data);
  renderDoctor(data);
  renderObjectRetention(data);
  renderRestorations(data);
  renderRestoreChecks(data);
  renderRestoreClosures(data);
  renderAuditStorage(data);
  renderDetailAttention(data);
  const h = data.host, overview = byId('overview'); overview.replaceChildren();
  for (const [label, value, detail] of [
    ['CPU', pct(h.cpu_percent), h.cpu_count + ' 个逻辑核 · 采集窗口总利用率'],
    ['内存', bytes(h.memory.used), '可用 ' + bytes(h.memory.available) + ' / 总计 ' + bytes(h.memory.total)],
    ['系统负载', h.load.map(x => x.toFixed(2)).join(' / '), '1 / 5 / 15 分钟'],
    ['连续运行', Math.floor(h.uptime_seconds / 86400) + ' 天', 'Swap 已用 ' + bytes(h.swap.used)]
  ]) { const c = el('article'); c.append(el('span', label, 'muted'), el('strong', value), el('small', detail)); overview.append(c); }
  const components = byId('components'); components.replaceChildren();
  for (const c of data.components) {
    const tr = el('tr');
    const version = c.image ? c.image.split('@')[0].split('/').pop() : '—';
    for (const value of [c.service, version, c.state + ' / ' + (c.health || '未配置') + (c.probe && c.probe !== 'not-probed' ? ' / ' + c.probe : ''), pct(c.cpu_percent), bytes(c.memory_bytes), String(c.restarts ?? '—')]) tr.append(el('td', value));
    if (c.state !== 'running' || c.health !== 'healthy' || !['ok', 'running', 'healthy', 'not-probed'].includes(c.probe)) tr.className = 'warning';
    components.append(tr);
  }
  byId('disks').replaceChildren(...data.disks.map((d, index) => item(d.label, '可用 ' + bytes(d.available) + ' / 总计 ' + bytes(d.total) + ' · inode ' + pct(d.inode_used_percent), '已用 ' + pct(d.used_percent), (data.capacity?.alerts || []).some(alert => alert.disk === index))));
  for (const [id, certificates] of [
    ['internal-certificates', data.certificates.filter(c => !c.label.startsWith('公网 /'))],
    ['public-certificates', data.certificates.filter(c => c.label.startsWith('公网 /'))]
  ]) byId(id).replaceChildren(...certificates.map(c => item(c.label, c.error ? '无法读取证书' : '到期 ' + date(c.not_after), c.error ? '检查失败' : Math.floor(c.days_remaining) + ' 天', c.error || c.days_remaining < 30)));
  const repairs = data.repairs || [];
  byId('repair-section').hidden = repairs.length === 0;
  byId('repairs').replaceChildren(...repairs.map(r => {
    const done=repairResolved(r,data), node=item(r.service,date(r.time)+' · '+(r.reason==='persistent-unhealthy'?'持续健康检查失败':r.reason),done?'已处理：后续已重启且采样健康':({restarted:'已请求重启',failed:'重启失败',skipped:'状态已变化，未重启',attempt:'处理中'})[r.result]||r.result,r.result==='failed'&&!done);
    if(done)node.className+=' record-resolved';
    else if(r.result==='failed')node.append(el('small','下一步：sudo ./scripts/open-assets doctor；查看 sudo journalctl -u open-assets-repair.service -n 50 --no-pager，先确认失败原因，不直接补跑重启。','storage-detail'));
    return node;
  }));
  const performance = data.performance?.scenarios || [];
  byId('operations-empty').hidden = repairs.length > 0 || performance.length > 0;
  byId('performance-section').hidden = performance.length === 0;
  if (performance.length) byId('performance-details').textContent = '完成于 ' + date(data.performance.collected_at) + ' · 固定并发与速率上限的读取测试，不代表容量上限。'+(performance.some(p=>p.errors>0)?'历史测试存在错误，不等于当前服务故障；先用 sudo ./scripts/open-assets doctor 检查服务与网络，再按性能文档安排限量复测。':'');
  byId('performance').replaceChildren(...performance.map(p => {
    const tr = el('tr');
    for (const v of [p.label, String(p.concurrency), p.requests + ' / ' + p.errors, p.requests_per_second.toFixed(1), [p.p50_ms, p.p95_ms, p.p99_ms].map(x => x.toFixed(1)).join(' / ')]) tr.append(el('td', v));
    return tr;
  }));
  byId('host-details').textContent = h.os + ' · 内核 ' + h.kernel + ' · 采集耗时 ' + data.collection_seconds.toFixed(2) + ' 秒';
}
async function update() {
  try { const r = await fetch('/api/status', {cache: 'no-store'}); if (!r.ok) throw new Error(); render(await r.json()); byId('error').hidden = true; }
  catch { byId('doctor-notice').textContent='诊断结果无法刷新，下方可能是旧数据，不能作为当前健康结论。';byId('doctor-notice').className='warning';byId('tab-diagnostics').textContent='部署诊断 · 提示';byId('tab-diagnostics').className='tab-attention'; byId('resource-notice').textContent='资源数据无法刷新，下方可能是旧数据。';byId('resource-notice').className='warning'; byId('audit-notice').textContent = '审计容量与归档结果无法刷新，下表可能是旧数据。'; byId('audit-notice').className = 'warning'; byId('restore-close-notice').textContent = '结案结果无法刷新，下表可能是旧数据。'; byId('restore-close-notice').className = 'warning'; byId('restore-check-notice').textContent = '核对结果无法刷新，下表可能是旧数据。'; byId('restore-check-notice').className = 'warning'; byId('restore-notice').textContent = '恢复报告无法刷新，下表可能是旧结果，请复核采样时间。'; byId('restore-notice').className = 'warning'; byId('object-retention-notice').textContent = '对象保留数据无法刷新，下表保留旧结果，请复核采样时间。'; byId('object-retention-notice').className = 'warning'; byId('error').hidden = false; byId('error').textContent = '无法读取最新状态，请检查登录会话与采集定时器。下方可能是上次成功读取的数据。'; const notice = byId('capacity-notice'); notice.className = 'capacity-notice capacity-unknown'; notice.replaceChildren(el('strong', '容量状态待确认'), el('p', '无法取得最新状态，下方保留上次采样，不能作为实时容量依据。')); }
}
initializeDetailTabs();
update(); setInterval(update, 30000);
