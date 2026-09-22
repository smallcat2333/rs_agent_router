'use strict';

// Desktop Commander 0.2.50 stops readline iteration without closing its input.
// Only its text reader is affected; stdin and other consumers keep native behavior.
const fs = require('node:fs');
const readline = require('node:readline');
const childProcess = require('node:child_process');
const { syncBuiltinESMExports } = require('node:module');
const createInterface = readline.createInterface;
const spawn = childProcess.spawn;

/** Match the pinned package's file reader across Windows and POSIX stack paths. */
function isTextReader(stack) {
    return stack.replace(/\\/g, '/').includes('/@wonderwhy-er/desktop-commander/dist/utils/files/text.js:');
}

/** Close the owned file input on readline close and await fd release on iteration exit. */
readline.createInterface = function (...args) {
    const rl = createInterface.apply(this, args);
    const input = args[0]?.input;
    if (!(input instanceof fs.ReadStream) || !isTextReader(new Error().stack)) return rl;
    const closed = input.closed ? Promise.resolve() : new Promise(resolve => input.once('close', resolve));
    rl.once('close', () => input.destroy());
    const iterate = rl[Symbol.asyncIterator];
    rl[Symbol.asyncIterator] = async function* () {
        try {
            yield* iterate.call(this);
        } finally {
            rl.close();
            input.destroy();
            await closed;
        }
    };
    return rl;
};

/** The MCP SDK filters the child environment; propagate this preload only to DCR's local server. */
childProcess.spawn = function (command, args, options) {
    if (Array.isArray(args) && args.some(arg => typeof arg === 'string' &&
        arg.replace(/\\/g, '/').endsWith('/@wonderwhy-er/desktop-commander/dist/index.js'))) {
        const env = { ...(options?.env || process.env) };
        const preload = `--require ${JSON.stringify(__filename.replace(/\\/g, '/'))}`;
        if (!(env.NODE_OPTIONS || '').includes(preload)) {
            env.NODE_OPTIONS = [env.NODE_OPTIONS, preload].filter(Boolean).join(' ');
        }
        options = { ...options, env };
    }
    return spawn.call(this, command, args, options);
};
syncBuiltinESMExports();
