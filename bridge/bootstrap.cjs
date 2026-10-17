"use strict";
const fs = require("node:fs");
const crypto = require("node:crypto");
const { NativeBridge } = require("./native.cjs");
const electron = require("electron");
const app = electron.app;
const desktop = new (require("./desktop.cjs").Desktop)(electron);
const handleBrokenPipe = () => {};
const { AccountSession } = require("./accounts.cjs");
const accounts = new AccountSession(app, {
  account: process.env.QQTUI_ACCOUNT,
  profile: process.env.QQTUI_PROFILE_DIR,
  leaseDir: process.env.QQTUI_LEASE_DIR,
});
process.on("exit", () => accounts.close());
const compatibility = require("./compatibility.json")[process.platform]?.[
  process.arch
]?.[app.getVersion()];
const bridge = new NativeBridge(process.env.QQTUI_SOCKET, {
  stateDir: process.env.QQTUI_STATE_DIR,
  cacheDir: process.env.QQTUI_CACHE_DIR,
  profileDir: app.getPath("userData"),
  userData: accounts.root,
  qqVersion: app.getVersion(),
  desktop,
  framework: compatibility?.framework,
});

bridge.listen();

const originalDlopen = process.dlopen;
let captured = false;
process.dlopen = function (module, filename, ...args) {
  const result = originalDlopen.call(this, module, filename, ...args);
  if (captured || require("node:path").basename(filename) !== "wrapper.node")
    return result;
  captured = true;
  process.dlopen = originalDlopen;
  try {
    const hash = crypto.createHash("sha256");
    const fd = fs.openSync(filename, "r");
    try {
      const buffer = Buffer.allocUnsafe(1024 * 1024);
      for (;;) {
        const count = fs.readSync(fd, buffer, 0, buffer.length, null);
        if (!count) break;
        hash.update(buffer.subarray(0, count));
      }
    } finally {
      fs.closeSync(fd);
    }
    if (!compatibility || hash.digest("hex") !== compatibility.wrapper_sha256) {
      bridge.phase = "incompatible";
      bridge.errors.push("wrapper.node 指纹不匹配，需要重新验证 QQ 版本");
      return result;
    }
  } catch (e) {
    bridge.phase = "incompatible";
    bridge.errors.push("无法验证 wrapper.node: " + e.message);
    return result;
  }
  const wrapper = module.exports;
  bridge.phase = "login";
  console.log("[qqtui] captured QQ wrapper");
  const wrapSession = (session) =>
    new Proxy(
      {},
      {
        get(_target, key) {
          const value = session[key];
          if (key !== "init")
            return typeof value === "function" ? value.bind(session) : value;
          return (...params) => {
            try {
              params[0] = accounts.prepare(params[0]);
            } catch (error) {
              bridge.phase = "incompatible";
              bridge.errors.push(error.message);
              console.error("[qqtui] account:", error.message);
              throw error;
            }
            bridge.attach(session, wrapper, null);
            bridge.sessionConfig = params[0];
            const callbacks = params[3];
            const original = callbacks.onOpentelemetryInit;
            callbacks.onOpentelemetryInit = function (...values) {
              const result = original?.apply(this, values);
              console.log("[qqtui] session initialized", values[0]?.is_init);
              if (values[0]?.is_init)
                setImmediate(() => {
                  try {
                    // QQ's native crash handler replaces Node's default SIGPIPE
                    // disposition. A disconnected RPC client is a stream error,
                    // not a fatal process signal; install after native init.
                    if (process.platform !== "win32") {
                      process.removeListener("SIGPIPE", handleBrokenPipe);
                      process.on("SIGPIPE", handleBrokenPipe);
                    }
                    bridge.ready(params[0]);
                  } catch (e) {
                    bridge.phase = "incompatible";
                    bridge.errors.push(e.message);
                    console.error("[qqtui] initialization:", e.message);
                  }
                });
              return result;
            };
            return session.init(...params);
          };
        },
      },
    );
  module.exports = new Proxy(
    {},
    {
      get(_target, key) {
        if (
          accounts.expected &&
          ["NodeIQQNTWrapperEngine", "NodeIKernelLoginService"].includes(key)
        ) {
          const service = wrapper[key];
          const method =
            key === "NodeIQQNTWrapperEngine"
              ? "initWithDeskTopConfig"
              : "initConfig";
          return new Proxy(
            {},
            {
              get(_service, name) {
                const value = service[name];
                if (name !== "get" || typeof value !== "function")
                  return typeof value === "function"
                    ? value.bind(service)
                    : value;
                return (...args) => {
                  const instance = value.apply(service, args);
                  return new Proxy(
                    {},
                    {
                      get(_instance, name) {
                        const fn = instance[name];
                        if (name !== method || typeof fn !== "function")
                          return typeof fn === "function"
                            ? fn.bind(instance)
                            : fn;
                        return (config, ...rest) =>
                          fn.call(
                            instance,
                            key === "NodeIQQNTWrapperEngine"
                              ? accounts.engine(config)
                              : accounts.login(config),
                            ...rest,
                          );
                      },
                    },
                  );
                };
              },
            },
          );
        }
        if (key !== "NodeIQQNTWrapperSession") return wrapper[key];
        return new Proxy(() => {}, {
          get(_service, method) {
            const value = wrapper[key][method];
            if (
              ["create", "getNTWrapperSession"].includes(method) &&
              typeof value === "function"
            )
              return (...args) => {
                return wrapSession(value.apply(wrapper[key], args));
              };
            return value;
          },
        });
      },
    },
  );

  return result;
};

const originalMain = process.env.QQTUI_ORIGINAL_MAIN;
if (!originalMain || !fs.existsSync(originalMain.split(".asar/")[0])) {
  throw new Error("QQ original application entry is missing");
}
require(originalMain);
