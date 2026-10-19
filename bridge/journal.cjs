"use strict";
const {
  Worker,
  isMainThread,
  parentPort,
  workerData,
} = require("node:worker_threads");
const fs = require("node:fs");

// One connection lives in the worker; QQ's main thread never runs SQLite or rewrites the journal.
if (!isMainThread) {
  try {
    const { DatabaseSync } = require("node:sqlite");
    const db = new DatabaseSync(workerData.database);
    fs.chmodSync(workerData.database, 0o600);
    if (db.prepare("PRAGMA user_version").get().user_version > 1)
      throw new Error("发送日志版本高于当前程序，拒绝降级");
    db.exec(
      "PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL; PRAGMA busy_timeout=5000; CREATE TABLE IF NOT EXISTS receipts(key TEXT PRIMARY KEY,account TEXT NOT NULL,native_id TEXT NOT NULL,data TEXT NOT NULL); CREATE INDEX IF NOT EXISTS receipt_native ON receipts(account,native_id);",
    );
    db.exec("PRAGMA user_version=1");
    const compact = (r) => {
      const value = {};
      for (const key of [
        "request_id",
        "account",
        "peer",
        "native_id",
        "message_id",
        "state",
        "fingerprint",
      ])
        if (r[key] !== undefined) value[key] = r[key];
      if (r.state !== "confirmed" && r.error)
        value.error = String(r.error).slice(0, 1024);
      return JSON.stringify(value);
    };
    const put = db.prepare(
      "INSERT INTO receipts VALUES(?,?,?,?) ON CONFLICT(key) DO UPDATE SET data=excluded.data,native_id=excluded.native_id WHERE json_extract(receipts.data,'$.state')!='confirmed' OR json_extract(excluded.data,'$.state')='confirmed'",
    );
    db.exec(
      "UPDATE receipts SET data=json_set(data,'$.state','unknown') WHERE json_extract(data,'$.state') IN ('submitting','submitted')",
    );
    const get = db.prepare("SELECT data FROM receipts WHERE key=?");
    const byNative = db.prepare(
      "SELECT data FROM receipts WHERE account=? AND native_id=?",
    );
    parentPort.postMessage({ ready: true });
    parentPort.on("message", ({ id, method, args }) => {
      try {
        let result;
        if (method === "get") result = get.get(args[0])?.data;
        else if (method === "native") result = byNative.get(...args)?.data;
        else if (method === "put") {
          const [key, r] = args;
          put.run(key, r.account, r.native_id || "", compact(r));
          result = true;
        } else if (method === "close") {
          db.close();
          parentPort.postMessage({ id, result: true });
          parentPort.close();
          return;
        } else throw new Error("Unknown journal operation");
        parentPort.postMessage({ id, result });
      } catch (e) {
        parentPort.postMessage({ id, error: e.message });
      }
    });
  } catch (e) {
    parentPort.postMessage({ fatal: e.message });
    parentPort.close();
  }
} else {
  class Journal {
    constructor(file) {
      this.file = file;
      this.pending = new Map();
      this.next = 0;
      this.worker = null;
      this.opening = null;
      this.closed = false;
      this.error = null;
    }
    async open() {
      if (this.opening) return this.opening;
      this.opening = new Promise((resolve, reject) => {
        const worker = (this.worker = new Worker(__filename, {
          workerData: { database: this.file },
        }));
        worker.unref();
        const failed = (error) => {
          this.error = error;
          reject(error);
          for (const waiter of this.pending.values()) waiter.reject(error);
          this.pending.clear();
        };
        worker.on("error", failed);
        worker.on("exit", (code) => {
          if (!this.closed) failed(new Error("发送日志进程退出：" + code));
        });
        worker.on("message", (reply) => {
          if (reply.ready) {
            resolve();
            return;
          }
          if (reply.fatal) {
            failed(new Error("发送日志不可用：" + reply.fatal));
            return;
          }
          const waiter = this.pending.get(reply.id);
          if (!waiter) return;
          this.pending.delete(reply.id);
          if (!this.pending.size) worker.unref();
          reply.error
            ? waiter.reject(new Error(reply.error))
            : waiter.resolve(reply.result);
        });
      });
      // Keep the worker alive until database initialization finishes.
      this.worker.ref();
      try {
        await this.opening;
      } finally {
        if (!this.pending.size) this.worker.unref();
      }
    }
    async call(method, args) {
      if (this.closed && method !== "close") throw new Error("发送日志已关闭");
      await this.open();
      if (this.error) throw this.error;
      if (this.pending.size >= 256) throw new Error("发送日志队列已满");
      return new Promise((resolve, reject) => {
        const id = ++this.next;
        this.pending.set(id, { resolve, reject });
        this.worker.ref();
        this.worker.postMessage({ id, method, args });
      });
    }
    async get(key) {
      const data = await this.call("get", [key]);
      return data ? JSON.parse(data) : null;
    }
    async byNative(account, id) {
      const data = await this.call("native", [account, id]);
      return data ? JSON.parse(data) : null;
    }
    async put(key, receipt) {
      return this.call("put", [key, { ...receipt }]);
    }
    async close() {
      if (!this.worker) return;
      this.closed = true;
      if (this.error) {
        await this.worker.terminate();
        return;
      }
      await this.call("close", []);
      await this.worker.terminate();
    }
  }
  module.exports = { Journal };
}
