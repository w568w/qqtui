const { test } = require('node:test');
const assert = require('node:assert/strict');
const { EventEmitter } = require('node:events');
const { Desktop } = require('../bridge/desktop.cjs');

class Window extends EventEmitter {
  constructor(visible = false) { super(); this.visible = visible; this.destroyed = false; }
  isVisible() { return this.visible; }
  isDestroyed() { return this.destroyed; }
  show() { if (!this.visible) { this.visible = true; this.emit('show'); } }
  hide() { if (this.visible) { this.visible = false; this.emit('hide'); } }
  close() { this.destroyed = true; this.emit('closed'); }
}
function fixture() {
  const app = new EventEmitter(), main = new Window(true), helper = new Window();
  const desktop = new Desktop({ app, BrowserWindow: { getAllWindows: () => [main, helper] } });
  return { app, main, helper, desktop };
}
test('ready sessions hide windows, viewer leases restore native visibility', () => {
  const { main, helper, desktop } = fixture();
  assert(main.isVisible());
  desktop.setReady(true);
  assert(!main.isVisible());
  const first = {}, second = {};
  desktop.viewer(first, true);
  desktop.viewer(second, true);
  assert(main.isVisible());
  assert(!helper.isVisible());
  desktop.viewer(first, false);
  assert(main.isVisible());
  desktop.viewer(second, false);
  assert(!main.isVisible());
  desktop.setReady(false);
  assert(main.isVisible());
});
test('new windows stay hidden in background and preserve their visibility on restore', () => {
  const { app, main, desktop } = fixture();
  desktop.setReady(true);
  const window = new Window();
  app.emit('browser-window-created', {}, window);
  window.show();
  assert(!window.isVisible());
  desktop.viewer({}, true);
  assert(window.isVisible());
  main.hide();
  desktop.setReady(false);
  assert(!main.isVisible());
  window.close();
  assert(!desktop.windows.has(window));
});

test('managed relaunch preserves Electron arguments and releases resources on quit', () => {
  const fs = require('node:fs');
  const app = new EventEmitter(), writes = [];
  let nativeRelaunches = 0, leasesClosed = 0, socketsClosed = 0;
  app.relaunch = () => ++nativeRelaunches;
  app.getVersion = () => 'test';
  app.getPath = () => '/profile';
  const crashReporter = { start(options) { this.options = options; return 'started'; } };
  const process = Object.assign(new EventEmitter(), {
    env: { QQTUI_CONTROL_FD: '9', QQTUI_ORIGINAL_MAIN: '/entry.cjs', QQTUI_PACKAGE: '/QQ/package.json', QQTUI_MANIFEST: '/bridge/package.json' },
    stdout: globalThis.process.stdout, stderr: globalThis.process.stderr,
    execPath: '/QQ', argv: ['/QQ', '--original'], platform: 'linux', arch: 'x64',
  });
  require('node:vm').runInNewContext(fs.readFileSync(require.resolve('../bridge/bootstrap.cjs'), 'utf8'), {
    process,
    require: name => {
      if (name === 'node:fs') return { ...fs, existsSync: () => true, writeSync: (fd, data) => writes.push([fd, JSON.parse(data)]) };
      if (name === 'electron') return { app, crashReporter, BrowserWindow: { getAllWindows: () => [] } };
      if (name === './desktop.cjs') return { Desktop };
      if (name === './native.cjs') return { NativeBridge: class { listen() {} server = { close: () => socketsClosed++ }; } };
      if (name === './accounts.cjs') return { AccountSession: class { close() { leasesClosed++; } } };
      if (name === '/entry.cjs') {
        assert.equal(process.env.QQTUI_PACKAGE, undefined);
        assert.equal(process.env.QQTUI_MANIFEST, undefined);
        return {};
      }
      if (name === './compatibility.json') return {};
      return require(name);
    },
  });
  const options = { uploadToServer: true, submitURL: 'https://example.test/crash' };
  assert.equal(crashReporter.start(options), 'started');
  assert.equal(crashReporter.options.uploadToServer, false);
  assert.equal(crashReporter.options.submitURL, options.submitURL);
  assert.equal(options.uploadToServer, true);
  for (const options of [undefined, { args: ['--new'] }, { execPath: '/other' }]) {
    assert.equal(app.relaunch(options), true);
  }
  assert.deepEqual(writes, [
    [9, { program: '/QQ', args: ['--original'] }],
    [9, { program: '/QQ', args: ['--new'] }],
    [9, { program: '/other', args: [] }],
  ]);
  assert.equal(nativeRelaunches, 0);
  app.emit('quit');
  assert.equal(leasesClosed, 1);
  assert.equal(socketsClosed, 1);
  delete process.env.QQTUI_CONTROL_FD;
  assert.equal(app.relaunch(), 1);
});

