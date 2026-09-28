// AR 私有 Node 子进程：凭据只从 stdin 接收，复用本机检测器采样与判定。
import { readFileSync, existsSync } from 'node:fs';
import { join } from 'node:path';
import { pathToFileURL } from 'node:url';
import { createInterface } from 'node:readline';

/** 输出单行事件；只调用此入口发送经过筛选的结果，不输出端点或凭据。 */
function emit(value) { process.stdout.write(`${JSON.stringify(value)}\n`); }

/** OpenCode/provider 和基准的厂商前缀不参与匹配；版本后缀必须完全一致。 */
export function modelId(value) { return value.split('/').at(-1); }

/** 同时核对有效采样、基准和真实响应 model；缺失回报不能当作通过。 */
export function passed(result, model, reported) {
  return result.verdict === 'match' && result.meanJsd !== null
    && result.meanJsd <= 0.25 && result.comparison.comparableCellCount >= 4
    && modelId(result.reference.model) === modelId(model)
    && reported.length === 1 && modelId(reported[0]) === modelId(model);
}

/** 定位 npm link / 全局安装产物，不执行 npm 下载或修改检测器源码。 */
function detectorPath() {
  const folders = [process.env.APPDATA && join(process.env.APPDATA, 'npm'),
    ...(process.env.PATH ?? '').split(';')].filter(Boolean);
  const found = folders.map(folder => join(folder, 'node_modules', 'llm-fingerprint-detector', 'dist'))
    .find(folder => existsSync(join(folder, 'index.js')));
  if (!found) throw new Error('未找到 llm-fingerprint-detector；请安装并执行 npm link。');
  return found;
}

/** 每次进程只处理一份配置；先校验基准，再向所选供应商发送 quick 采样。 */
async function main() {
  const reader = createInterface({ input: process.stdin });
  let input;
  for await (const line of reader) { input = JSON.parse(line); break; }
  reader.close();
  const folder = detectorPath();
  const { verify } = await import(pathToFileURL(join(folder, 'index.js')).href);
  const { listBundledReferences, loadBundledReference, parseFingerprintJson } =
    await import(pathToFileURL(join(folder, 'reference.js')).href);
  let reference;
  if (input.reference) {
    try { reference = parseFingerprintJson(readFileSync(input.reference, 'utf8')); }
    catch { throw new Error('基准 JSON 无法读取或格式无效。'); }
  } else {
    const refs = listBundledReferences().filter(ref => modelId(ref.model) === modelId(input.model));
    if (refs.length !== 1) throw new Error(`无唯一对应基准：${input.model}。请填写该模型的可信基准 JSON。`);
    reference = loadBundledReference(refs[0].id);
  }
  if (modelId(reference.model) !== modelId(input.model)) throw new Error('基准模型与所选模型不一致。');
  if (!input.endpoint) throw new Error(input.endpointError);
  // 只读取本次检测器请求的响应 metadata；检测器自身的 fingerprint.model 是请求值。
  const reported = new Set();
  let missingModel = false;
  const originalFetch = globalThis.fetch;
  globalThis.fetch = async (...args) => {
    const response = await originalFetch(...args);
    if (response.ok) {
      const data = await response.clone().json();
      if (typeof data.model === 'string' && data.model.trim()) reported.add(data.model.trim());
      else missingModel = true;
    }
    return response;
  };
  let result;
  try {
    result = await verify(input.endpoint, reference, {
      cells: 4, samplesPerCell: 15, timeoutMs: 30000, maxRetries: 1,
      signal: AbortSignal.timeout(180000),
      onProgress: event => emit({ type: 'progress', stage: event.stage, done: event.done, total: event.total }),
    });
  } catch (error) {
    // HTTP 错误可能含供应商原始 body；绝不把它带入日志或界面。
    const reason = error?.reason ?? error?.kind;
    throw new Error(reason === 'auth' ? 'API 鉴权失败，请检查当前供应商密钥。'
      : reason === 'aborted' ? '检测超时或已取消。'
      : 'API 检测失败；请确认供应商支持 /chat/completions、网络可达且模型可用。');
  } finally { globalThis.fetch = originalFetch; }
  const models = [...reported];
  emit({ type: 'result', passed: !missingModel && passed(result, input.model, models),
    model: input.model, reported: models, missingModel, reference: result.reference.model,
    verdict: result.verdict, meanJsd: result.meanJsd,
    comparableCells: result.comparison.comparableCellCount,
    protocolMismatch: result.comparison.protocolMismatch,
    postReasoning: result.target.fingerprint.postReasoning,
    errors: result.target.errorCount });
}

// 被单元测试导入时不读取 stdin；AR 以 node --input-type=module -e 启动。
if (process.argv.includes('--ar-model-quality')) {
  main().catch(error => { emit({ type: 'error', message: error.message }); process.exitCode = 1; });
}
