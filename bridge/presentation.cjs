"use strict";
const fs = require("node:fs");

function permission(group, detail, now = Math.floor(Date.now() / 1000)) {
  const personal = Number(
    detail?.shutUpMeTimestamp || group?.personShutupExpireTime || 0,
  );
  const all =
    Number(detail?.shutUpAllTimestamp || group?.groupShutupExpireTime || 0) > 0;
  const role = Number(group?.memberRole || 0);
  if (personal > now)
    return {
      state: "blocked",
      scope: "self",
      reason: "你已被禁言",
      until: String(personal),
    };
  if (all && role !== 3 && role !== 4)
    return {
      state: "blocked",
      scope: "all",
      reason: "群已开启全员禁言",
      until: null,
    };
  if (![2, 3, 4].includes(role))
    return {
      state: "unknown",
      scope: "unknown",
      reason: "无法确认本人群身份",
      until: null,
    };
  return {
    state: "allowed",
    scope: all ? "all" : "none",
    reason: all ? "全员禁言中，你具有发言权限" : "",
    until: null,
  };
}
class Presentation {
  constructor(owner) {
    this.owner = owner;
    this.groups = new Map();
    this.details = new Map();
    this.waiters = new Map();
    this.pending = new Map();
    this.avatarRequests = new Map();
    this.selfMembers = new Map();
  }
  update(groups) {
    for (const group of groups || []) {
      const id = String(group.groupCode),
        old = this.groups.get(id);
      if (
        old &&
        ["memberRole", "personShutupExpireTime", "groupShutupExpireTime"].some(
          (key) => old[key] !== group[key],
        )
      )
        this.owner.event("permissions.invalidated");
      this.groups.set(id, group);
    }
  }
  detail(info) {
    const id = String(info.groupCode);
    const old = this.details.get(id);
    if (
      old &&
      ["shutUpAllTimestamp", "shutUpMeTimestamp"].some(
        (key) => old[key] !== info[key],
      )
    )
      this.owner.event("permissions.invalidated");
    this.details.set(id, info);
    for (const done of this.waiters.get(id) || []) done(null, info);
    this.waiters.delete(id);
  }
  member(group, members) {
    const me =
      members instanceof Map
        ? members.get(this.owner.selfUid)
        : members?.[this.owner.selfUid];
    if (!me) return;
    const state = String(me.role) + ":" + String(me.shutUpTime);
    if (this.selfMembers.get(group) !== state) {
      this.selfMembers.set(group, state);
      this.owner.event("permissions.invalidated");
    }
  }
  async speech(id) {
    if (this.pending.has(id)) return this.pending.get(id);
    if (this.pending.size >= 8) throw new Error("群权限查询队列已满");
    const context = this.owner.sessionContext();
    const work = (async () => {
      const service = context.session.getGroupService();
      const group = await new Promise((resolve, reject) => {
        const done = (groups) => {
          clearTimeout(timer);
          resolve(groups.find((g) => String(g.groupCode) === id));
        };
        const timer = setTimeout(() => {
          this.owner.groupWaiters.delete(done);
          reject(new Error("读取本人群身份超时"));
        }, 5000);
        this.owner.groupWaiters.add(done);
        Promise.resolve(service.getGroupList(false)).catch((e) => {
          clearTimeout(timer);
          this.owner.groupWaiters.delete(done);
          reject(e);
        });
      });
      context.check();
      const detail = await new Promise((resolve, reject) => {
        const finish = (error, value) => {
          clearTimeout(timer);
          this.waiters.get(id)?.delete(finish);
          if (error) reject(error);
          else resolve(value);
        };
        const timer = setTimeout(
          () => finish(new Error("读取群禁言状态超时")),
          5000,
        );
        if (!this.waiters.has(id)) this.waiters.set(id, new Set());
        this.waiters.get(id).add(finish);
        Promise.resolve(service.getGroupDetailInfo(id, 2)).then((result) => {
          if (result?.result)
            finish(new Error("QQ 群权限读取失败：" + result.result));
        }, finish);
      });
      context.check();
      return permission(group, detail);
    })();
    this.pending.set(id, work);
    try {
      return await work;
    } finally {
      if (this.pending.get(id) === work) this.pending.delete(id);
    }
  }
  async avatar(uid) {
    if (this.avatarRequests.has(uid)) return this.avatarRequests.get(uid);
    if (this.avatarRequests.size >= 8) throw new Error("头像查询队列已满");
    const request = this.resolveAvatar(uid);
    this.avatarRequests.set(uid, request);
    try {
      return await request;
    } finally {
      if (this.avatarRequests.get(uid) === request)
        this.avatarRequests.delete(uid);
    }
  }
  async resolveAvatar(uid) {
    if (typeof uid !== "string" || !uid || uid.length > 128)
      throw new Error("Avatar UID required");
    const context = this.owner.sessionContext();
    const service = context.session.getAvatarService();
    const file = () => {
      const value = service.getAvatarPath(uid, 0);
      return typeof value === "string" && value && fs.existsSync(value)
        ? value
        : "";
    };
    let path = file();
    if (!path) {
      await service.forceDownloadAvatar(uid, 0);
      context.check();
      path = file();
    }
    if (!path) throw new Error("QQ 尚未返回本地头像");
    const stat = await fs.promises.stat(path);
    context.check();
    return { path, version: `${stat.mtimeMs}:${stat.size}` };
  }
  close() {
    for (const waiting of this.waiters.values())
      for (const finish of waiting) finish(new Error("QQ 会话已变化"));
    this.waiters.clear();
    this.groups.clear();
    this.details.clear();
    this.pending.clear();
    this.avatarRequests.clear();
    this.selfMembers.clear();
  }
}
module.exports = { Presentation, permission };
