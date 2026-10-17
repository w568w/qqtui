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
