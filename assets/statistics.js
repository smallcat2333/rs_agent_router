'use strict';

// 页面只消费本机统计 API；评分与任务控制由 Harness 的 CLI 完成。
const state = { sessions: [], sort: 'created_at_ms', direction: -1, page: 0, pending: false, ready: false, detailId: null };
const pageSize = 25;
const ids = ['search', 'date-from', 'date-to', 'cli-filter', 'model-filter', 'source-filter', 'app-filter', 'feat-filter', 'state-filter', 'review-filter', 'include-deleted'];
const labels = { running: '运行中', succeeded: 'CLI 已完成', failed: '失败', timed_out: '超时', cancelled: '已取消', interrupted: '中断', unavailable: '记录缺失', accepted: '审计通过', rework: '需要返工', unreviewed: '待评分' };

// 定位静态控件；缺失控件会在开发时直接暴露。
function byId(id) { return document.getElementById(id); }
// 动态内容统一按纯文本赋值，不把会话名称、评价或路径拼接为 HTML。
function element(tag, className, text) {
  const result = document.createElement(tag);
  if (className) result.className = className;
  if (text !== undefined && text !== null) result.textContent = String(text);
  return result;
}
// 与桌面页一致的紧凑耗时格式，小时级不再显示秒。
function duration(ms) {
  if (ms === null) return '—';
  const seconds = Math.floor(ms / 1000), minutes = Math.floor(seconds / 60), hours = Math.floor(minutes / 60);
  if (hours) return `${hours}h${minutes % 60 ? `${minutes % 60}m` : ''}`;
  if (minutes) return `${minutes}m${seconds % 60 ? `${seconds % 60}s` : ''}`;
  return `${seconds}s`;
}
// 缺失用量不当零，大数字使用便于比较的 k/m 单位。
function compact(value) {
  if (value === null) return '—';
  if (value >= 1000000) return `${(value / 1000000).toFixed(1)}m`;
  if (value >= 1000) return `${(value / 1000).toFixed(1)}k`;
  return String(value);
}
// 精确数值用于提示与证据；未知字段保留破折号。
function number(value) { return value === null ? '—' : new Intl.NumberFormat('zh-CN').format(value); }
// 展示本地时间，未结束任务没有结束时间。
function timestamp(value) { return value === null ? '未结束' : new Date(value).toLocaleString('zh-CN', { hour12: false }); }
// 用本地日期分组，与日期筛选控件采用同一口径。
function day(value) {
  const date = new Date(value);
  return `${date.getFullYear()}-${String(date.getMonth() + 1).padStart(2, '0')}-${String(date.getDate()).padStart(2, '0')}`;
}
// 协议允许未分组任务，筛选时保留可选名称。
function group(session, depth) { return session.group_path[depth] || '未分组'; }
// 模型 ID 大小写不构成新模型，筛选和汇总使用同一个键，明细保留原始配置。
function modelKey(session) { return session.model === null ? 'CLI 默认' : session.model.toLowerCase(); }
// 短时响应保留一位小数，缺失数据不显示为零。
function latency(ms) { return ms === null ? '—' : `${(ms / 1000).toFixed(1)}s`; }
// 明确区分等待加响应、响应流，以及排除工具阶段的 CLI 观察周期。
function timingSource(source) {
  return { claude_request: '等待 + 完整响应', claude_stream: '仅响应流，缺少等待时间', codex_reply_cycle: 'CLI 回复周期（排除工具阶段，非精确 API 计时）' }[source];
}
// 填充筛选选项并保留用户的当前选择。
function options(id, values) {
  const select = byId(id), previous = select.value;
  select.replaceChildren(element('option', '', '全部'));
  select.firstChild.value = '';
  for (const value of [...new Set(values)].sort((a, b) => a.localeCompare(b, 'zh-CN'))) {
    const option = element('option', '', value); option.value = value; select.append(option);
  }
  select.value = previous;
}
// 从完整会话集合建立筛选字典，切换“含已删除”不会意外重置其它筛选。
function updateOptions() {
  options('cli-filter', state.sessions.map(s => s.cli));
  options('model-filter', state.sessions.map(modelKey));
  for (const [id, depth] of [['source-filter', 0], ['app-filter', 1], ['feat-filter', 2]]) options(id, state.sessions.map(s => group(s, depth)));
}
// 所有卡片、图表和表格使用同一筛选结果；日期以会话创建时间为准。
function filtered() {
  const query = byId('search').value.trim().toLowerCase(), from = byId('date-from').value, to = byId('date-to').value;
  const invalid = from && to && from > to;
  byId('filter-error').hidden = !invalid;
  byId('filter-error').textContent = invalid ? '起始日期不能晚于结束日期。' : '';
  if (invalid) return [];
  return state.sessions.filter(s => {
    const created = day(s.created_at_ms);
    return (!s.deleted || byId('include-deleted').checked)
      && (!query || [s.title, s.task_id, ...s.group_path].join(' ').toLowerCase().includes(query))
      && (!from || created >= from) && (!to || created <= to)
      && (!byId('cli-filter').value || s.cli === byId('cli-filter').value)
      && (!byId('model-filter').value || modelKey(s) === byId('model-filter').value)
      && (!byId('source-filter').value || group(s, 0) === byId('source-filter').value)
      && (!byId('app-filter').value || group(s, 1) === byId('app-filter').value)
      && (!byId('feat-filter').value || group(s, 2) === byId('feat-filter').value)
      && (!byId('state-filter').value || s.state === byId('state-filter').value)
      && (!byId('review-filter').value || s.review_status === byId('review-filter').value);
  });
}
// 已知数值求和，完全没有用量报告时返回 null。
function knownTotal(rows) {
  const known = rows.map(s => s.tokens.total).filter(v => v !== null);
  return known.length ? known.reduce((a, b) => a + b, 0) : null;
}
// 摘要分母只使用已评分会话，缺失用量在卡片说明中可见。
function renderKpis(rows) {
  const graded = rows.filter(s => s.score !== null), running = rows.filter(s => s.state === 'running').length;
  const average = graded.length ? (graded.reduce((sum, s) => sum + s.score, 0) / graded.length).toFixed(1) : '—';
  const incomplete = rows.filter(s => !s.tokens.complete).length;
  const values = [
    ['会话总数', number(rows.length), `其中 ${running} 个运行中`],
    ['平均质量分', average, `${graded.length} 个已评分 / ${rows.length - graded.length} 个待评分`],
    ['返工派发', number(rows.reduce((sum, s) => sum + s.rework_count, 0)), '仅统计明确派发的返工'],
    ['累计执行耗时', duration(rows.reduce((sum, s) => sum + s.elapsed_ms, 0)), '不含轮次间等待'],
    ['已知 Token', compact(knownTotal(rows)), incomplete ? `${incomplete} 个会话的用量未完整报告` : '所有轮次均已报告'],
    ['审计通过', number(rows.filter(s => s.review_status === 'accepted').length), `${rows.filter(s => s.review_status === 'rework').length} 个需要返工`]
  ];
  byId('kpis').replaceChildren(...values.map(([label, value, note]) => {
    const card = element('article', 'kpi'); card.append(element('div', 'kpi-label', label), element('div', 'kpi-value', value), element('div', 'kpi-note', note)); return card;
  }));
}
// 绘制 1–10 的离散评分分布，无评分时明确显示等待审计。
function renderScores(rows) {
  const counts = Array(10).fill(0), graded = rows.filter(s => s.score !== null);
  for (const session of graded) counts[session.score - 1]++;
  byId('score-count').textContent = `${graded.length} 个当前已评分会话`;
  const container = byId('score-chart'); container.replaceChildren();
  if (!graded.length) { container.append(element('div', 'empty', '尚无评分 · 由 Harness 审计后录入')); return; }
  const max = Math.max(...counts, 1);
  counts.forEach((count, index) => {
    const column = element('div', `score-column ${index < 4 ? 'low' : index < 7 ? 'mid' : ''}`), track = element('div', 'bar-track'), bar = element('div', 'bar');
    bar.style.height = `${count / max * 100}%`; column.title = `${index + 1} 分：${count} 个会话`;
    track.append(bar); column.append(element('strong', '', count), track, element('small', '', index + 1)); container.append(column);
  });
}
// SVG 只用可信数值设置几何属性，文字通过 textContent 写入。
function svgElement(tag, attributes, text) {
  const result = document.createElementNS('http://www.w3.org/2000/svg', tag);
  for (const [key, value] of Object.entries(attributes)) result.setAttribute(key, String(value));
  if (text !== undefined) result.textContent = String(text);
  return result;
}
// 绘制每天已知 Token，未知会话不伪造为零消耗。
function renderDaily(rows) {
  const buckets = new Map();
  for (const session of rows) if (session.tokens.total !== null) { const key = day(session.created_at_ms); buckets.set(key, (buckets.get(key) || 0) + session.tokens.total); }
  const values = [...buckets].sort((a, b) => a[0].localeCompare(b[0])), container = byId('daily-chart'); container.replaceChildren();
  if (!values.length) { container.append(element('div', 'empty', '暂无已报告的 Token 用量')); return; }
  const width = 460, height = 174, left = 42, bottom = 145, top = 16, inner = width - left - 12, max = Math.max(...values.map(v => v[1]), 1);
  const svg = svgElement('svg', { viewBox: `0 0 ${width} ${height}`, role: 'img', 'aria-label': '每日已知 Token 用量' });
  for (const ratio of [0, .5, 1]) { const y = bottom - ratio * (bottom - top); svg.append(svgElement('line', { x1: left, y1: y, x2: width - 8, y2: y, stroke: '#2c3e52', 'stroke-dasharray': '3 4' }), svgElement('text', { x: left - 7, y: y + 4, fill: '#90a3ba', 'text-anchor': 'end', 'font-size': 10 }, compact(Math.round(max * ratio)))); }
  const stride = inner / values.length, barWidth = Math.max(2, Math.min(28, stride * .55));
  values.forEach(([date, count], index) => {
    const x = left + stride * (index + .5), barHeight = count / max * (bottom - top), bar = svgElement('rect', { x: x - barWidth / 2, y: bottom - barHeight, width: barWidth, height: Math.max(1, barHeight), rx: 3, fill: '#42c6b4' });
    bar.append(svgElement('title', {}, `${date}：${number(count)} Token`)); svg.append(bar);
    if (index === 0 || index === values.length - 1 || (values.length <= 7)) svg.append(svgElement('text', { x, y: height - 7, fill: '#90a3ba', 'text-anchor': 'middle', 'font-size': 10 }, date.slice(5)));
  });
  container.append(svg);
}
// 用量按 App 配置模型分组；这不是对供应商内部真实路由的推断。
function renderModels(rows) {
  const totals = new Map();
  for (const session of rows) if (session.tokens.total !== null) { const key = `${session.cli} / ${modelKey(session)}`; totals.set(key, (totals.get(key) || 0) + session.tokens.total); }
  const values = [...totals].sort((a, b) => b[1] - a[1]).slice(0, 8), container = byId('model-chart'); container.replaceChildren();
  if (!values.length) { container.append(element('div', 'empty', '暂无已报告的模型用量')); return; }
  const max = Math.max(...values.map(v => v[1]), 1);
  for (const [model, value] of values) { const row = element('div', 'model-row'), label = element('div', 'model-label'), track = element('div', 'model-track'), bar = element('div', 'model-bar'); label.append(element('span', '', model), element('strong', '', compact(value))); bar.style.width = `${value / max * 100}%`; row.title = `${model}：${number(value)}`; track.append(bar); row.append(label, track); container.append(row); }
}
// 状态值只用于标签文字和固定映射，未知状态仍可作为文本显示。
function chip(value) { return element('span', `chip ${Object.hasOwn(labels, value) ? value : ''}`, labels[value] || value); }
// 排序统一把未知评分/用量放在末尾，不把未评分等同最低分。
function sorted(rows) {
  const get = session => state.sort === 'tokens' ? session.tokens.total
    : ['first_text_ms', 'reply_mean_ms'].includes(state.sort) ? session.latency[state.sort]
    : state.sort === 'model' ? modelKey(session) : session[state.sort];
  return [...rows].sort((a, b) => { const av = get(a), bv = get(b); if (av === null && bv === null) return a.task_id.localeCompare(b.task_id); if (av === null) return 1; if (bv === null) return -1; return (typeof av === 'string' ? av.localeCompare(bv, 'zh-CN') : av - bv) * state.direction || a.task_id.localeCompare(b.task_id); });
}
// 渲染分页明细；所有数字显示口径与摘要一致。
function renderTable(rows) {
  const values = sorted(rows), pages = Math.max(1, Math.ceil(values.length / pageSize)); state.page = Math.min(state.page, pages - 1);
  byId('rows').replaceChildren(); byId('row-count').textContent = number(values.length);
  for (const session of values.slice(state.page * pageSize, (state.page + 1) * pageSize)) {
    const row = element('tr'); row.tabIndex = 0; row.dataset.taskId = session.task_id;
    const name = element('td'); name.append(element('div', 'task-name', session.title), element('div', 'subtext', session.group_path.join(' / ') || '未分组')); name.title = `${session.task_id}\n创建：${timestamp(session.created_at_ms)}`;
    const model = element('td'); model.append(element('div', '', `${session.cli} · ${session.model || 'CLI 默认'}`), element('div', 'subtext', `强度 ${session.effort || 'CLI 默认'}${session.archived ? ' · 已归档' : ''}${session.deleted ? ' · 已删除' : ''}`));
    const status = element('td'); status.append(chip(session.state));
    const elapsed = element('td', 'numeric', duration(session.elapsed_ms)); elapsed.title = `${number(session.elapsed_ms)} ms / ${session.turn_count} 轮`;
    const first = element('td', 'numeric', latency(session.latency.first_text_ms));
    first.append(element('div', 'subtext', session.latency.first_text_ms === null ? '未报告' : session.latency.first_text_source === 'completed_message' ? '首段' : '首字'));
    first.title = '本轮从 CLI 启动到首次可见文本；非流式 CLI 是完整首段抵达时间，不是 API TTFT。';
    const mean = element('td', 'numeric', latency(session.latency.reply_mean_ms));
    mean.append(element('div', 'subtext', `${session.latency.reply_samples.length} 条样本`));
    mean.title = session.latency.reply_samples.map((sample, index) => `近第 ${index + 1} 条 ${latency(sample.duration_ms)} · ${timingSource(sample.source)}`).join('\n') || '没有完整计时样本；不按任务总时长估算。';
    const tokens = element('td', 'numeric', compact(session.tokens.total)); if (session.tokens.total !== null && !session.tokens.complete) tokens.append(element('span', 'partial', '部分'));
    tokens.append(element('div', 'subtext', `入 ${compact(session.tokens.input)} / 出 ${compact(session.tokens.output)}`)); tokens.title = `缓存已包含在输入内：${number(session.tokens.cached)}\n完整报告 ${session.tokens.known_turns}/${session.tokens.completed_turns} 个完成轮次`;
    const score = element('td', 'numeric'); score.append(element('span', `score ${session.score === null ? 'unknown' : session.score < 5 ? 'low' : session.score < 8 ? 'mid' : ''}`, session.score === null ? '—' : session.score)); score.append(element('div', 'subtext', labels[session.review_status]));
    row.append(name, model, status, elapsed, first, mean, tokens, score, element('td', 'numeric', session.rework_count));
    row.addEventListener('click', () => showSession(session.task_id)); row.addEventListener('keydown', event => { if (event.key === 'Enter' || event.key === ' ') { event.preventDefault(); showSession(session.task_id); } }); byId('rows').append(row);
  }
  if (!values.length) { const row = element('tr', 'empty-row'), cell = element('td', 'empty', state.ready ? '没有符合筛选条件的会话' : '正在读取会话…'); cell.colSpan = 9; row.append(cell); byId('rows').append(row); }
  byId('page-info').textContent = `${state.page + 1} / ${pages} 页 · 每页 ${pageSize} 条`;
  byId('previous').disabled = state.page === 0; byId('next').disabled = state.page + 1 >= pages;
  document.querySelectorAll('[data-sort]').forEach(button => { const active = button.dataset.sort === state.sort; button.classList.toggle('active', active); button.parentElement.setAttribute('aria-sort', active ? state.direction > 0 ? 'ascending' : 'descending' : 'none'); });
}
// 共用渲染入口，确保筛选后的图表与明细不会各用不同的数据集。
function render() { const rows = filtered(); renderKpis(rows); renderScores(rows); renderDaily(rows); renderModels(rows); renderTable(rows); if (state.detailId && byId('detail').open) showSession(state.detailId); }
// 增加一块详情，标题和值都作为纯文本显示。
function section(title) { const result = element('section', 'detail-section'); result.append(element('h3', '', title)); return result; }
// 复制仅复制路径文字，网页不会启动本机文件或命令。
async function copyPath(path, button) { try { await navigator.clipboard.writeText(path); button.textContent = '已复制'; } catch { button.textContent = '请手动选择路径复制'; } }
// 详情展示首评和每轮审计，当前轮无评分时不会误用上一轮分数。
function showSession(taskId) {
  const session = state.sessions.find(s => s.task_id === taskId); if (!session) return;
  state.detailId = taskId; byId('detail-title').textContent = session.title;
  const body = byId('detail-body'); body.replaceChildren();
  const meta = element('div', 'detail-meta'); meta.append(chip(session.state), chip(session.review_status), element('span', '', `${session.cli} / ${session.model || 'CLI 默认'} / ${session.effort || '默认强度'}`), element('span', '', `返工 ${session.rework_count} 次`)); body.append(meta);
  body.append(element('p', 'small', `${session.task_id} · ${session.group_path.join(' / ') || '未分组'}`), element('p', 'small', `创建 ${timestamp(session.created_at_ms)} · 结束 ${timestamp(session.finished_at_ms)}`));
  const timing = section('响应耗时');
  timing.append(element('p', 'small', `本轮${session.latency.first_text_source === 'completed_message' ? '首段' : '首字'} ${latency(session.latency.first_text_ms)} · 近 ${session.latency.reply_samples.length} 次完整回复平均 ${latency(session.latency.reply_mean_ms)}`));
  for (const [index, sample] of session.latency.reply_samples.entries()) {
    const row = element('div', 'turn-row'); row.append(element('span', '', `近第 ${index + 1} 条 · ${timingSource(sample.source)}`), element('strong', '', latency(sample.duration_ms))); timing.append(row);
  }
  if (!session.latency.reply_samples.length) timing.append(element('p', 'small', '尚无完整计时样本；旧记录不回填估算值。'));
  body.append(timing);
  const scoreSection = section(`评分历史 · 首评 ${session.first_score === null ? '—' : session.first_score} / 当前 ${session.score === null ? '待评分' : session.score}`);
  if (!session.reviews.length) scoreSection.append(element('p', 'small', '尚无 Harness 审计记录。'));
  for (const review of session.reviews) {
    const card = element('article', 'review-card'), top = element('div', 'review-top'); top.append(element('strong', '', `第 ${review.turn} 轮 · ${review.score}/10`), chip(review.verdict), element('span', 'small', timestamp(review.created_at_ms))); card.append(top);
    const dims = element('div', 'dimensions'); dims.append(element('span', '', `正确性 ${review.dimensions.correctness}/4`), element('span', '', `完成度 ${review.dimensions.completeness}/3`), element('span', '', `规范 ${review.dimensions.compliance}/2`), element('span', '', `证据 ${review.dimensions.evidence}/1`)); card.append(dims, element('p', 'review-summary', review.summary));
    const evidence = element('ul', 'evidence'); for (const text of review.evidence) evidence.append(element('li', '', text)); card.append(evidence, element('div', 'small', `${review.reviewer} · ${review.review_id}`)); scoreSection.append(card);
  }
  body.append(scoreSection);
  const reworks = section(`返工记录 · ${session.rework_count} 次`);
  for (const rework of session.reworks) {
    const card = element('article', 'review-card'); card.append(element('strong', '', `第 ${rework.from_turn} 轮 → 第 ${rework.turn} 轮`), element('div', 'subtext', timestamp(rework.created_at_ms)), element('p', 'small', `关联审计 ${rework.review_id} · 派发 ${rework.request_id}`)); reworks.append(card);
  }
  if (!session.reworks.length) reworks.append(element('p', 'small', '尚无明确返工派发；普通补充指令不计入。'));
  body.append(reworks);
  const turns = section('执行轮次'); for (const turn of session.turns) { const row = element('div', 'turn-row'); row.append(element('span', '', `第 ${turn.turn} 轮 · ${labels[turn.state] || turn.state}`), element('span', '', `${duration(turn.duration_ms)} · 入 ${compact(turn.input_tokens)} / 出 ${compact(turn.output_tokens)}`)); turns.append(row); } body.append(turns);
  const paths = section('最新结果路径'); paths.append(element('code', 'path', session.result_path)); const copy = element('button', '', '复制路径'); copy.addEventListener('click', () => copyPath(session.result_path, copy)); paths.append(copy); body.append(paths);
  if (!byId('detail').open) byId('detail').showModal();
}
// 量表说明与服务端固定规则对应，页面不提供修改分数的入口。
function showRubric() {
  state.detailId = null; byId('detail-title').textContent = '统一评分 · ar-quality-v1'; const body = byId('detail-body'); body.replaceChildren();
  body.append(element('p', 'rubric-note', '总分 = max(1, 正确性 + 完成度 + 规范遵守 + 验收证据)，范围 1–10。未评分显示 —。质量评分不按速度或 Token 消耗扣分。'));
  const table = element('table', 'rubric-table');
  for (const [name, range, description] of [['正确性', '0–4', '从不可用/无法验证，到全部验收结果与必要边界检查正确。'], ['完成度', '0–3', '从未交付，到完整覆盖本次授权范围。'], ['规范遵守', '0–2', '从违规或无法验证，到符合项目规则与文件边界。'], ['验收证据', '0–1', '执行者是否提供可复核的差异、测试结论与交付说明。']]) { const row = element('tr'); row.append(element('td', '', name), element('td', '', range), element('td', '', description)); table.append(row); } body.append(table);
  body.append(element('p', 'rubric-note', '接受门槛：至少 8 分，正确性至少 3、完成度 3、规范至少 1、证据 1，且 Harness 确认无阻断问题。分数高仍可要求返工；CLI 成功不自动获得评分。'));
  body.append(element('p', 'rubric-note', '会话以 Router task_id 统计；当前评分属于当前执行轮次。普通 send 是补充指令，不计返工；明确 rework 派发被接收后才增加一次，重复请求不会重复计数。默认统计含归档、不含已删除；配置模型分组不等同于供应商内部路由推断。'));
  byId('detail').showModal();
}
// 只保留最新有效快照；服务不可用时显示错误并保留已取到的数据。
async function refresh() {
  if (state.pending) return; state.pending = true; byId('refresh').disabled = true;
  try {
    const response = await fetch('./api/statistics', { cache: 'no-store' }); if (!response.ok) throw new Error(`HTTP ${response.status}`);
    const data = await response.json(); if (data.schema_version !== 1 || !Array.isArray(data.sessions)) throw new Error('统计协议不匹配');
    state.sessions = data.sessions; state.ready = true; updateOptions(); render(); byId('updated').textContent = `更新于 ${timestamp(data.generated_at_ms)}`;
    byId('error').hidden = true; byId('connection').classList.remove('offline'); byId('connection').textContent = byId('auto-refresh').checked ? '已连接' : '已暂停自动刷新';
  } catch (error) {
    byId('error').hidden = false; byId('error').textContent = `无法读取管理器统计（${error.message}）。请从 App 重新打开统计页；已有数据保留。`;
    byId('connection').classList.add('offline'); byId('connection').textContent = '连接中断';
  } finally { state.pending = false; byId('refresh').disabled = false; }
}
// 所有交互只更改本页展示，不会评价、删除、取消或重新执行任务。
function bind() {
  for (const id of ids) byId(id).addEventListener('input', () => { state.page = 0; render(); });
  document.querySelectorAll('[data-sort]').forEach(button => button.addEventListener('click', () => { const key = button.dataset.sort; state.direction = state.sort === key ? -state.direction : -1; state.sort = key; state.page = 0; render(); }));
  byId('previous').addEventListener('click', () => { state.page--; render(); }); byId('next').addEventListener('click', () => { state.page++; render(); });
  byId('reset').addEventListener('click', () => { for (const id of ids) { if (byId(id).type === 'checkbox') byId(id).checked = false; else byId(id).value = ''; } state.page = 0; render(); });
  byId('refresh').addEventListener('click', refresh); byId('rubric').addEventListener('click', showRubric); byId('close-detail').addEventListener('click', () => byId('detail').close());
  byId('detail').addEventListener('close', () => { state.detailId = null; });
  byId('auto-refresh').addEventListener('change', () => { byId('connection').textContent = byId('auto-refresh').checked ? '正在恢复刷新' : '已暂停自动刷新'; if (byId('auto-refresh').checked) refresh(); });
  setInterval(() => { if (byId('auto-refresh').checked) refresh(); }, 5000);
}
bind(); render(); refresh();
