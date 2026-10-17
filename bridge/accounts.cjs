"use strict";
const fs = require("node:fs");
const path = require("node:path");
const crypto = require("node:crypto");

// Browser/login state belongs to an instance; native account_path remains QQ's original root.
class AccountSession {
  constructor(app, options = {}) {
    this.root = app.getPath("userData");
    this.expected = options.account;
    this.profile = options.profile;
    this.leaseDir = options.leaseDir;
    this.leases = new Map();
    if (this.expected && !/^[1-9][0-9]{0,19}$/.test(this.expected))
      throw new Error("Invalid QQ account");
    if (options.profile) {
      if (!path.isAbsolute(options.profile))
        throw new Error("Absolute QQ profile required");
      fs.mkdirSync(options.profile, { recursive: true, mode: 0o700 });
      app.setPath("userData", options.profile);
      app.setPath("sessionData", options.profile);
    }
  }
  globalPath() {
    const dir = path.join(this.profile, "global");
    fs.mkdirSync(dir, { recursive: true, mode: 0o700 });
    return dir;
  }
  engine(config) {
    return this.profile
      ? {
          ...config,
          global_path_config: {
            ...config.global_path_config,
            desktopGlobalPath: this.globalPath(),
          },
        }
      : config;
  }
  login(config) {
    return this.profile ? { ...config, commonPath: this.globalPath() } : config;
  }
  prepare(config) {
    const account = String(config.selfUin || "");
    if (!/^[1-9][0-9]{0,19}$/.test(account))
      throw new Error("QQ session has no account");
    if (this.expected && account !== this.expected)
      throw new Error(
        `此窗口仅登录 ${this.expected}，请在 QQ 登录页选择对应账号`,
      );
    if (
      this.leaseDir &&
      !this.leases.has(path.join(this.leaseDir, account + ".json"))
    ) {
      fs.mkdirSync(this.leaseDir, { recursive: true, mode: 0o700 });
      const file = path.join(this.leaseDir, account + ".json");
      const start =
        process.platform === "linux"
          ? fs
              .readFileSync("/proc/self/stat", "utf8")
              .split(/\) /)
              .pop()
              .split(/\s+/)[19]
          : undefined;
      const owner = JSON.stringify({
        pid: process.pid,
        start,
        token: crypto.randomUUID(),
      });
      try {
        fs.writeFileSync(file, owner, { flag: "wx", mode: 0o600 });
      } catch (error) {
        if (error.code === "EEXIST")
          throw new Error(
            `账号 ${account} 已由另一个宿主管理；请打开它的 TUI 窗口`,
          );
        throw error;
      }
      this.leases.set(file, owner);
    }
    if (this.expected) {
      if (!config.desktopPathConfig || !path.isAbsolute(this.root))
        throw new Error("QQ native account path unavailable");
      config = {
        ...config,
        desktopPathConfig: {
          ...config.desktopPathConfig,
          account_path: this.root,
        },
      };
    }
    return config;
  }
  close() {
    for (const [file, owner] of this.leases) {
      try {
        if (fs.readFileSync(file, "utf8") === owner) fs.unlinkSync(file);
      } catch (_) {}
    }
    this.leases.clear();
  }
}
module.exports = { AccountSession };
