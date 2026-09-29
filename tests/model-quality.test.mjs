// 固定上游样本用于算法回归；本地模拟 API 验证整个三挑战流程，不消耗真实账号额度。
import test from 'node:test';
import assert from 'node:assert/strict';
import { createServer } from 'node:http';
import { spawn } from 'node:child_process';
import { readFileSync } from 'node:fs';
import { verdict, completionUrl, completionText, requestBody } from '../assets/model-quality.mjs';
import { analyzeGlobalOutputs } from '../vendor/modeltrace/fingerprint-core.mjs';
import { generateChallenges } from '../vendor/modeltrace/challenge-browser.mjs';

const bank = JSON.parse(readFileSync(new URL('../vendor/modeltrace/unified_bank.json', import.meta.url)));
const fixture = JSON.parse(readFileSync(new URL('./modeltrace-astra-fixture.json', import.meta.url)));

test('三份有效回答且请求、回报、归因一致才通过', () => {
  const result = { used_outputs: 3, prediction: 'gpt-6-astra' };
  assert.equal(verdict(result, 'openai/gpt-6-astra', Array(3).fill('gpt-6-astra')), 'match');
  assert.equal(verdict({ ...result, used_outputs: 2 }, 'gpt-6-astra', Array(3).fill('gpt-6-astra')), 'insufficient');
  assert.equal(verdict(result, 'gpt-6-astra', ['gpt-6-astra', '', 'gpt-6-astra']), 'missing_model');
  assert.equal(verdict(result, 'gpt-6-astra', ['gpt-6-astra', 'gpt-6-sol', 'gpt-6-astra']), 'reported_mismatch');
  assert.equal(verdict(result, 'gpt-6-sol', Array(3).fill('gpt-6-sol')), 'mismatch');
});

test('原版算法对固定上游样本保持归因与概率，三题数量独立', () => {
  const result = analyzeGlobalOutputs(fixture.outputs, bank);
  assert.equal(result.prediction, 'gpt-6-astra');
  assert.ok(Math.abs(result.probability - 0.9999996236029431) < 1e-10);
  assert.equal(result.used_outputs, 3);
  assert.ok(bank.models.some(model => model.id === 'gpt-6-astra'));
  const challenges = generateChallenges(3);
  assert.equal(new Set(challenges.map(challenge => challenge.expected_count)).size, 3);
  assert.ok(challenges.every(challenge => challenge.expected_count >= 292 && challenge.expected_count <= 332));
});

test('接口路由保留供应商前缀，拒绝携带凭据的地址', () => {
  assert.equal(completionUrl('https://example.invalid', 'responses'), 'https://example.invalid/v1/responses');
  assert.equal(completionUrl('https://example.invalid/api/v1/', 'chat'), 'https://example.invalid/api/v1/chat/completions');
  assert.equal(completionUrl('https://example.invalid/v1/messages', 'anthropic'), 'https://example.invalid/v1/messages');
  assert.equal(completionUrl('https://example.invalid/proxy/anthropic', 'anthropic'), 'https://example.invalid/proxy/anthropic/v1/messages');
  assert.throws(() => completionUrl('https://user:pass@example.invalid', 'chat'));
  const body = requestBody({ model: 'gpt-6-astra', protocol: 'responses' }, 'challenge');
  assert.equal(body.store, false);
  assert.equal(body.input.length, 1);
  assert.equal(body.previous_response_id, undefined);
  assert.equal(body.tools, undefined);
});

test('截断及未完成响应不参与指纹，不采集推理内容', () => {
  assert.equal(completionText({ status: 'incomplete', output: [] }, 'responses'), '');
  assert.equal(completionText({ stop_reason: 'max_tokens', content: [{ type: 'text', text: '1 2 3' }] }, 'anthropic'), '');
  assert.equal(completionText({ choices: [{ finish_reason: 'length', message: { content: '1 2 3' } }] }, 'chat'), '');
  assert.equal(completionText({ status: 'completed', output: [
    { type: 'reasoning', content: [{ type: 'output_text', text: 'ignored' }] },
    { type: 'message', role: 'assistant', content: [{ type: 'output_text', text: '1 2 3' }] },
  ] }, 'responses'), '1 2 3');
});