test('initialization failures retain local details without exposing them to QQ', t => {
  const fs = require('node:fs'), path = require('node:path');
  const directory = fs.mkdtempSync(path.join(require('node:os').tmpdir(), 'qqtui-account-error-'));
  t.after(() => fs.rmSync(directory, { recursive: true, force: true }));
  const filename = path.join(directory, 'wrapper.node');
  fs.writeFileSync(filename, 'wrapper fixture');
  const hash = require('node:crypto').createHash('sha256').update(fs.readFileSync(filename)).digest('hex');
  const app = Object.assign(new EventEmitter(), { getVersion: () => 'test', getPath: () => '/profile' });
  const originals = [], localErrors = [];
  let bridge, callback, nativeCalls = 0, failure = 'config';
  const nativeInit = (...args) => {
    nativeCalls++;
    callback = args[3]?.onOpentelemetryInit;
    if (failure === 'native') fail();
    return 'initialized';
  };
  const session = { init: nativeInit };
  const wrapper = {
    NodeIQQNTWrapperSession: { create: () => session },
    NodeIQQNTWrapperEngine: { get: () => ({ initWithDeskTopConfig: nativeInit }) },
    NodeIKernelLoginService: { get: () => ({ initConfig: nativeInit }) },
  };
  function fail() {
    const error = Object.assign(new Error('private account setup detail'), {
      path: '/private/bridge/account.json', cause: new Error('private cause'),
    });
    originals.push(error);
    throw error;
  }
  class AccountSession {
    expected = '123';
    prepare(config) { if (failure === 'config') fail(); return config; }
    engine(config) { return this.prepare(config); }
    login(config) { return this.prepare(config); }
  }
  const process = Object.assign(new EventEmitter(), {
    env: { QQTUI_ORIGINAL_MAIN: '/entry.cjs' }, platform: 'linux', arch: 'x64',
    stdout: globalThis.process.stdout, stderr: globalThis.process.stderr,
    dlopen(module) { module.exports = wrapper; },
  });
  require('node:vm').runInNewContext(fs.readFileSync(require.resolve('../bridge/bootstrap.cjs'), 'utf8'), {
    process, Buffer,
    require: name => {
      if (name === 'node:fs') return { ...fs, existsSync: () => true };
      if (name === 'node:console') return { Console: class { log() {} error(_prefix, error) { localErrors.push(error); } } };
      if (name === 'electron') return { app, BrowserWindow: { getAllWindows: () => [] } };
      if (name === './desktop.cjs') return { Desktop };
      if (name === './accounts.cjs') return { AccountSession };
      if (name === './native.cjs') return { NativeBridge: class {
        errors = []; server = { close() {} };
        constructor() { bridge = this; }
        listen() {}
        attach() { if (failure === 'attach') fail(); }
      } };
      if (name === './compatibility.json') return { linux: { x64: { test: { wrapper_sha256: hash } } } };
      if (name === '/entry.cjs') return {};
      return require(name);
    },
  }, { filename: require.resolve('../bridge/bootstrap.cjs') });
  const module = { exports: {} };
  process.dlopen(module, filename);
  assert.equal(bridge.phase, 'login');
  const initializers = [
    () => module.exports.NodeIQQNTWrapperSession.create().init({}, null, null, {
      onOpentelemetryInit() { if (failure === 'callback') fail(); return 'callback'; },
    }),
    () => module.exports.NodeIQQNTWrapperEngine.get().initWithDeskTopConfig({}),
    () => module.exports.NodeIKernelLoginService.get().initConfig({}),
  ];
  const check = initialize => assert.throws(initialize, error => {
    assert.equal(error.message, 'QQ account setup failed');
    assert.equal(error.stack, undefined);
    assert.equal(error.path, undefined);
    assert.equal(error.cause, undefined);
    return true;
  });
  initializers.forEach(check);
  assert.equal(nativeCalls, 0);
  failure = 'attach';
  check(initializers[0]);
  assert.equal(nativeCalls, 0);
  failure = 'native';
  initializers.forEach(check);
  assert.equal(nativeCalls, 3);
  failure = 'callback';
  initializers[0]();
  check(() => callback({ is_init: false }));
  assert.equal(nativeCalls, 4);
  assert.equal(bridge.phase, 'incompatible');
  assert.deepEqual([...bridge.errors], originals.map(error => error.message));
  assert.deepEqual(localErrors, originals);
  assert(originals.every(error => error.stack.includes(require.resolve('../bridge/bootstrap.cjs'))));
  failure = 'none';
  initializers.forEach(initialize => assert.equal(initialize(), 'initialized'));
  initializers[0]();
  assert.equal(callback({ is_init: false }), 'callback');
});
