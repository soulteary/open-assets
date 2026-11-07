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
  cleanupNotice.textContent = ({failed: '最近定时清理失败；下表保留上次完整执行结果，请检查服务日志。', running: '定时清理正在运行；下表显示上次完整执行结果。', success: '定时进程上次正常退出；忙碌跳过也会正常退出，请以各仓库的完成时间和结果为准。', never: '定时清理尚未执行。', 'not-installed': '尚未安装定时清理服务；手工执行结果仍会在下表显示。', unavailable: '无法读取定时清理服务状态。'})[service];
  cleanupNotice.className = service === 'failed' || service === 'unavailable' ? 'warning' : 'muted';
  const rows = storage.repositories || [];
  const table = byId('repositories'); table.replaceChildren();
  if (!rows.length) {
    const tr = el('tr'), td = storageCell('暂时无法读取仓库统计'); td.colSpan = 5; tr.append(td); table.append(tr);
  }
  for (const row of rows) {
    const tr = el('tr');
    const old = data.stale || row.measurement_stale || row.measurement_status !== 'ok';
    if (old || row.policy_status === 'unavailable' || row.partial_versions || row.unusual_entries || (data.capacity?.alerts || []).some(alert => alert.key === row.key)) tr.className = 'warning';
    tr.append(storageCell(row.key, [row.measured_at ? '本地统计于 ' + date(row.measured_at) + (old ? '（旧数据 / 需复查）' : '') : '本地统计不可用']));
    tr.append(storageCell(row.versions == null ? '—' : row.versions + ' 版', ['完整快照 ' + (row.full_copies ?? '—') + ' / 图片副本 ' + (row.image_copies ?? '—'), '未配对 ' + (row.partial_versions ?? '—') + ' · 异常条目 ' + (row.unusual_entries ?? '—')]));
    tr.append(storageCell(bytes(row.local_bytes), ['版本 ' + bytes(row.version_bytes) + ' · Git ' + bytes(row.git_bytes), '清理暂存 ' + bytes(row.trash_bytes) + ' · 发布暂存 ' + bytes(row.staging_bytes), '发布预留 ' + bytes(row.publication_required_bytes)]));
    const policy = row.policy;
    tr.append(!policy || row.policy_status === 'unavailable' ? storageCell('策略不可用') : storageCell(policy.enabled ? '自动清理已启用' : '实际清理未启用', ['最近 ' + policy.keep_last + ' 版、' + policy.keep_days + ' 天（取并集）', '回滚 ' + policy.rollback_versions + ' 版 · 固定 ' + policy.pinned_count + ' 版 · 最少 ' + policy.min_age_hours + ' 小时', '每轮上限 ' + policy.max_delete_versions + ' 版 / ' + bytes(policy.max_delete_bytes) + '（逻辑）', '任务 ' + policy.archive_jobs_after_days + ' 天后归档，上限 ' + policy.max_archive_jobs + ' 份']));
    const cleanup = row.cleanup || {};
    const labels = {complete: '完整执行于 ' + date(cleanup.completed_at), disabled: '上次执行时未启用', never: '尚无执行记录', 'not-in-last-run': '最近一次未包含该仓库', unavailable: '结果不可用'};
    const details = cleanup.status === 'complete' ? ['移入清理暂存 ' + cleanup.claimed_versions + ' 版', '删除暂存目录 ' + cleanup.removed_directories + ' 个 · 归档任务 ' + cleanup.archived_jobs + ' 份'] : [];
    tr.append(storageCell(labels[cleanup.status] || '结果不可用', details)); table.append(tr);
  }
}
function render(data) {
  byId('freshness').textContent = (data.stale ? '采集已过期 · ' : '更新于 ') + date(data.collected_at) + ' · ' + data.age_seconds + ' 秒前';
  byId('freshness').className = data.stale ? 'warning' : '';
  renderStorage(data);
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
  catch { byId('error').hidden = false; byId('error').textContent = '无法读取最新状态，请检查登录会话与采集定时器。下方可能是上次成功读取的数据。'; const notice = byId('capacity-notice'); notice.className = 'capacity-notice capacity-unknown'; notice.replaceChildren(el('strong', '容量状态待确认'), el('p', '无法取得最新状态，下方保留上次采样，不能作为实时容量依据。')); }
}
update(); setInterval(update, 30000);