/** 与 Rust 完全相同地组合模块，bank 和假密钥都只经 stdin 传入。 */
function runBridge(input) {
  return new Promise((resolve, reject) => {
    const script = ['../vendor/modeltrace/fingerprint-core.mjs', '../vendor/modeltrace/challenge-browser.mjs', '../assets/model-quality.mjs']
      .map(file => readFileSync(new URL(file, import.meta.url), 'utf8')).join('\n');
    const child = spawn(process.execPath, ['--input-type=module', '-e', script, '--', '--ar-model-quality'],
      { stdio: ['pipe', 'pipe', 'pipe'], windowsHide: true });
    let stdout = '', stderr = '';
    const timeout = setTimeout(() => { child.kill(); reject(new Error('bridge timed out')); }, 20000);
    child.stdout.on('data', data => { stdout += data; });
    child.stderr.on('data', data => { stderr += data; });
    child.on('error', error => { clearTimeout(timeout); reject(error); });
    child.on('close', code => {
      clearTimeout(timeout);
      try {
        assert.equal(`${stdout}${stderr}`.includes('private-test-key'), false);
        assert.equal(stderr, '');
        resolve({ code, events: stdout.trim().split('\n').filter(Boolean).map(line => JSON.parse(line)) });
      } catch (error) { reject(error); }
    });
    child.stdin.on('error', reject);
    child.stdin.end(JSON.stringify({ ...input, bank }));
  });
}

test('三个 API 协议共用 ModelTrace 完整检测流程', async t => {
  let reported = 'gpt-6-astra', status = 200, calls = 0, refuse = false;
  const server = createServer(async (req, res) => {
    const index = calls++ % 3;
    let body = '';
    for await (const chunk of req) body += chunk;
    const input = JSON.parse(body);
    const text = refuse ? 'I cannot do that.' : fixture.outputs[index].text;
    let payload;
    if (req.url === '/v1/messages') {
      assert.equal(req.headers['x-api-key'], 'private-test-key');
      assert.equal(req.headers['anthropic-version'], '2023-06-01');
      assert.equal(input.messages.length, 1);
      payload = { model: reported, stop_reason: 'end_turn', content: [{ type: 'text', text }] };
    } else if (req.url === '/v1/responses') {
      assert.equal(req.headers.authorization, 'Bearer private-test-key');
      assert.equal(input.input.length, 1);
      assert.equal(input.store, false);
      payload = { model: reported, status: 'completed', output: [{ type: 'message', role: 'assistant', content: [{ type: 'output_text', text }] }] };
    } else {
      assert.equal(req.url, '/v1/chat/completions');
      assert.equal(req.headers.authorization, 'Bearer private-test-key');
      assert.equal(input.messages.length, 1);
      payload = { model: reported, choices: [{ finish_reason: 'stop', message: { content: text } }] };
    }
    assert.equal(input.model, 'gpt-6-astra');
    assert.equal(input.tools, undefined);
    assert.equal(input.stream, false);
    res.writeHead(status, { 'Content-Type': 'application/json' });
    res.end(status === 200 ? JSON.stringify(payload) : JSON.stringify({ error: 'private-test-key' }));
  });
  await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
  const input = { model: 'custom/astra-alias', endpoint: {
    baseUrl: `http://127.0.0.1:${server.address().port}/v1`, model: 'gpt-6-astra', apiKey: 'private-test-key', protocol: 'chat' } };
  try {
    for (const protocol of ['chat', 'responses', 'anthropic']) {
      await t.test(`${protocol}：三次采样、内置基准和最终结论`, async () => {
        calls = 0;
        const run = await runBridge({ ...input, endpoint: { ...input.endpoint, protocol } });
        const output = run.events.at(-1);
        assert.equal(run.code, 0);
        assert.equal(output.type, 'result');
        assert.equal(output.verdict, 'match');
        assert.equal(output.prediction, 'gpt-6-astra');
        assert.equal(output.usedOutputs, 3);
        assert.equal(calls, 3);
        assert.equal(run.events.filter(event => event.type === 'progress').at(-1).done, 3);
      });
    }
    await t.test('回报另一模型不通过，缺失回报不能当作通过', async () => {
      for (const [model, expected] of [['gpt-6-sol', 'reported_mismatch'], [undefined, 'missing_model']]) {
        reported = model;
        const output = (await runBridge(input)).events.at(-1);
        assert.equal(output.verdict, expected);
      }
    });
    await t.test('拒答导致样本不足，不返回伪归因', async () => {
      refuse = true;
      const output = (await runBridge(input)).events.at(-1);
      assert.equal(output.verdict, 'insufficient');
      assert.equal(output.prediction, '');
      assert.equal(output.probability, null);
      refuse = false;
    });
    await t.test('鉴权错误不回显密钥且不继续采样', async () => {
      status = 401;
      const before = calls;
      const run = await runBridge(input);
      assert.equal(run.code, 1);
      assert.equal(run.events.at(-1).type, 'error');
      assert.equal(calls - before, 1);
    });
    await t.test('未收录模型不请求 API，不回退到另一套库', async () => {
      const before = calls;
      const run = await runBridge({ ...input, endpoint: { ...input.endpoint, model: 'unlisted-model' } });
      assert.equal(run.code, 1);
      assert.match(run.events.at(-1).message, /未收录/);
      assert.equal(calls, before);
    });
  } finally {
    server.closeAllConnections();
    await new Promise(resolve => server.close(resolve));
  }
});
