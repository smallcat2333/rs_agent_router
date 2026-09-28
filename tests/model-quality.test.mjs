// 离线验收：纯判定边界及真实 Node 子进程到本地模拟 API 的完整采样链路。
import test from 'node:test';
import assert from 'node:assert/strict';
import { createServer } from 'node:http';
import { spawn } from 'node:child_process';
import { mkdtempSync, writeFileSync, readFileSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { passed } from '../assets/model-quality.mjs';

/** 建立判定输入，专注相邻阈值与模型回报，不依赖真实 API。 */
function result(verdict = 'match', meanJsd = 0.25, cells = 4) {
  return { verdict, meanJsd, comparison: { comparableCellCount: cells }, reference: { model: 'openai/gpt-4o' } };
}

test('只有指纹 MATCH 且实际回报模型一致才通过', () => {
  assert.equal(passed(result(), 'gpt-4o', ['gpt-4o']), true);
  for (const output of [[], ['different'], ['gpt-4o', 'different']]) {
    assert.equal(passed(result(), 'gpt-4o', output), false);
  }
  for (const sample of [result('uncertain', .251), result('mismatch', .351), result('insufficient', null), result('match', .1, 3)]) {
    assert.equal(passed(sample, 'gpt-4o', ['gpt-4o']), false);
  }
  assert.equal(passed(result(), 'gpt-4o-2024-08-06', ['gpt-4o-2024-08-06']), false);
});

/** 用与 Rust 一致的 node -e 命令运行桥接，stdin 传假密钥，收集 JSONL 事件。 */
function runBridge(input) {
  return new Promise((resolve, reject) => {
    const script = readFileSync(new URL('../assets/model-quality.mjs', import.meta.url), 'utf8');
    const child = spawn(process.execPath, ['--input-type=module', '-e', script, '--', '--ar-model-quality'],
      { stdio: ['pipe', 'pipe', 'pipe'], windowsHide: true });
    let stdout = '', stderr = '';
    const timeout = setTimeout(() => { child.kill(); reject(new Error('bridge timed out')); }, 15000);
    child.stdout.on('data', data => { stdout += data; });
    child.stderr.on('data', data => { stderr += data; });
    child.on('error', reject);
    child.on('close', code => {
      clearTimeout(timeout);
      assert.equal(`${stdout}${stderr}`.includes('private-test-key'), false);
      try { resolve({ code, events: stdout.trim().split('\n').filter(Boolean).map(line => JSON.parse(line)) }); }
      catch (error) { reject(error); }
    });
    child.stdin.end(JSON.stringify(input));
  });
}

/** 生成四个确定分布；模拟端点返回相同 token 时应获得 JSD=0。 */
function referenceFixture() {
  const cells = {};
  for (const [id, answer] of [['random-number-1-100:en', '42'], ['random-number-1-100:zh', '42'],
    ['random-color:en', 'blue'], ['random-animal:en', 'cat']]) {
    cells[id] = { cellId: id, counts: { [answer]: 30 }, validCount: 30, totalCount: 30 };
  }
  return { formatVersion: 1, protocol: 'one-token/v1', model: 'openai/gpt-4o',
    collectedAt: '2026-09-28', samplesPerCell: 30, postReasoning: false, cells };
}

test('本地 API 完整采样、错误回报、缺失回报、鉴权失败和无基准', async t => {
  const directory = mkdtempSync(join(tmpdir(), 'ar-model-quality-'));
  const reference = join(directory, 'reference.json');
  writeFileSync(reference, JSON.stringify(referenceFixture()));
  let reported = 'gpt-4o', status = 200, calls = 0;
  const server = createServer(async (req, res) => {
    calls++;
    assert.equal(req.url, '/v1/chat/completions');
    assert.equal(req.headers.authorization, 'Bearer private-test-key');
    let body = '';
    for await (const chunk of req) body += chunk;
    const input = JSON.parse(body);
    assert.equal(input.model, 'gpt-4o');
    const prompt = input.messages.at(-1).content.toLowerCase();
    const content = prompt.includes('color') || prompt.includes('colour') ? 'blue' : prompt.includes('animal') ? 'cat' : '42';
    res.writeHead(status, { 'Content-Type': 'application/json' });
    res.end(status === 200 ? JSON.stringify({ model: reported, choices: [{ message: { content } }] })
      : JSON.stringify({ error: 'private-test-key' }));
  });
  await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
  const input = { model: 'gpt-4o', reference, endpoint: {
    baseUrl: `http://127.0.0.1:${server.address().port}/v1`, model: 'gpt-4o', apiKey: 'private-test-key' } };
  try {
    await t.test('match 通过且真实发出 60 次采样和参数探测', async () => {
      const run = await runBridge(input);
      const output = run.events.at(-1);
      assert.equal(run.code, 0);
      assert.equal(output.type, 'result');
      assert.equal(output.passed, true);
      assert.equal(output.meanJsd, 0);
      assert.ok(calls >= 61);
    });
    await t.test('相同指纹但回报另一模型不通过', async () => {
      reported = 'substitute-model';
      const output = (await runBridge(input)).events.at(-1);
      assert.equal(output.verdict, 'match');
      assert.equal(output.passed, false);
    });
    await t.test('缺失 API model 不通过', async () => {
      reported = undefined;
      const output = (await runBridge(input)).events.at(-1);
      assert.equal(output.missingModel, true);
      assert.equal(output.passed, false);
    });
    await t.test('HTTP 错误正文不泄露密钥', async () => {
      status = 401;
      const run = await runBridge(input);
      assert.equal(run.code, 1);
      assert.equal(run.events.at(-1).type, 'error');
    });
    await t.test('没有基准或基准模型不一致时不发请求', async () => {
      const before = calls;
      for (const referencePath of ['', reference]) {
        const run = await runBridge({ ...input, reference: referencePath, model: 'unlisted-model' });
        assert.equal(run.code, 1);
        assert.equal(run.events.at(-1).type, 'error');
      }
      assert.equal(calls, before);
    });
  } finally {
    server.closeAllConnections();
    await new Promise(resolve => server.close(resolve));
    rmSync(directory, { recursive: true, force: true });
  }
});
