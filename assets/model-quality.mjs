// AR 将固定版本的 ModelTrace 核心、挑战生成器与本文件组合为一个 Node 模块。
// 基准库和私有 API 配置均从 stdin 接收，不依赖 npm 包或用户自建指纹。
import { createInterface } from 'node:readline';

/** 事件仅包含进度和检测证据，不输出地址、密钥或上游错误正文。 */
function emit(event) { process.stdout.write(`${JSON.stringify(event)}\n`); }

/** 仅去除供应商命名空间；具体版本不作模糊归并。 */
export function modelId(model) { return model.split('/').at(-1); }

/** 三份有效回答完整归因后，所选、API 回报和识别结果一致即通过。 */
export function verdict(analysis, expectedModel, reported) {
  if (analysis.used_outputs !== 3) return 'insufficient';
  if (reported.length !== 3 || reported.some(model => !model)) return 'missing_model';
  if (reported.some(model => modelId(model) !== modelId(expectedModel))) return 'reported_mismatch';
  return analysis.prediction === modelId(expectedModel) ? 'match' : 'mismatch';
}

/** 按供应商声明的协议构造一个端点，不轮询或回退到另一套检测算法。 */
export function completionUrl(baseUrl, protocol) {
  const url = new URL(baseUrl);
  if (!['http:', 'https:'].includes(url.protocol) || url.username || url.password || url.search || url.hash) {
    throw new Error('供应商 API 地址格式不支持，请使用不含凭据或查询参数的 Base URL。');
  }
  const suffix = { chat: '/chat/completions', responses: '/responses', anthropic: '/messages' }[protocol];
  if (!suffix) throw new Error('供应商 API 协议不支持。');
  let path = url.pathname.replace(/\/+$/, '');
  if (!path.endsWith(suffix)) {
    // Messages 与上游 ModelTrace 一样补 /v1；OpenAI 保留已配置的路由前缀。
    if (!path || (protocol === 'anthropic' && !path.endsWith('/v1'))) path += '/v1';
    path += suffix;
  }
  url.pathname = path;
  return url.href;
}

/** 提取公开回答，排除思考块；截断、拒答与未完成响应不纳入指纹。 */
export function completionText(payload, protocol) {
  if (protocol === 'anthropic') {
    if (payload.stop_reason !== 'end_turn') return '';
    return (payload.content ?? []).filter(part => part.type === 'text').map(part => part.text).join('');
  }
  if (protocol === 'responses') {
    if (payload.status !== 'completed') return '';
    return (payload.output ?? []).filter(item => item.type === 'message' && item.role === 'assistant')
      .flatMap(item => item.content ?? []).filter(part => part.type === 'output_text')
      .map(part => part.text).join('');
  }
  const choice = payload.choices?.[0];
  if (choice?.finish_reason !== 'stop') return '';
  const content = choice.message?.content;
  return typeof content === 'string' ? content
    : Array.isArray(content) ? content.filter(part => part.type === 'text').map(part => part.text).join('') : '';
}

/** 三种传输协议共用同一套 ModelTrace 挑战；保持默认采样参数且禁用工具。 */
export function requestBody(endpoint, prompt) {
  if (endpoint.protocol === 'responses') {
    return { model: endpoint.model, input: [{ role: 'user', content: prompt }], stream: false, store: false };
  }
  const body = { model: endpoint.model, messages: [{ role: 'user', content: prompt }], stream: false };
  if (endpoint.protocol === 'anthropic') body.max_tokens = 4096;
  return body;
}

/** 单次 API 调用有界等待；只返回正文和真实响应模型，错误不携带供应商正文。 */
async function sample(endpoint, prompt, signal) {
  const headers = { 'Content-Type': 'application/json', Accept: 'application/json' };
  if (endpoint.protocol === 'anthropic') {
    headers['anthropic-version'] = '2023-06-01';
    if (endpoint.authType === 'bearer') headers.Authorization = `Bearer ${endpoint.apiKey}`;
    else headers['x-api-key'] = endpoint.apiKey;
  } else { headers.Authorization = `Bearer ${endpoint.apiKey}`; }
  let response;
  try {
    response = await fetch(completionUrl(endpoint.baseUrl, endpoint.protocol), {
      method: 'POST', headers, body: JSON.stringify(requestBody(endpoint, prompt)),
      signal, redirect: 'error',
    });
  } catch {
    throw new Error(signal.aborted ? '检测超时。' : 'API 连接失败，请检查地址、协议与网络。');
  }
  if (!response.ok) {
    throw new Error([401, 403].includes(response.status) ? 'API 鉴权失败，请检查所选供应商的密钥。'
      : `API 请求失败（HTTP ${response.status}），请检查所选模型与接口协议。`);
  }
  let payload;
  try { payload = await response.json(); }
  catch { throw new Error('API 未返回有效 JSON，请检查接口协议。'); }
  return { text: completionText(payload, endpoint.protocol),
    model: typeof payload.model === 'string' ? payload.model.trim() : '' };
}

/** 验证覆盖后执行三个独立挑战；不会用其它模型基准补位。 */
async function main() {
  const reader = createInterface({ input: process.stdin });
  let input;
  for await (const line of reader) { input = JSON.parse(line); break; }
  reader.close();
  const bank = input.bank;
  const expected = input.endpoint?.model ?? input.model;
  if (!bank.models.some(model => model.id === modelId(expected))) {
    throw new Error(`内置 ModelTrace 基准尚未收录 ${expected}，无法检测。`);
  }
  if (!input.endpoint) throw new Error(input.endpointError);
  const outputs = [], reported = [];
  const challenges = generateChallenges(3);
  const signal = AbortSignal.timeout(360000);
  for (const [index, challenge] of challenges.entries()) {
    emit({ type: 'progress', done: index, total: 3 });
    const reply = await sample(input.endpoint, challenge.prompt, signal);
    outputs.push({ text: reply.text, expected_count: challenge.expected_count });
    reported.push(reply.model);
    emit({ type: 'progress', done: index + 1, total: 3 });
  }
  // 与上游使用完全相同的有效样本门槛；不足三份时不作通过判定。
  const valid = outputs.filter(output => parseNumbers(output.text).length >= Math.max(80, Math.ceil(output.expected_count * 0.55)));
  const analysis = valid.length ? analyzeGlobalOutputs(outputs, bank) : { used_outputs: 0, prediction: '', probability: null, results: [] };
  const state = verdict(analysis, expected, reported);
  emit({ type: 'result', verdict: state, model: input.model, apiModel: expected,
    reported: [...new Set(reported.filter(Boolean))], prediction: analysis.used_outputs === 3 ? analysis.prediction : '',
    probability: analysis.used_outputs === 3 ? analysis.probability : null,
    usedOutputs: analysis.used_outputs, bankDate: bank.built_at,
    candidates: analysis.used_outputs === 3 ? analysis.results.slice(0, 3).map(item => ({ model: item.model, probability: item.probability })) : [] });
}

// 测试可导入纯函数；AR 的组合模块才读取 stdin 并发送请求。
if (process.argv.includes('--ar-model-quality')) {
  main().catch(error => { emit({ type: 'error', message: error.message }); process.exitCode = 1; });
}
