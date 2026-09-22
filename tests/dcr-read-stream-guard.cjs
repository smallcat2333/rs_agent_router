'use strict';
const { test } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const vm = require('node:vm');
const readline = require('node:readline');
const { once } = require('node:events');
require('../assets/dcr-read-stream-guard.cjs');

// Reproduce the upstream call site without downloading npm dependencies.
const read = vm.compileFunction(`
    return (async () => {
        const rl = readline.createInterface({input, crlfDelay: Infinity});
        const lines = [];
        for await (const line of rl) {
            lines.push(line);
            if (fail) throw new Error('consumer failure');
            if (lines.length >= limit) break;
        }
        rl.close();
        return lines;
    })();
`, ['readline', 'input', 'limit', 'fail'], {
    filename: '/node_modules/@wonderwhy-er/desktop-commander/dist/utils/files/text.js',
});

/** Use a private fixture and remove it even if the regression fails. */
test('partial, full and failed reads release the fd before returning', async () => {
    const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'dcr-stream-test-'));
    const file = path.join(dir, 'fixture.ui');
    fs.writeFileSync(file, 'line\n'.repeat(20000));
    try {
        for (const [limit, fail] of [[5, false], [Infinity, false], [5, true], [5, false]]) {
            const input = fs.createReadStream(file, {encoding: 'utf8'});
            try {
                if (fail) await assert.rejects(read(readline, input, limit, fail), /consumer failure/);
                else assert.equal((await read(readline, input, limit, fail)).length, Math.min(limit, 20000));
                assert.equal(input.closed, true);
                assert.equal(input.fd, null);
                fs.renameSync(file, file + '.saved');
                fs.renameSync(file + '.saved', file);
            } finally {
                if (!input.closed) {
                    const closed = once(input, 'close');
                    input.destroy();
                    await closed;
                }
            }
        }
    } finally {
        fs.rmSync(dir, {recursive: true, force: true});
    }
});

/** Non-file streams, including the MCP stdin transport, must not be destroyed. */
test('ordinary readline input retains native ownership', () => {
    const input = new (require('node:stream').PassThrough)();
    const rl = readline.createInterface({input});
    rl.close();
    assert.equal(input.destroyed, false);
    input.destroy();
});

/** Aborting a pending read must reject and close the underlying file as well. */
test('aborted file read releases its descriptor', async () => {
    const controller = new AbortController();
    const input = fs.createReadStream(__filename, {signal: controller.signal});
    const pending = read(readline, input, Infinity, false);
    controller.abort();
    await assert.rejects(pending, {name: 'AbortError'});
    assert.equal(input.closed, true);
    assert.equal(input.fd, null);
});

/** Simulate the MCP SDK dropping NODE_OPTIONS before spawning the DCR server. */
test('filtered MCP child environment still loads the guard', async () => {
    const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'dcr child test '));
    const pkg = path.join(dir, 'node_modules/@wonderwhy-er/desktop-commander/dist');
    fs.mkdirSync(pkg, {recursive: true});
    const entry = path.join(pkg, 'index.js');
    const guard = path.resolve(__dirname, '../assets/dcr-read-stream-guard.cjs');
    fs.writeFileSync(entry, `console.log(JSON.stringify({loaded:!!require.cache[${JSON.stringify(guard)}], options:process.env.NODE_OPTIONS}));`);
    try {
        const child = require('node:child_process').spawn(process.execPath, [entry], {
            env: {SystemRoot: process.env.SystemRoot || '', NODE_OPTIONS: '--no-warnings'},
            windowsHide: true,
        });
        let output = '', errors = '';
        child.stdout.on('data', data => output += data);
        child.stderr.on('data', data => errors += data);
        const [code] = await once(child, 'close');
        assert.equal(code, 0, errors);
        const result = JSON.parse(output);
        assert.equal(result.loaded, true);
        assert.ok(result.options.includes('--no-warnings'));
    } finally {
        fs.rmSync(dir, {recursive: true, force: true});
    }
});
