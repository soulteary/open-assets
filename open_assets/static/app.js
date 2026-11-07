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
function renderRestoreClosures(data) {
  const source=data.restoration_closures || {},notice=byId('restore-close-notice');
  notice.textContent=data.stale || source.status!=='ok' ? '结案摘要不可用或快照过期，请核对采集时间。' : '每仓库最近一次结案尝试；私有审计保留全部尝试，网页不提供结案操作。';
  notice.className=data.stale || source.status!=='ok' ? 'warning' : 'muted';
  const table=byId('restore-close-repositories');table.replaceChildren();
  const rows=source.repositories || [];
  if(!rows.length){const tr=el('tr'),td=storageCell('暂无结案摘要');td.colSpan=5;tr.append(td);table.append(tr);}
  const reasons={verified:'重新核对通过并显式结案','source-unavailable':'原恢复报告缺失、损坏或范围已变',ineligible:'原恢复不是未结案或状态不确定','check-failed':'重新核对未通过','references-missing':'没有一致的在用 current/published','evidence-changed':'核对期间证据或屏障发生变化',io:'审计或文件读写失败',permission:'权限不足',timeout:'超时',interrupted:'中断',unavailable:'未能确认',missing:'证据缺失',mismatch:'证据不一致',storage:'存储请求失败'};
  for(const row of rows){const tr=el('tr'),a=row.attempt;tr.append(storageCell(row.key));
    if(row.result_status!=='ok'||!a){const td=storageCell(row.result_status==='never'?'尚无结案记录':'结案报告不可用，不能确认结果');td.colSpan=4;tr.append(td);if(row.result_status!=='never')tr.className='warning';}
    else{
      tr.append(storageCell(({closed:'已显式结案（当时）',refused:'结案被拒绝',failed:'结案失败',running:'结案未完成 / 未结案'})[a.status]||'未知',['开始 '+date(a.started_at),a.finished_at?'结束 '+date(a.finished_at):'未记录结束时间']));
      tr.append(storageCell(a.source_status?({running:'原报告未结案',failed:'原报告失败',complete:'原报告完整结束'})[a.source_status]:'原证据未确认',[a.source_uncertain?'原报告的不确定状态保持原样':'原恢复报告不被改写','审计原因与原始快照仅在私有目录保存']));
      tr.append(storageCell(a.status==='closed'?'结案前重新核对通过':'未确认满足结案条件',[a.current_is_target===true?'核对时目标版本在用':a.current_is_target===false?'核对时保留其他当前版本':'目标是否在用：未知','未自动切换版本或解除屏障']));
      const notes=[];if(a.source_changed)notes.push('原恢复报告已变化，此结案不涵盖新尝试');if(a.scope_changed)notes.push('存储范围已变化');if(a.manifest_changed)notes.push('清单已变化');if(a.references_changed)notes.push('current/published 已变化；结案是历史记录');if(a.observation_unavailable)notes.push('无法比较当前证据，请人工核对');if(a.status==='running')notes.push('可能仍在执行或进程已中断，不能视为结案');
      tr.append(storageCell(reasons[a.reason]||'未记录最终结果',notes));
      if(a.status!=='closed'||a.source_changed||a.scope_changed||a.manifest_changed||a.references_changed||a.observation_unavailable)tr.className='warning';
    }table.append(tr);
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
      if(a.diagnosis!=='verified'||a.status!=='complete'||a.scope_changed||a.source_changed||a.manifest_changed||a.references_changed||a.observation_unavailable||a.stale)tr.className='warning';
    }
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
      tr.append(storageCell(({complete:'恢复完整结束', failed:'恢复失败', running:'运行中 / 尚未结案'})[a.status] || '未知', ['开始 ' + date(a.started_at), a.finished_at ? '结束 ' + date(a.finished_at) : '未记录结束时间', '阶段 ' + a.phase]));
      tr.append(storageCell(a.objects_verified ? '对象校验通过（当时）' : '对象尚未完整校验', ['已读回 ' + (a.verified_objects ?? '—') + ' / ' + (a.files ?? '—'), '上传 ' + (a.uploaded_objects ?? '—') + ' · 修复 ' + (a.repaired_objects ?? '—')]));
      tr.append(storageCell(a.with_local === false ? '未要求本地恢复' : a.local_verified ? '两份本地副本校验通过（当时）' : a.with_local ? '本地校验未完成' : '旧报告未记录本地模式', ['本次安装：public ' + (a.local_full_rebuilt === true ? '是' : a.local_full_rebuilt === false ? '否' : '未知') + ' / 图片 ' + (a.local_images_rebuilt === true ? '是' : a.local_images_rebuilt === false ? '否' : '未知')]));
      const notes = [];
      if (a.status === 'failed') notes.push('失败原因：' + ({validation:'校验或前提不满足',io:'文件或读写失败',permission:'权限不足',missing:'文件缺失',timeout:'超时',git:'Git 操作失败',storage:'存储请求失败',interrupted:'人工中断',unknown:'未分类异常'})[a.failure_reason || 'unknown'] + '；详细原因请核对私有报告');
      if (a.uncertain) notes.push('状态不确定：可能已有部分写入、安装或临时切换，请人工核对');
      if (a.scope_changed) notes.push('存储配置已变化，此结果不证明当前存储有效');
      tr.append(storageCell(a.activation_uncertain ? '切换状态不确定' : a.activated ? '显式回滚完成（当时）' : a.rollback_requested ? '请求显式回滚，未确认完成' : a.rollback_requested === false ? '未请求显式回滚' : '旧报告未记录回滚模式', notes));
      if (a.status !== 'complete' || a.uncertain || a.scope_changed) tr.className = 'warning';
    }
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
  renderObjectRetention(data);
  renderRestorations(data);
  renderRestoreChecks(data);
  renderRestoreClosures(data);
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
  byId('repairs').replaceChildren(...repairs.map(r => item(r.service, date(r.time) + ' · ' + (r.reason === 'persistent-unhealthy' ? '持续健康检查失败' : r.reason), ({restarted: '已请求重启', failed: '重启失败', skipped: '状态已变化，未重启', attempt: '处理中'})[r.result] || r.result, r.result === 'failed')));
  const performance = data.performance?.scenarios || [];
  byId('performance-section').hidden = performance.length === 0;
  if (performance.length) byId('performance-details').textContent = '完成于 ' + date(data.performance.collected_at) + ' · 固定并发与速率上限的读取测试，不代表容量上限。';
  byId('performance').replaceChildren(...performance.map(p => {
    const tr = el('tr');
    for (const v of [p.label, String(p.concurrency), p.requests + ' / ' + p.errors, p.requests_per_second.toFixed(1), [p.p50_ms, p.p95_ms, p.p99_ms].map(x => x.toFixed(1)).join(' / ')]) tr.append(el('td', v));
    return tr;
  }));
  byId('host-details').textContent = h.os + ' · 内核 ' + h.kernel + ' · 采集耗时 ' + data.collection_seconds.toFixed(2) + ' 秒';
}
async function update() {
  try { const r = await fetch('/api/status', {cache: 'no-store'}); if (!r.ok) throw new Error(); render(await r.json()); byId('error').hidden = true; }
  catch { byId('restore-close-notice').textContent = '结案结果无法刷新，下表可能是旧数据。'; byId('restore-close-notice').className = 'warning'; byId('restore-check-notice').textContent = '核对结果无法刷新，下表可能是旧数据。'; byId('restore-check-notice').className = 'warning'; byId('restore-notice').textContent = '恢复报告无法刷新，下表可能是旧结果，请复核采样时间。'; byId('restore-notice').className = 'warning'; byId('object-retention-notice').textContent = '对象保留数据无法刷新，下表保留旧结果，请复核采样时间。'; byId('object-retention-notice').className = 'warning'; byId('error').hidden = false; byId('error').textContent = '无法读取最新状态，请检查登录会话与采集定时器。下方可能是上次成功读取的数据。'; const notice = byId('capacity-notice'); notice.className = 'capacity-notice capacity-unknown'; notice.replaceChildren(el('strong', '容量状态待确认'), el('p', '无法取得最新状态，下方保留上次采样，不能作为实时容量依据。')); }
}
update(); setInterval(update, 30000);
