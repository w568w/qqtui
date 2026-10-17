"use strict";

class Desktop {
  constructor({ app, BrowserWindow }) {
    this.windows = new Map();
    this.viewers = new Set();
    this.ready = false;
    this.updating = false;
    const track = (window) => {
      this.windows.set(window, window.isVisible());
      window.on("show", () => {
        if (this.updating) return;
        this.windows.set(window, true);
        this.update();
      });
      window.on("hide", () => {
        if (!this.updating) this.windows.set(window, false);
      });
      window.once("closed", () => this.windows.delete(window));
      this.update();
    };
    app.on("browser-window-created", (_event, window) => track(window));
    for (const window of BrowserWindow.getAllWindows()) track(window);
  }
  setReady(ready) {
    if (this.ready === ready) return;
    this.ready = ready;
    this.update();
  }
  viewer(client, active) {
    if (active) this.viewers.add(client);
    else this.viewers.delete(client);
    this.update();
  }
  update() {
    if (this.updating) return;
    this.updating = true;
    try {
      for (const [window, visible] of this.windows) {
        if (window.isDestroyed()) continue;
        const show = visible && (!this.ready || this.viewers.size > 0);
        if (show && !window.isVisible()) window.show();
        else if (!show && window.isVisible()) window.hide();
      }
    } finally {
      this.updating = false;
    }
  }
}

module.exports = { Desktop };
