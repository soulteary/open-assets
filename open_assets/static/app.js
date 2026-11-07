'use strict';
const byId = id => document.getElementById(id);
const el = (tag, text, cls) => { const n = document.createElement(tag); if (text !== undefined) n.textContent = text; if (cls) n.className = cls; return n; };
const bytes = n => n == null ? '—' : n >= 1073741824 ? (n / 1073741824).toFixed(2) + ' GiB' : (n / 1048576).toFixed(1) + ' MiB';
const pct = n => n == null ? '—' : Number(n).toFixed(1) + '%';
const date = n => n ? new Date(n * 1000).toLocaleString() : '—';
function item(title, detail, value, warning) {
  const n = el('div', undefined, 'item');
  n.append(el('strong', title), el('span', detail, 'muted'), el('b', value, warning ? 'warning' : ''));
  return n;
}
function render(data) {
  byId('freshness').textContent = (data.stale ? '采集已过期 · ' : '更新于 ') + date(data.collected_at) + ' · ' + data.age_seconds + ' 秒前';
  byId('freshness').className = data.stale ? 'warning' : '';
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
  byId('disks').replaceChildren(...data.disks.map(d => item(d.label, '可用 ' + bytes(d.available) + ' / 总计 ' + bytes(d.total) + ' · inode ' + pct(d.inode_used_percent), '已用 ' + pct(d.used_percent), d.used_percent > 85)));
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
  catch { byId('error').hidden = false; byId('error').textContent = '无法读取最新状态，请检查登录会话与采集定时器。下方可能是上次成功读取的数据。'; }
}
update(); setInterval(update, 30000);
