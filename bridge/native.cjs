"use strict";
const net = require("node:net");
const fs = require("node:fs");
const logger = new (require("node:console").Console)(process.stdout, process.stderr);
const fsp = fs.promises;
const path = require("node:path");
const crypto = require("node:crypto");
const { QqResources, faceName } = require("./qq-resources.cjs");
const { Journal } = require("./journal.cjs");
const { Presentation } = require("./presentation.cjs");

const MAX_FRAME = 8 * 1024 * 1024;
// QQ's native notification decision: 1 updates counters, 2 requests an alert.
const NOTIFICATION_ALERT = 2;
const json = (value) =>
  JSON.stringify(value, (_, v) =>
    typeof v === "bigint"
      ? v.toString()
      : v instanceof Map
        ? Object.fromEntries(v)
        : v,
  );
const listener = (handlers) =>
  new Proxy(handlers, { get: (target, key) => target[key] ?? (() => {}) });
const peerKey = (p) => `${p.chat_type}:${p.uid}`;
function nativePeer(p) {
  if (
    !p ||
    ![1, 2, 8, 100, 103].includes(p.chat_type) ||
    typeof p.uid !== "string" ||
    !p.uid ||
    p.uid.length > 128
  )
    throw new Error("Invalid peer");
  return { chatType: p.chat_type, peerUid: p.uid, guildId: "" };
}
function isNativeId(value, allowZero = false) {
  return (
    typeof value === "string" &&
    (allowZero ? /^(0|[1-9][0-9]{0,19})$/ : /^[1-9][0-9]{0,19}$/).test(value) &&
    BigInt(value) <= 18446744073709551615n
  );
}
function samePeer(raw, peer) {
  return (
    (raw.chatType === undefined || raw.chatType === peer.chatType) &&
    (raw.peerUid === undefined || String(raw.peerUid) === peer.peerUid)
  );
}
function checked(result) {
  if (
    ["number", "string"].includes(typeof result?.result) &&
    result.result !== 0 &&
    result.result !== "0"
  )
    throw new Error(`QQ: ${result.errMsg ?? result.result}`);
  if (result?.errCode)
    throw new Error(`QQ: ${result.errMsg ?? result.errCode}`);
  return result;
}
function cleanText(value) {
  return String(value ?? "").replace(/[\x00-\x08\x0b-\x1f\x7f-\x9f]/g, "");
}
function grayTipText(tip, names = {}, raw = {}) {
  const revoke = tip.revokeElement;
  if (revoke || tip.subElementType === 1) {
    const actor = revoke?.isSelfOperate ? "你" :
      revoke?.operatorMemRemark || revoke?.operatorRemark || names[revoke?.operatorUid] || revoke?.operatorNick ||
      names[raw.senderUid] || raw.sendMemberName || raw.sendNickName || raw.senderUin || "对方";
    return cleanText(`${actor}撤回了一条消息${revoke?.wording ? "：" + revoke.wording : ""}`);
  }
  const jsonTip = tip.jsonGrayTipElement;
  if (typeof jsonTip?.jsonStr === "string" && jsonTip.jsonStr.length <= 65536) {
    try {
      const data = JSON.parse(jsonTip.jsonStr);
      if (Array.isArray(data.items))
        return (
          data.items.map((item) => cleanText(names[item.uid] || names[item.uin] || item.txt || "")).join("") ||
          cleanText(jsonTip.recentAbstract) || "[系统消息]"
        );
    } catch (_) {}
  }
  return cleanText(jsonTip?.recentAbstract || "[系统消息]");
}
function mailText(element) {
  const data = element.bytesData;
  if (typeof data !== "string" || data.length > 65536) return "[QQ 邮箱提醒]";
  try {
    const card = JSON.parse(data),
      mail = card.view === "mail" && card.meta?.mail;
    if (!mail) return "[QQ 邮箱提醒]";
    let text = [mail.title, mail.subTitle, mail.content]
      .filter((v) => typeof v === "string" && v)
      .map(cleanText)
      .join("\n");
    try {
      const link = new URL(mail.mailUrl);
      if (
        link.protocol === "https:" &&
        (link.hostname === "mail.qq.com" ||
          link.hostname.endsWith(".mail.qq.com"))
      )
        text += "\n" + link.href;
    } catch (_) {}
    return text || "[QQ 邮箱提醒]";
  } catch (_) {
    return "[QQ 邮箱提醒]";
  }
}
function arkData(element) {
  if (typeof element?.bytesData !== "string" || element.bytesData.length > 65536) return null;
  try {
    const data = JSON.parse(element.bytesData);
    return data && typeof data === "object" && !Array.isArray(data) ? data : null;
  } catch (_) { return null; }
}
function cardText(data) {
  if (!data) return "[卡片消息]";
  const lines = [];
  const add = value => {
    if (typeof value !== "string") return;
    const text = cleanText(value).trim();
    if (text && !lines.includes(text)) lines.push(text);
  };
  for (const detail of Object.values(data.meta || {}).slice(0, 8)) {
    if (!detail || typeof detail !== "object") continue;
    for (const key of ["title", "desc", "summary", "content", "tag", "name", "address"]) add(detail[key]);
    for (const key of ["jumpUrl", "qqdocurl", "url"]) {
      try {
        const url = new URL(detail[key]);
        if (["https:", "http:"].includes(url.protocol) && !url.username && !url.password) add(url.href);
      } catch (_) {}
    }
  }
  if (!lines.length) add(data.prompt || data.desc);
  return lines.join("\n") || "[卡片消息]";
}
function message(raw, account, resources = new QqResources(), names = {}) {
  const recalled = (raw.elements ?? []).some(
    (e) =>
      e.grayTipElement?.subElementType === 1 || e.grayTipElement?.revokeElement,
  );
  const attachments = [];
  const parts = [];
  const card_parts = [];
  let forward = null;
  let reaction_notice = null;
  let text = "";
  let reply_to = null;
  let reply_seq = null;
  for (const e of raw.elements ?? []) {
    if (e.textElement) {
      const content = cleanText(e.textElement.content);
      text += content;
      parts.push({ type: "text", text: content });
    } else if (e.replyElement) {
      reply_to =
        [e.replyElement.replayMsgId, e.replyElement.sourceMsgIdInRecords]
          .map((v) => String(v ?? ""))
          .find((v) => /^[1-9][0-9]{0,19}$/.test(v)) || null;
      reply_seq = /^[1-9][0-9]{0,19}$/.test(String(e.replyElement.replayMsgSeq))
        ? String(e.replyElement.replayMsgSeq)
        : null;
    } else if (
      e.picElement ||
      e.fileElement ||
      e.videoElement ||
      e.pttElement
    ) {
      const v = e.picElement ?? e.fileElement ?? e.videoElement ?? e.pttElement;
      const kind =
        e.picElement?.picSubType === 1
          ? "sticker"
          : e.picElement ||
              (e.fileElement &&
                /\.(png|jpe?g|webp|gif|bmp)$/i.test(v.fileName || ""))
            ? "image"
            : e.videoElement
              ? "video"
              : e.pttElement
                ? "audio"
                : "file";
      attachments.push({
        element_id: String(e.elementId),
        kind,
        name: cleanText(v.fileName || kind),
        path:
          v.sourcePath && fs.existsSync(v.sourcePath)
            ? v.sourcePath
            : (e.picElement && resources.imagePath(v)) ||
              v.sourcePath ||
              v.filePath ||
              "",
        size: String(v.fileSize ?? 0),
        width: Number(v.picWidth || 0),
        height: Number(v.picHeight || 0),
      });
      parts.push({ type: "attachment", element_id: String(e.elementId) });
    } else if (e.faceElement) {
      const id = String(e.faceElement.faceIndex);
      const name = faceName(e.faceElement.faceText, `表情 ${id}`);
      const source = resources.facePath(id);
      text += `[${name}]`;
      if (source && fs.existsSync(source)) {
        attachments.push({
          element_id: String(e.elementId),
          kind: "emoji",
          name,
          path: source,
          size: "0",
        });
        parts.push({ type: "attachment", element_id: String(e.elementId) });
      } else parts.push({ type: "text", text: `[${name}]` });
    } else if (e.arkElement && raw.chatType === 103) {
      const content = mailText(e.arkElement);
      text += content;
      parts.push({ type: "text", text: content });
    } else if (e.arkElement) {
      const data = arkData(e.arkElement);
      if (data?.app === "com.tencent.multimsg") {
        forward = e.arkElement.bytesData;
        text += "[合并转发聊天记录]";
      } else {
        const content = cardText(data);
        text += content;
        card_parts.push(parts.length);
        parts.push({ type: "text", text: content });
      }
    } else if (e.multiForwardMsgElement) {
      forward = cleanText(e.multiForwardMsgElement.xmlContent);
      text += "[合并转发聊天记录]";
    } else if (e.marketFaceElement) {
      const name = faceName(e.marketFaceElement.faceName, "商城表情");
      const id = String(e.marketFaceElement.emojiId || "");
      const content = `[${name}]`;
      text += content;
      if (/^[a-fA-F0-9]{32}$/.test(id)) {
        const source = resources.marketFacePath(id);
        attachments.push({
          element_id: String(e.elementId),
          kind: "sticker",
          name: `${name}.gif`,
          path: source && fs.existsSync(source) ? source : "",
          size: "0",
        });
        parts.push({ type: "attachment", element_id: String(e.elementId) });
      } else parts.push({ type: "text", text: content });
    } else if (String(e.grayTipElement?.xmlElement?.templId) === "10382") {
      reaction_notice = cleanText(e.grayTipElement.xmlElement.content);
      text += "[表情回应]";
    } else {
      const content = e.grayTipElement
        ? grayTipText(e.grayTipElement, names, raw)
        : `[消息类型 ${e.elementType}]`;
      text += content;
      parts.push({ type: "text", text: content });
    }
  }
  const noticeSource = json((raw.elements || []).filter(e => e.grayTipElement));
  return {
    account,
    peer: { chat_type: raw.chatType, uid: String(raw.peerUid) },
    id: String(raw.msgId),
    seq: String(raw.msgSeq ?? "0"),
    time: String(raw.msgTime ?? "0"),
    sender: cleanText(
      raw.sendMemberName || raw.sendNickName || raw.senderUin || raw.senderUid,
    ),
    sender_uid: String(raw.senderUid ?? ""),
    is_self: String(raw.senderUin) === account,
    reactions: recalled
      ? []
      : (raw.emojiLikesList || []).map((r) => ({
          emoji_id: String(r.emojiId),
          emoji_type: String(r.emojiType),
          count: String(r.likesCnt || "0"),
          clicked: r.isClicked === true || String(r.isClicked) === "1",
          label: resources.reactionLabel(
            String(r.emojiId),
            String(r.emojiType),
          ),
          path:
            String(r.emojiType) === "1"
              ? resources.facePath(String(r.emojiId))
              : "",
        })),
    system_notice: (raw.elements || []).some((e) => e.grayTipElement),
    reaction_notice: recalled ? null : reaction_notice,
    notice_names: Object.fromEntries(Object.entries(names).filter(([uid]) =>
      uid && uid !== "0" && noticeSource.includes(uid)
    ).slice(0, 64)),
    text: recalled ? (raw.elements || []).filter(e => e.grayTipElement &&
      (e.grayTipElement.revokeElement || e.grayTipElement.subElementType === 1))
      .map(e => grayTipText(e.grayTipElement, names, raw)).join("") : text,
    card_parts: recalled ? [] : card_parts,
    parts: recalled ? [] : parts,
    forward: recalled ? null : forward,
    attachments: recalled ? [] : attachments,
    reply_to: recalled ? null : reply_to,
    reply_seq: recalled ? null : reply_seq,
    send_status: raw.sendStatus ?? 0,
    correlation: String(raw.guildId ?? ""),
    recalled,
  };
}

class NativeBridge {
  constructor(socketPath, options = {}) {
    if (!socketPath || !path.isAbsolute(socketPath))
      throw new Error("Absolute QQTUI_SOCKET required");
    this.socketPath = socketPath;
    this.epoch = crypto.randomUUID();
    this.seq = 0;
    this.phase = "starting";
    this.account = "";
    this.clients = new Set();
    this.registrations = [];
    this.receipts = new Map();
    this.inflight = new Map();
    this.mediaWaiters = new Map();
    this.notificationIds = new Map();
    this.unreadCounts = new Map();
    this.groupWaiters = new Set();
    this.profileWaiters = new Map();
    this.contacts = new Map();
    this.views = new Map();
    this.viewRequests = new Map();
    this.viewCancels = new Set();
    this.viewWaiters = new Map();
    this.active = false;
    this.errors = [];
    this.options = options;
    this.journal = path.join(
      options.stateDir || path.dirname(socketPath),
      "send-journal.sqlite3",
    );
    this.resources = new QqResources({
      ...options,
      stateDir: path.dirname(this.journal),
    });
    this.stickers = new Map();
    this.journalStore = new Journal(this.journal);
    this.presentation = new Presentation(this);
    this.journalStore.open().catch((error) => {
      this.journalError = error.message;
      this.phase = "incompatible";
      this.errors.push(error.message);
      this.event("status", { status: this.status() });
    });
  }
  sessionContext() {
    const { account, epoch, session, msg } = this;
    return {
      account,
      epoch,
      session,
      msg,
      check: () => {
        if (
          this.phase !== "ready" ||
          this.account !== account ||
          this.epoch !== epoch ||
          this.session !== session ||
          this.msg !== msg
        )
          throw new Error("QQ session changed during operation");
      },
    };
  }
  listen() {
    if (process.platform !== "win32")
      fs.mkdirSync(path.dirname(this.socketPath), {
        recursive: true,
        mode: 0o700,
      });
    // The supervisor owns stale-socket removal after it has excluded a live host.
    this.server = net.createServer((socket) => {
      socket.subscribed = false;
      this.clients.add(socket);
      let buffer = "";
      socket.setEncoding("utf8");
      socket.on("data", (chunk) => {
        buffer += chunk;
        if (Buffer.byteLength(buffer) > MAX_FRAME) return socket.destroy();
        for (;;) {
          const end = buffer.indexOf("\n");
          if (end < 0) break;
          const frame = buffer.slice(0, end);
          buffer = buffer.slice(end + 1);
          this.handle(socket, frame);
        }
      });
      socket.on("error", () => {});
      socket.on("close", () => {
        this.clients.delete(socket);
        this.options.desktop?.viewer(socket, false);
        if (![...this.clients].some((s) => s.subscribed)) this.unsubscribe();
      });
    });
    this.server.on("error", (e) => {
      logger.error("[qqtui] bridge:", e.message);
    });
    this.server.listen(this.socketPath, () => {
      if (process.platform !== "win32") fs.chmodSync(this.socketPath, 0o600);
    });
  }
  write(socket, value) {
    if (socket.destroyed) return;
    if (socket.writableLength > MAX_FRAME) return socket.destroy();
    socket.write(json(value) + "\n");
  }
  event(kind, data = {}) {
    if (kind === "status") this.options.desktop?.setReady(this.phase === "ready");
    if (![...this.clients].some((s) => s.subscribed)) return;
    const event = {
      epoch: this.epoch,
      seq: ++this.seq,
      account: this.account,
      kind,
      ...data,
    };
    for (const socket of this.clients)
      if (socket.subscribed)
        this.write(socket, { jsonrpc: "2.0", method: "event", params: event });
  }
  attach(session, wrapper, login) {
    if (this.session && this.session !== session) {
      this.epoch = crypto.randomUUID();
      this.seq = 0;
      this.unsubscribe(true);
      this.account = "";
    }
    this.session = session;
    this.wrapper = wrapper;
    this.login = login;
    this.phase = "login";
    this.event("status", { status: this.status() });
  }
  ready(config) {
    if (this.journalError) {
      this.phase = "incompatible";
      return;
    }
    const account = String(config.selfUin ?? "");
    if (!account || account === "0") return;
    const changed = account !== this.account;
    if (changed) {
      this.epoch = crypto.randomUUID();
      this.seq = 0;
      this.stickers.clear();
      this.notificationIds.clear();
      this.unsubscribe(true);
    }
    this.account = account;
    const active = path.join(path.dirname(this.journal), "active-account.json");
    const temporary = active + "." + crypto.randomUUID();
    try {
      fs.writeFileSync(temporary, json({ account }), {
        mode: 0o600,
        flag: "wx",
      });
      fs.renameSync(temporary, active);
    } catch (error) {
      this.errors.push("Account state: " + cleanText(error.message));
    } finally {
      try {
        fs.unlinkSync(temporary);
      } catch (_) {}
    }
    this.selfUid = String(config.selfUid ?? "");
    this.phase = "ready";
    this.msg = this.session.getMsgService();
    this.recent = this.session.getRecentContactService();
    this.subscribe();
    this.event("status", { status: this.status() });
    logger.log("[qqtui] native session ready");
  }
  status() {
    const capabilities =
      this.phase === "ready"
        ? [
            "recent",
            "history",
            "history_navigation",
            "request_context",
            "send",
            "recall",
            "read",
            "contacts",
            "media",
            "avatars",
            "speech_permissions",
          ]
        : [];
    if (this.options.desktop) capabilities.push("desktop_viewer");
    if (this.phase === "ready" && typeof this.msg?.getFirstUnreadMsgSeq === "function" && typeof this.msg?.getSingleMsg === "function") capabilities.push("unread_anchor");
    if (this.phase === "ready") {
      if (
        typeof this.msg?.fetchFavEmojiList === "function" ||
        this.resources.faces.size
      )
        capabilities.push("stickers");
      if (
        ["setMsgEmojiLikes", "getMsgEmojiLikesList"].every(
          (method) => typeof this.msg?.[method] === "function",
        ) &&
        typeof this.msg?.getSingleMsg === "function"
      )
        capabilities.push("reactions");
    }
    return {
      protocol_version: 2,
      phase: this.phase,
      account: this.account,
      native_account_path:
        this.sessionConfig?.desktopPathConfig?.account_path || "",
      profile_dir: this.options.profileDir || "",
      epoch: this.epoch,
      seq: this.seq,
      qq_version: this.options.qqVersion || "",
      errors: [...this.errors, ...this.resources.errors],
      recent_shape: this.recentShape,
      capabilities,
    };
  }
  async forwardMessages(peer, ids) {
    if (
      !Array.isArray(ids) ||
      !ids.length ||
      ids.length > 8 ||
      ids.some((id) => !isNativeId(id))
    )
      throw new Error("Forward path must contain 1..8 decimal message IDs");
    const context = this.sessionContext();
    let raws =
      checked(await context.msg.getMsgsByMsgId(peer, [ids[0]])).msgList || [];
    context.check();
    for (const id of ids) {
      const source = raws.find((m) => String(m.msgId) === id);
      if (!source?.elements?.some((e) => e.multiForwardMsgElement || arkData(e.arkElement)?.app === "com.tencent.multimsg"))
        throw new Error("Forward message unavailable");
      raws =
        checked(await context.msg.getMultiMsg(peer, ids[0], id)).msgList || [];
      context.check();
    }
    return raws;
  }
  async normalizeMessages(raws, context = this.sessionContext()) {
    context.check();
    const groups = await this.presentation.messageNames(raws);
    context.check();
    return raws.map(raw => message(raw, context.account, this.resources, groups.get(String(raw.peerUid))));
  }
  async messagesBySequence(peer, seq) {
    const context = this.sessionContext();
    const msg = context.msg;
    if (typeof msg.getSingleMsg !== "function")
      throw new Error("QQ single-message lookup unavailable");
    const result = checked(await msg.getSingleMsg(peer, seq));
    context.check();
    if (!Array.isArray(result.msgList))
      throw new Error("QQ single-message lookup returned no message list");
    return result.msgList.filter(raw => samePeer(raw, peer) && String(raw.msgSeq) === seq &&
      !(raw.elements || []).some(e => e.grayTipElement && !e.grayTipElement.revokeElement && e.grayTipElement.subElementType !== 1));
  }
  subscribe() {
    if (this.active || this.phase !== "ready") return;
    const add = (service, method, remove, handlers) => {
      const account = this.account;
      const session = this.session;
      const epoch = this.epoch;
      const object = listener(
        Object.fromEntries(
          Object.entries(handlers).map(([key, fn]) => [
            key,
            (...args) => {
              const lifecycle =
                ["onAddSendMsg", "onMsgInfoListUpdate"].includes(key) ||
                key === "onRichMediaDownloadComplete" ||
                key === "onEmojiResourceUpdate" ||
                key === "onKickedOffLine" ||
                key === "onSelfStatusChanged" ||
                method === "addKernelRecentContactListener";
              if (
                epoch !== this.epoch ||
                account !== this.account ||
                session !== this.session ||
                (!lifecycle && ![...this.clients].some((s) => s.subscribed))
              )
                return;
              try {
                return fn(...args);
              } catch (e) {
                this.errors.push(cleanText(e.message));
                this.errors = this.errors.slice(-5);
                logger.error("[qqtui] callback:", cleanText(e.message));
              }
            },
          ]),
        ),
      );
      const id = service[method](object);
      this.registrations.push({ service, remove, id, object });
    };
    const invalidate = () => this.event("recent.invalidated");
    const recentEvent =
      (name) =>
      (...args) => {
        if (name === "onMsgUnreadCountUpdate") {
          const counts =
            args[0] instanceof Map
              ? args[0].values()
              : Object.values(args[0] || {});
          for (const info of counts)
            if (
              info &&
              Number.isInteger(info.listType) &&
              /^[0-9]+$/.test(String(info.msgListUnreadCnt))
            )
              this.unreadCounts.set(
                info.listType,
                String(info.msgListUnreadCnt),
              );
        }
        if (name === "onRecentContactListChangedVer2") {
          this.applyRecentViews(args[0] || []);
        }
        if (name === "onRecentContactNotification" && Array.isArray(args[0])) {
          const unread = String(args[1]?.msgListUnreadCnt || "0");
          if (Number.isInteger(args[1]?.listType))
            this.unreadCounts.set(args[1].listType, unread);
          const items = args[0]
            .filter((c) => {
              const key = `${c.chatType}:${c.peerUid}`,
                id = String(c.msgId || "");
              if (c.notifiedType !== NOTIFICATION_ALERT) return false;
              if (
                !id ||
                id === "0" ||
                c.isOnlineMsg !== true ||
                String(c.senderUin) === this.account ||
                (this.selfUid && c.senderUid === this.selfUid) ||
                this.notificationIds.get(key) === id
              )
                return false;
              if (this.notificationIds.size >= 512)
                this.notificationIds.delete(
                  this.notificationIds.keys().next().value,
                );
              this.notificationIds.set(key, id);
              return true;
            })
            .map((c) => ({
              key: `${c.chatType}:${c.peerUid}`,
              target: { account: this.account, peer: { chat_type: c.chatType, uid: String(c.peerUid) }, id: String(c.msgId) },
              title: `[${unread}] QQ · ${this.account} · ${cleanText(c.remark || c.peerName || c.peerUin || "消息提醒")}`,
              body: cleanText(
                (c.abstractContent || []).map((e) => e.content || "").join(""),
              ),
            }));
          if (items.length)
            this.event("notification", {
              notification: { unread, items: items.slice(0, 64) },
            });
        }
        invalidate();
      };
    add(
      this.recent,
      "addKernelRecentContactListener",
      "removeKernelRecentContactListener",
      Object.fromEntries(
        [
          "onRecentContactListChanged",
          "onRecentContactListChangedVer2",
          "onRecentContactNotification",
          "onMsgUnreadCountUpdate",
          "onDeletedContactsNotify",
        ].map((k) => [k, recentEvent(k)]),
      ),
    );
    add(this.msg, "addKernelMsgListener", "removeKernelMsgListener", {
      onRecvMsg: (raws) => this.messages(raws),
      onMsgInfoListUpdate: (raws) => this.messages(raws),
      onAddSendMsg: (raw) => this.messages([raw]),
      onMsgRecall: (chatType, uid, seq) => {
        if (
          [1, 2, 100].includes(chatType) &&
          typeof uid === "string" &&
          seq &&
          String(seq) !== "0"
        ) {
          const contact = [...this.contacts.values()].find(
            (c) =>
              c.chatType === chatType &&
              (c.peerUid === uid || c.peerUin === uid),
          );
          this.event("message.recalled", {
            message_peer: { chat_type: chatType, uid: contact?.peerUid || uid },
            message_seq: String(seq),
          });
        }
        this.event("messages.invalidated");
        invalidate();
      },
      onContactUnreadCntUpdate: invalidate,
      onUnreadCntUpdate: invalidate,
      onKickedOffLine: (info) => {
        this.phase = "offline";
        const reason = cleanText(
          info.tipsDesc || info.tipsTitle || "QQ session is offline",
        );
        logger.error(
          `${new Date().toISOString()} ${process.pid} [qqtui] kicked offline:`,
          json({
            title: cleanText(info.tipsTitle || ""),
            desc: cleanText(info.tipsDesc || ""),
            appId: info.appId,
            instanceId: info.instanceId,
            sameDevice: info.sameDevice,
            kickedType: info.kickedType,
            securityKickedType: info.securityKickedType,
          }).slice(0, 1024),
        );
        this.errors.push(reason);
        this.errors = this.errors.slice(-5);
        this.event("status", { status: this.status() });
      },
      onEmojiResourceUpdate: () => {
        this.resources.reload();
        this.stickers.clear();
        this.event("stickers.invalidated");
      },
      onRichMediaDownloadComplete: (result) => {
        const key = `${result.msgId}:${result.msgElementId}`;
        const request = this.mediaWaiters.get(key);
        if (request) {
          const code = result.fileErrCode ?? result.result ?? 0;
          if (String(code) !== "0")
            request.finish(
              new Error(
                `QQ media download failed: ${code}${result.fileErrMsg ? ": " + cleanText(result.fileErrMsg).slice(0, 256) : ""}`,
              ),
            );
          else if (
            typeof result.filePath !== "string" ||
            !result.filePath ||
            !fs.existsSync(result.filePath)
          )
            request.finish(
              new Error("QQ media download returned no local file"),
            );
          else request.finish(null, result);
        }
      },
    });
    add(
      this.session.getGroupService(),
      "addKernelGroupListener",
      "removeKernelGroupListener",
      {
        onGroupDetailInfoChange: (info) => {
          this.presentation.detail(info);
        },
        onMemberInfoChange: (group, _source, members) =>
          this.presentation.member(group, members),
        onGroupListUpdate: (_type, groups) => {
          this.presentation.update(groups);
          for (const done of this.groupWaiters) done(groups);
          this.groupWaiters.clear();
        },
      },
    );
    add(
      this.session.getProfileService(),
      "addKernelProfileListener",
      "removeKernelProfileListener",
      {
        onUserDetailInfoChanged: (info) => {
          this.event("avatars.invalidated");
          for (const done of this.profileWaiters.get(info.uid) || [])
            done(info);
        },
        onSelfStatusChanged: (info) => {
          const phase = this.journalError
            ? "incompatible"
            : info.status === 20
              ? "offline"
              : "ready";
          logger.log(
            `${new Date().toISOString()} ${process.pid} [qqtui] self status:`,
            json({
              status: info.status,
              extStatus: info.extStatus,
              offlineType: info.offlineType,
              termType: info.termType,
              netType: info.netType,
            }),
          );
          if (this.phase !== phase) {
            this.phase = phase;
            this.event("status", { status: this.status() });
          }
        },
      },
    );
    const buddy = this.session.getBuddyService?.();
    if (typeof buddy?.addKernelBuddyListener === "function")
      add(buddy, "addKernelBuddyListener", "removeKernelBuddyListener", {
        onAvatarUrlUpdated: () => this.event("avatars.invalidated"),
      });
    this.active = true;
  }
  unsubscribe(force = false) {
    if (!force) return;
    this.presentation.close();
    this.contacts.clear();
    this.views.clear();
    this.unreadCounts.clear();
    for (const request of [...this.mediaWaiters.values()])
      request.finish(new Error("QQ media session changed"));
    this.mediaWaiters.clear();
    for (const cancel of [...this.viewCancels]) cancel();
    this.viewRequests.clear();
    this.viewWaiters.clear();
    for (const r of this.registrations) {
      try {
        if (r.id !== undefined) r.service[r.remove](r.id);
      } catch (e) {
        logger.error("[qqtui] listener cleanup:", e.message);
      }
    }
    // A service returning void has no removable registration in upstream's
    // declaration. Retain it once; callbacks do no work without subscribers.
    this.registrations = [];
    this.active = false;
  }
  messages(raws) {
    const subscribed = [...this.clients].some((s) => s.subscribed);
    for (const raw of raws || []) {
      if (
        raw.sendStatus === undefined ||
        (String(raw.senderUin) !== this.account &&
          String(raw.senderUid) !== this.selfUid)
      )
        continue;
      const ids = [String(raw.guildId || ""), String(raw.msgId || "")].filter(
        (id) => id && id !== "0",
      );
      for (const id of ids)
        if (
          ![...this.receipts.values()].some(
            (r) =>
              r.account === this.account &&
              (r.native_id === id || r.message_id === id),
          )
        ) {
          const account = this.account;
          this.journalStore
            .byNative(account, id)
            .then((receipt) => {
              if (!receipt) return;
              this.remember(receipt);
              return this.updateReceipt(
                receipt,
                message(raw, account, this.resources),
              );
            })
            .catch((error) =>
              logger.error("[qqtui] receipt lookup:", error.message),
            );
        }
    }
    if (!subscribed) {
      raws = (raws || []).filter(
        (raw) =>
          raw.sendStatus !== undefined &&
          (String(raw.senderUin) === this.account ||
            String(raw.senderUid) === this.selfUid) &&
          [...this.receipts.values()].some(
            (r) =>
              r.account === this.account &&
              peerKey(r.peer) === `${raw.chatType}:${raw.peerUid}` &&
              (r.native_id === String(raw.msgId) ||
                r.native_id === String(raw.guildId)),
          ),
      );
      if (!raws.length) return;
    }
    const messages = (raws ?? []).map((raw) =>
      message(raw, this.account, this.resources),
    );
    for (const [index, m] of messages.entries())
      for (const receipt of this.receipts.values()) {
        if (raws[index].sendStatus === undefined) continue;
        if (
          receipt.account !== this.account ||
          peerKey(receipt.peer) !== peerKey(m.peer) ||
          (receipt.native_id !== m.id && receipt.native_id !== m.correlation)
        )
          continue;
        this.updateReceipt(receipt, m);
      }
    const routable = (raw) =>
      Number.isInteger(raw.chatType) &&
      raw.chatType > 0 &&
      typeof raw.peerUid === "string" &&
      raw.peerUid &&
      raw.msgId !== undefined;
    const complete = messages.filter(
      (_, i) => routable(raws[i]) && Array.isArray(raws[i].elements),
    );
    const partial = messages.filter(
      (_, i) =>
        routable(raws[i]) &&
        !Array.isArray(raws[i].elements) &&
        Array.isArray(raws[i].emojiLikesList),
    );
    if (complete.length) this.event("messages", { messages: complete });
    if (complete.some(m => m.system_notice)) {
      this.normalizeMessages(raws.filter(raw => routable(raw) && Array.isArray(raw.elements)))
        .then(messages => this.event("notices.updated", { messages: messages.filter(m => m.system_notice) }))
        .catch(error => logger.error("[qqtui] notice names:", error.message));
    }
    if (partial.length) this.event("reactions.updated", { messages: partial });
    if (complete.length + partial.length < messages.length)
      this.event("messages.invalidated");
  }
  updateReceipt(receipt, m) {
    receipt.message_id = m.id;
    let next = receipt.state;
    if (next === "confirmed" && m.send_status !== 2) return null;
    if (m.send_status === 2) next = "confirmed";
    else if (m.send_status === 0) next = "failed";
    else if (m.send_status === 3) next = "unknown";
    if (next === receipt.state) return null;
    receipt.state = next;
    if (next === "failed" && !receipt.error) receipt.error = "QQ 原生发送失败";
    return this.settle(receipt);
  }
  settle(receipt) {
    return this.saveJournal(receipt)
      .then(() => this.event("send.updated", { receipt }))
      .catch(() => {});
  }
  saveJournal(receipt) {
    const result = this.journalStore.put(
      receipt.account + ":" + receipt.request_id,
      receipt,
    );
    result.catch((error) => {
      this.errors.push("Send journal: " + cleanText(error.message));
      this.errors = this.errors.slice(-5);
      this.journalError = error.message;
      this.phase = "incompatible";
      this.event("status", { status: this.status() });
    });
    return result;
  }
  remember(receipt) {
    const key = receipt.account + ":" + receipt.request_id;
    this.receipts.delete(key);
    this.receipts.set(key, receipt);
    while (this.receipts.size > 256)
      this.receipts.delete(this.receipts.keys().next().value);
    return receipt;
  }
  async handle(socket, line) {
    let request;
    try {
      request = JSON.parse(line);
      if (
        request.jsonrpc !== "2.0" ||
        !Number.isSafeInteger(request.id) ||
        typeof request.method !== "string"
      )
        throw new Error("Invalid JSON-RPC request");
      const account = this.account;
      const session = this.session;
      const epoch = this.epoch;
      const result = await this.call(
        request.method,
        request.params ?? {},
        socket,
      );
      if (
        !["host.status", "events.subscribe"].includes(request.method) &&
        (epoch !== this.epoch ||
          account !== this.account ||
          session !== this.session)
      )
        throw new Error("Account changed; refresh the session");
      this.write(socket, { jsonrpc: "2.0", id: request.id, result });
    } catch (e) {
      this.write(socket, {
        jsonrpc: "2.0",
        id: request?.id ?? null,
        error: { code: -32000, message: String(e.message) },
      });
    }
  }
  applyRecentViews(updates) {
    for (const view of updates) {
      for (const contact of view.changedList || [])
        this.contacts.set(String(contact.contactId), contact);
      if (Array.isArray(view.sortedContactList)) {
        const previous = this.views.get(view.listType);
        const offset = Number(view.firstContactPos || 0);
        if (!Number.isSafeInteger(offset) || offset < 0 || offset > 10000)
          throw new Error("Invalid native recent window offset");
        const hasPrefix =
          offset === 0 ||
          (previous &&
            Number(previous.firstContactPos || 0) === 0 &&
            offset <= previous.sortedContactList.length);
        const order =
          hasPrefix && previous && offset > 0
            ? previous.sortedContactList
                .slice(0, offset)
                .concat(view.sortedContactList)
            : view.sortedContactList;
        this.views.set(view.listType, {
          ...view,
          sortedContactList: order,
          firstContactPos: hasPrefix ? "0" : view.firstContactPos,
        });
      }
      const waiting = this.viewWaiters.get(view.listType);
      for (const done of waiting || []) done(view);
      if (waiting && !waiting.size) this.viewWaiters.delete(view.listType);
    }
  }
  async ensureView(listType, anchor = null) {
    if (
      !anchor &&
      this.views.has(listType) &&
      Number(this.views.get(listType).firstContactPos || 0) === 0
    )
      return;
    if (this.viewRequests.has(listType)) return this.viewRequests.get(listType);
    const epoch = this.epoch;
    const previousCount =
      this.views.get(listType)?.sortedContactList.length || 0;
    let cancel;
    const request = new Promise((resolve, reject) => {
      const finish = (error) => {
        clearTimeout(timer);
        this.viewWaiters.get(listType)?.delete(done);
        this.viewCancels.delete(cancel);
        if (error) reject(error);
        else resolve();
      };
      const done = (view) => {
        if (!anchor && Number(view.firstContactPos || 0) !== 0) return;
        if (
          anchor &&
          Number(view.firstContactPos || 0) < anchor.pos &&
          view.sortedContactList.length <= previousCount
        )
          return;
        finish();
      };
      const timer = setTimeout(
        () => finish(new Error("Native recent view timed out")),
        10000,
      );
      cancel = () =>
        finish(new Error("Session context changed during recent query"));
      this.viewCancels.add(cancel);
      if (!this.viewWaiters.has(listType))
        this.viewWaiters.set(listType, new Set());
      this.viewWaiters.get(listType).add(done);
      try {
        this.recent.fetchAndSubscribeABatchOfRecentContact({
          anchorPointContact: anchor || {
            contactId: "",
            sortField: "",
            pos: 0,
          },
          relativeMoveCount: 0,
          listType,
          count: 200,
          fetchOld: true,
        });
      } catch (e) {
        finish(e);
      }
    });
    if (epoch === this.epoch) this.viewRequests.set(listType, request);
    else cancel();
    try {
      return await request;
    } finally {
      if (this.viewRequests.get(listType) === request)
        this.viewRequests.delete(listType);
    }
  }
  async completeView(listType) {
    while (this.views.get(listType)?.moreDataAtTheBottom) {
      const view = this.views.get(listType);
      const count = view.sortedContactList.length;
      if (count >= 10000 || !count)
        throw new Error("Native recent list exceeds the supported window");
      const contactId = String(view.sortedContactList[count - 1]);
      const contact = this.contacts.get(contactId);
      if (!contact) throw new Error("Native recent anchor is missing");
      await this.ensureView(listType, {
        contactId,
        sortField: contact.sortField,
        pos: count - 1,
      });
      if (
        this.views.get(listType).sortedContactList.length <= count &&
        this.views.get(listType).moreDataAtTheBottom
      )
        throw new Error("Native recent pagination made no progress");
    }
  }
  async snapshot(listType = 1) {
    await this.ensureView(listType);
    const seq = this.seq;
    const result = checked(
      await this.recent.getRecentContactListSnapShot(1000),
    );
    const info = result.info || result;
    checked(info);
    for (const contact of info.changedList || [])
      this.contacts.set(String(contact.contactId), contact);
    await this.completeView(listType);
    if (
      listType === 1 &&
      this.views
        .get(1)
        ?.sortedContactList.some(
          (id) => this.contacts.get(String(id))?.chatType === 7,
        )
    ) {
      await this.ensureView(2);
      await this.completeView(2);
    }
    const normalize = (view) =>
      (view?.sortedContactList || [])
        .map((id) => this.contacts.get(String(id)))
        .filter(Boolean)
        .map((c) => ({
          peer: { chat_type: c.chatType, uid: String(c.peerUid) },
          id: String(c.contactId),
          name:
            c.chatType === 7
              ? "群助手"
              : c.chatType === 8
                ? "我的手机"
                : cleanText(
                    c.remark ||
                      c.peerName ||
                      (c.peerUin !== "0" ? c.peerUin : "") ||
                      "QQ 入口 " + c.chatType,
                  ),
          unread: String(
            (c.chatType === 7 ? c.unreadChatCnt : c.unreadCnt) || "0",
          ),
          pinned: Boolean(c.topFlag),
          preview: cleanText(
            (c.abstractContent || []).map((e) => e.content || "").join(""),
          ),
          last_id: String(c.msgId || ""),
          children:
            c.chatType === 7 && view !== this.views.get(2)
              ? normalize(this.views.get(2))
              : [],
        }));
    const view = this.views.get(listType);
    if (!view) throw new Error("Native recent view was invalidated");
    return {
      account: this.account,
      epoch: this.epoch,
      seq,
      conversations: normalize(view),
      unread_total: this.unreadCounts.get(1) ?? null,
      has_more: Boolean(view.moreDataAtTheBottom),
    };
  }
  async call(method, p, socket) {
    if (method === "host.status") return this.status();
    if (method === "host.viewer") {
      if (!this.options.desktop) throw new Error("QQ desktop control unavailable");
      if (typeof p.active !== "boolean") throw new Error("Viewer state required");
      this.options.desktop.viewer(socket, p.active);
      return {};
    }
    if (method === "accounts.list") {
      const login = this.wrapper?.NodeIKernelLoginService?.get?.();
      if (typeof login?.getLoginList !== "function") return { items: [] };
      const result = checked(await login.getLoginList());
      return {
        items: (result.LocalLoginInfoList || []).map((item) => ({
          account: String(item.uin),
          name: cleanText(item.nickName),
        })),
      };
    }
    if (method === "events.subscribe") {
      socket.subscribed = true;
      this.subscribe();
      return this.status();
    }
    if (this.phase !== "ready")
      throw new Error("QQ is waiting for login / initialization");
    if (p.account !== this.account)
      throw new Error("Account changed; refresh the session");
    const context = this.sessionContext();
    if (
      method !== "messages.send" &&
      ([
        "messages.read",
        "messages.recall",
        "conversations.open",
        "reactions.set",
      ].includes(method) ||
        p.epoch !== undefined) &&
      p.epoch !== this.epoch
    )
      throw new Error("Session context changed; refresh the session");
    if (method === "conversations.snapshot") return this.snapshot();
    if (method === "conversations.children" && p.peer?.chat_type === 7)
      return this.snapshot(2);

    if (method === "conversations.read") {
      const context = this.sessionContext();
      if (p.epoch !== this.epoch || typeof p.id !== "string")
        throw new Error("Conversation context required");
      await this.snapshot();
      context.check();
      const contact = this.contacts.get(p.id);
      if (
        !contact ||
        contact.chatType !== p.peer?.chat_type ||
        String(contact.peerUid) !== p.peer?.uid
      )
        throw new Error("Conversation changed; refresh the list");
      if (contact.chatType === 7) {
        await this.ensureView(2);
        await this.completeView(2);
        context.check();
        const ids = [...(this.views.get(2)?.sortedContactList || [])];
        for (const id of ids) {
          const child = this.contacts.get(String(id));
          if (!child || !Number(child.unreadCnt)) continue;
          checked(
            await context.msg.setMsgRead(
              nativePeer({
                chat_type: child.chatType,
                uid: String(child.peerUid),
              }),
            ),
          );
          context.check();
        }
        checked(await this.recent.clearMsgUnreadCount(p.id));
        context.check();
      } else if ([1, 2, 8, 100, 103].includes(contact.chatType)) {
        checked(await context.msg.setMsgRead(nativePeer(p.peer)));
        context.check();
      } else {
        checked(await this.recent.clearMsgUnreadCount(p.id));
        context.check();
      }
      this.event("recent.invalidated");
      return {};
    }
    if (method === "contacts.list") {
      const categories = checked(
        await this.session.getBuddyService().getBuddyListV2("0", false, 0),
      ).data;
      const uids = [...new Set(categories.flatMap((c) => c.buddyUids))];
      const profiles = await this.session
        .getProfileService()
        .getCoreAndBaseInfo("nodeStore", uids);
      const friends = [...profiles].map(([uid, v]) => ({
        ...v.baseInfo,
        ...v.coreInfo,
        uid,
        uin: String(v.coreInfo?.uin || v.uin || ""),
        nick: cleanText(v.coreInfo?.nick || v.nick),
        remark: cleanText(v.coreInfo?.remark || v.remark),
      }));
      const groups = await new Promise((resolve, reject) => {
        const done = (groups) => {
          clearTimeout(timer);
          resolve({ groupList: groups });
        };
        const timer = setTimeout(() => {
          this.groupWaiters.delete(done);
          reject(new Error("Group list timed out"));
        }, 15000);
        this.groupWaiters.add(done);
        Promise.resolve(this.session.getGroupService().getGroupList(false))
          .then(checked)
          .catch((e) => {
            clearTimeout(timer);
            this.groupWaiters.delete(done);
            reject(e);
          });
      });
      const items = [
        ...friends.map((f) => ({
          peer: { chat_type: 1, uid: f.uid },
          id: "1:" + f.uid,
          name: cleanText(f.remark || f.nick || f.uid),
          unread: "0",
          pinned: false,
        })),
        ...(groups.groupList || []).map((g) => ({
          peer: { chat_type: 2, uid: String(g.groupCode) },
          id: "2:" + g.groupCode,
          name: cleanText(g.groupName || g.groupCode),
          unread: "0",
          pinned: false,
        })),
      ];
      return { items };
    }
    if (method === "members.list") {
      const result = checked(
        await this.session
          .getGroupService()
          .getAllMemberList(nativePeer(p.peer).peerUid, false),
      );
      const infos = result.result?.infos || result.infos;
      return {
        items: [...infos].map(([uid, m]) => ({
          uid: String(uid),
          uin: String(m.uin || ""),
          name: cleanText(m.cardName || m.nick || uid),
        })),
      };
    }
    if (method === "profiles.info") {
      if (typeof p.uid !== "string" || !p.uid || p.uid.length > 128)
        throw new Error("Profile UID required");
      const profiles = await this.session
        .getProfileService()
        .getCoreAndBaseInfo("nodeStore", [p.uid]);
      let info = profiles.get(p.uid);
      if (!info) {
        const detail = await new Promise((resolve, reject) => {
          const waiting = this.profileWaiters.get(p.uid) || new Set();
          this.profileWaiters.set(p.uid, waiting);
          const cleanup = () => {
            clearTimeout(timer);
            waiting.delete(done);
            if (!waiting.size) this.profileWaiters.delete(p.uid);
          };
          const done = (value) => {
            cleanup();
            resolve(value);
          };
          const timer = setTimeout(() => {
            cleanup();
            reject(new Error("Profile timed out"));
          }, 10000);
          waiting.add(done);
          try {
            Promise.resolve(
              this.session
                .getProfileService()
                .fetchUserDetailInfo("BuddyProfileStore", [p.uid], 1, [0]),
            )
              .then(checked)
              .then((result) => {
                const value = result?.detail?.get(p.uid);
                if (value) done(value);
              })
              .catch((error) => {
                cleanup();
                reject(error);
              });
          } catch (error) {
            cleanup();
            reject(error);
          }
        });
        info = detail.simpleInfo || detail;
      }
      const core = info.coreInfo || info,
        base = info.baseInfo || info;
      return {
        uid: p.uid,
        uin: String(core.uin || info.uin || ""),
        nick: cleanText(core.nick),
        remark: cleanText(core.remark),
        signature: cleanText(base.longNick || info.personalNote || ""),
      };
    }
    if (method === "stickers.list") {
      const context = this.sessionContext();
      const { account, msg } = context;
      const faces = [...this.resources.faces.values()].map((face) => ({
        id: "face:" + face.id,
        kind: "face",
        name: face.name,
        path: this.resources.facePath(face.id),
        emoji_id: face.id,
        emoji_type: "1",
      }));
      if (p.favorites === false) return { items: faces };
      const result =
        typeof msg.fetchFavEmojiList === "function"
          ? checked(await msg.fetchFavEmojiList("", 2000, true, true))
          : { emojiInfoList: [] };
      context.check();
      this.stickers.clear();
      const favorites = (result.emojiInfoList || []).map((entry) => {
        const id =
          "favorite:" + String(entry.md5 || entry.resId || entry.emoId);
        this.stickers.set(id, { ...entry, account });
        const source = this.resources.favoritePath(entry);
        return {
          id,
          kind: "sticker",
          name: cleanText(
            entry.modifyWord || entry.ocrWord || `收藏表情 ${entry.emoId}`,
          ),
          path: source,
          emoji_id: "",
          emoji_type: "",
        };
      });

      return { items: [...favorites, ...faces] };
    }
    if (method === "stickers.resolve") {
      const entry = this.stickers.get(p.id);
      if (!entry || entry.account !== this.account)
        throw new Error("Sticker catalog changed; reopen the picker");
      return { path: await this.resources.favorite(entry) };
    }
    if (method === "avatars.resolve") return this.presentation.avatar(p.uid);
    const peer = nativePeer(p.peer);
    if (method === "conversations.permissions")
      return peer.chatType === 2
        ? this.presentation.speech(peer.peerUid)
        : { state: "allowed", scope: "none", reason: "", until: null };
    if (
      peer.chatType === 103 &&
      ![
        "messages.history",
        "messages.get",
        "messages.read",
        "media.resolve",
      ].includes(method)
    )
      throw new Error("QQ mailbox reminders are read-only");
    if (method === "conversations.open")
      return (await this.recent.addRecentContact(peer)) ?? {};
    if (method === "reactions.refresh") {
      if (peer.chatType !== 2 || !isNativeId(p.seq))
        throw new Error("Group message sequence required");
      const raws = await this.messagesBySequence(peer, p.seq);
      return {
        messages: await this.normalizeMessages(raws, context),
      };
    }
    if (method === "reactions.set" || method === "reactions.members") {
      if (peer.chatType !== 2 || !isNativeId(p.id))
        throw new Error("A group message ID is required");
      if (
        typeof p.emoji_id !== "string" ||
        !/^[0-9]{1,7}$/.test(p.emoji_id) ||
        !["1", "2"].includes(p.emoji_type)
      )
        throw new Error("Invalid reaction");
      const context = this.sessionContext();
      const raw = (
        checked(await context.msg.getMsgsByMsgId(peer, [p.id])).msgList || []
      ).find((raw) => String(raw.msgId) === p.id);
      context.check();
      if (
        !raw ||
        !samePeer(raw, peer) ||
        String(raw.msgSeq || "0") === "0" ||
        message(raw, this.account, this.resources).recalled
      )
        throw new Error("Message unavailable for reactions");
      if (method === "reactions.set") {
        if (p.epoch !== this.epoch || typeof p.set !== "boolean")
          throw new Error("Reaction session changed or missing desired state");
        return (
          checked(
            await context.msg.setMsgEmojiLikes(
              peer,
              String(raw.msgSeq),
              p.emoji_id,
              p.emoji_type,
              p.set,
            ),
          ) || {}
        );
      }
      if (
        p.cookie !== undefined &&
        (typeof p.cookie !== "string" || p.cookie.length > 8192)
      )
        throw new Error("Invalid reaction page cookie");
      const result = checked(
        await context.msg.getMsgEmojiLikesList(
          peer,
          String(raw.msgSeq),
          p.emoji_id,
          p.emoji_type,
          p.cookie || "",
          false,
          50,
        ),
      );
      return {
        members: (result.emojiLikesList || []).map((user) => ({
          uid: String(user.tinyId),
          name: cleanText(user.nickName),
        })),
        cookie:
          result.isLastPage || !result.cookie ? null : String(result.cookie),
      };
    }
    if (method === "messages.forward") {
      const raws = await this.forwardMessages(peer, p.path);
      return {
        messages: (await this.normalizeMessages(raws.map(raw => ({ ...raw, chatType: peer.chatType, peerUid: peer.peerUid })), context))
          .map(raw => ({ ...raw, forward_path: p.path })),
      };
    }
    if (method === "messages.sequence") {
      if (!isNativeId(p.seq)) throw new Error("Message sequence required");
      return {
        messages: await this.normalizeMessages(await this.messagesBySequence(peer, p.seq), context),
      };
    }
    if (method === "messages.quote") {
      if (!isNativeId(p.id)) throw new Error("Source message ID required");
      const source = (
        checked(await this.msg.getMsgsByMsgId(peer, [p.id])).msgList || []
      ).find((m) => String(m.msgId) === p.id);
      const reply = source?.elements?.find((e) => e.replyElement)?.replyElement;
      if (!reply) return { messages: [] };
      const id = [reply.replayMsgId, reply.sourceMsgIdInRecords]
        .map((v) => String(v ?? ""))
        .find((v) => /^[1-9][0-9]{0,19}$/.test(v));
      const seq = String(reply.replayMsgSeq ?? "");
      let originals = [];
      if (id) {
        try {
          originals = (
            checked(await this.msg.getMsgsByMsgId(peer, [id])).msgList || []
          ).filter((m) => String(m.msgId) === id);
        } catch (error) {
          if (!/^[1-9][0-9]{0,19}$/.test(seq)) throw error;
        }
      }
      if (!originals.length && /^[1-9][0-9]{0,19}$/.test(seq)) {
        originals = await this.messagesBySequence(peer, seq);
      }
      return {
        messages: await this.normalizeMessages(originals, context),
      };
    }
    if (method === "messages.get") {
      if (
        !Array.isArray(p.ids) ||
        !p.ids.length ||
        p.ids.length > 100 ||
        p.ids.some((id) => !isNativeId(id))
      )
        throw new Error("Message IDs required");
      const result = checked(await this.msg.getMsgsByMsgId(peer, p.ids));
      return {
        messages: await this.normalizeMessages(result.msgList || [], context),
      };
    }
    if (method === "conversations.unread") {
      if (peer.chatType !== 2) return {id:null};
      const result = checked(await this.msg.getFirstUnreadMsgSeq(peer));
      context.check();
      const seq = String(result.seq ?? "0");
      if (seq === "0") return {id:null};
      if (!isNativeId(seq)) throw new Error("Invalid native unread sequence");
      let raws = checked(await this.msg.getSingleMsg(peer, seq)).msgList || [];
      context.check();
      if (!raws.some(raw => samePeer(raw,peer) && String(raw.msgSeq) === seq) && typeof this.msg.getMsgsBySeqAndCount === "function") {
        raws = checked(await this.msg.getMsgsBySeqAndCount(peer,seq,1,false,false)).msgList || [];
        context.check();
      }
      const target = raws.filter(raw => samePeer(raw,peer) && isNativeId(String(raw.msgId)) && isNativeId(String(raw.msgSeq)) && BigInt(raw.msgSeq) >= BigInt(seq))
        .sort((a,b) => BigInt(a.msgSeq) < BigInt(b.msgSeq) ? -1 : BigInt(a.msgSeq) > BigInt(b.msgSeq) ? 1 : 0)[0];
      return {id:target ? String(target.msgId) : null};
    }
    if (method === "messages.history") {
      const limit = p.limit ?? 40;
      if (!Number.isInteger(limit) || limit < 1 || limit > 100)
        throw new Error("History limit must be 1..100");
      const anchors = [p.before, p.after, p.around].filter(
        (value) => value !== undefined && value !== null,
      );
      if (anchors.length > 1 || anchors.some((value) => !isNativeId(value)))
        throw new Error("Use one decimal history anchor");
      const query = async (id, count, older) =>
        count
          ? checked(
              await this.msg.getMsgs(
                { ...peer, privilegeFlag: 0 },
                id,
                count,
                older,
              ),
            ).msgList || []
          : [];
      let raws,
        has_before = false,
        has_after = false,
        anchor_found = true;
      if (p.around) {
        const anchors =
          checked(await this.msg.getMsgsByMsgId(peer, [p.around])).msgList ||
          [];
        if (anchors.length) {
          const before = Math.floor((limit - 1) / 2),
            after = limit - before - 1;
          const [older, newer] = await Promise.all([
            query(p.around, before, true),
            query(p.around, after, false),
          ]);
          raws = [...older, ...anchors, ...newer];
          has_before = before === 0 || older.length >= before;
          has_after = after === 0 || newer.length >= after;
        } else {
          anchor_found = false;
        }
      }
      if (!raws) {
        if (p.before) {
          raws = await query(p.before, limit, true);
          has_before = raws.length >= limit;
        } else if (p.after) {
          raws = await query(p.after, limit, false);
          has_after = raws.length >= limit;
        } else {
          raws =
            checked(await this.msg.getAioFirstViewLatestMsgs(peer, limit))
              .msgList || [];
          has_before = raws.length >= limit;
        }
      }
      return {
        messages: await this.normalizeMessages(raws, context),
        has_before,
        has_after,
        anchor_found,
      };
    }
    if (method === "messages.read")
      return checked(await this.msg.setMsgRead(peer));
    if (method === "messages.recall") {
      if (!isNativeId(p.id)) throw new Error("Message ID required");
      if (peer.chatType === 8)
        throw new Error("QQ device conversations do not support recall");
      return checked(await this.msg.recallMsg(peer, [String(p.id)]));
    }
    if (method === "messages.send") {
      try {
        return await this.send(p, peer);
      } catch (e) {
        if (this.receipts.has(this.account + ":" + p.request_id)) throw e;
        return {
          account: this.account,
          peer: p.peer,
          request_id: p.request_id,
          state: "failed",
          error: e.message,
        };
      }
    }
    if (method === "messages.receipt") {
      const key = this.account + ":" + p.request_id;
      let receipt =
        this.receipts.get(key) || (await this.journalStore.get(key));
      if (!receipt && typeof p.native_id === "string" && p.native_id) {
        receipt = {
          account: this.account,
          peer: p.peer,
          request_id: p.request_id,
          native_id: p.native_id,
          message_id: p.message_id || "",
          state: "unknown",
        };
        this.receipts.set(key, receipt);
      }
      if (!receipt || peerKey(receipt.peer) !== peerKey(p.peer))
        return {
          account: this.account,
          peer: p.peer,
          state: "unknown",
          request_id: p.request_id,
        };
      if (!["confirmed", "failed"].includes(receipt.state)) {
        const result = checked(
          receipt.message_id
            ? await this.msg.getMsgsByMsgId(peer, [receipt.message_id])
            : await this.msg.getAioFirstViewLatestMsgs(peer, 100),
        );
        this.messages(result.msgList);
      }
      return receipt;
    }
    if (method === "media.resolve") return this.resolve(p, peer);
    throw new Error("Unknown method");
  }
  async send(p, peer) {
    const key = this.account + ":" + p.request_id;
    const fingerprint = crypto
      .createHash("sha256")
      .update(
        json({
          peer: p.peer,
          text: p.text,
          segments: p.segments,
          reply_to: p.reply_to,
          attachments: p.attachments,
        }),
      )
      .digest("hex");
    const receipt =
      this.receipts.get(key) || (await this.journalStore.get(key));
    if (receipt) {
      this.remember(receipt);
      if (
        peerKey(receipt.peer) !== peerKey(p.peer) ||
        (receipt.fingerprint && receipt.fingerprint !== fingerprint)
      )
        throw new Error("Request ID reused with another message");
      return receipt;
    }
    const active = this.inflight.get(key);
    if (active) {
      if (active.fingerprint !== fingerprint)
        throw new Error("Request ID reused with another message");
      return active.promise;
    }
    if (this.inflight.size >= 8) throw new Error("Too many in-flight sends");
    const promise = Promise.resolve().then(() =>
      this.sendNative(p, peer, fingerprint),
    );
    this.inflight.set(key, { fingerprint, promise });
    try {
      return await promise;
    } finally {
      this.inflight.delete(key);
    }
  }
  async sendNative(p, peer, fingerprint) {
    const account = this.account;
    const session = this.session;
    const epoch = this.epoch;
    const msg = this.msg;
    const assertContext = () => {
      if (
        this.epoch !== epoch ||
        this.account !== account ||
        this.session !== session ||
        this.phase !== "ready"
      )
        throw new Error(
          "Account changed or went offline during message preparation",
        );
    };
    if (p.epoch !== epoch)
      throw new Error("Session context changed before submission");
    if (
      typeof p.request_id !== "string" ||
      !/^[a-zA-Z0-9-]{1,80}$/.test(p.request_id)
    )
      throw new Error("Request ID required");
    const key = `${this.account}:${p.request_id}`;
    if (this.receipts.has(key)) return this.receipts.get(key);
    if (
      (!p.text && !p.attachments?.length && !p.segments?.length) ||
      (p.text?.length ?? 0) > 20000
    )
      throw new Error("Invalid message");
    if (peer.chatType === 2) {
      const permission = await this.presentation.speech(peer.peerUid);
      assertContext();
      if (permission.state !== "allowed")
        throw new Error(permission.reason || "群发言权限未知");
    }
    const elements = [];
    if (p.reply_to && peer.chatType === 8)
      throw new Error("QQ device conversations do not support replies");
    if (p.reply_to) {
      if (!isNativeId(p.reply_to)) throw new Error("Reply message ID required");
      const result = checked(
        await msg.getMsgsByMsgId(peer, [String(p.reply_to)]),
      );
      const raw = result.msgList?.[0];
      if (!raw || !samePeer(raw, peer))
        throw new Error("Reply message unavailable");
      elements.push({
        elementType: 7,
        elementId: "",
        replyElement: {
          replayMsgSeq: raw.msgSeq,
          replayMsgId: raw.msgId,
          senderUin: raw.senderUin,
          senderUidStr: raw.senderUid,
          replyMsgTime: raw.msgTime,
          senderUinStr: raw.senderUin,
          replyMsgClientSeq: raw.clientSeq,
        },
      });
    }
    const segments = p.segments?.length ? p.segments : [{ text: p.text || "" }];
    for (const s of segments) {
      if (typeof s.text !== "string") throw new Error("Invalid text segment");
      if (!s.text && !s.uid) continue;
      if (s.uid && (peer.chatType !== 2 || typeof s.uin !== "string"))
        throw new Error("Mentions require a group member");
      elements.push({
        elementType: 1,
        elementId: "",
        textElement: {
          content: s.text,
          atType: s.uid ? 2 : 0,
          atUid: s.uin || "",
          atNtUid: s.uid || "",
          atTinyId: "",
        },
      });
    }
    for (const a of p.attachments ?? []) {
      if (!["image", "sticker", "face", "file"].includes(a.kind))
        throw new Error("Unsupported attachment kind");
      if (a.kind === "face") {
        const face = this.resources.faces.get(String(a.element_id));
        if (!face) throw new Error("QQ face unavailable");
        if (peer.chatType === 8)
          throw new Error("Device conversation cannot send QQ face elements");
        elements.push({
          elementType: 6,
          elementId: "",
          faceElement: face.native,
        });
        continue;
      }
      const filename = await fsp.realpath(a.path);
      const stat = await fsp.stat(filename);
      const name = path.basename(
        typeof a.name === "string" && a.name ? a.name : filename,
      );
      if (!stat.isFile() || !stat.size || stat.size > 1024 * 1024 * 1024)
        throw new Error("Invalid attachment size");
      if ((a.kind === "image" || a.kind === "sticker") && peer.chatType !== 8) {
        if (
          !Number.isInteger(a.width) ||
          !Number.isInteger(a.height) ||
          a.width <= 0 ||
          a.height <= 0
        )
          throw new Error("Image dimensions required");
        const hash = crypto.createHash("md5");
        let signature;
        for await (const chunk of fs.createReadStream(filename)) {
          signature ??= chunk.toString("latin1", 0, 6);
          hash.update(chunk);
        }
        const md5 = hash.digest("hex");
        const picSubType = a.kind === "sticker" ? 1 : 0;
        assertContext();
        const destination = msg.getRichMediaFilePathForGuild({
          md5HexStr: md5,
          fileName: name,
          elementType: 2,
          elementSubType: picSubType,
          thumbSize: 0,
          needCreate: true,
          downloadType: 1,
          file_uuid: "",
        });
        assertContext();
        if (typeof destination !== "string" || !path.isAbsolute(destination))
          throw new Error("QQ 返回了无效的媒体缓存位置");
        if (filename !== destination) {
          await fsp.mkdir(path.dirname(destination), {
            recursive: true,
            mode: 0o700,
          });
          try {
            await fsp.copyFile(filename, destination);
          } catch (error) {
            const exists = await fsp.stat(filename).then(
              () => true,
              () => false,
            );
            throw new Error(
              exists
                ? `准备 QQ 媒体缓存失败：${error.code || error.message}`
                : "附件源文件已不存在，请重新选择附件",
            );
          }
        }
        elements.push({
          elementType: 2,
          elementId: "",
          picElement: {
            md5HexStr: md5,
            fileSize: String(stat.size),
            picWidth: a.width,
            picHeight: a.height,
            fileName: name,
            sourcePath: destination,
            original: true,
            picType: signature === "GIF87a" || signature === "GIF89a" ? 2000 : 1000,
            picSubType,
            fileUuid: "",
            fileSubId: "",
            thumbFileSize: 0,
            summary: "",
          },
        });
      } else
        elements.push({
          elementType: 3,
          elementId: "",
          fileElement: {
            fileName: name,
            folderId: "",
            filePath: filename,
            fileSize: String(stat.size),
          },
        });
    }
    assertContext();
    const nativeId = msg.generateMsgUniqueId(
      peer.chatType,
      session.getMSFService().getServerTime(),
    );
    assertContext();
    const receipt = {
      request_id: p.request_id,
      account: this.account,
      peer: p.peer,
      native_id: String(nativeId),
      message_id: "",
      state: "submitting",
      fingerprint,
    };
    this.remember(receipt);
    await this.saveJournal(receipt);
    try {
      assertContext();
    } catch (error) {
      receipt.state = "failed";
      receipt.error = "发送上下文已变化，消息未提交";
      await this.saveJournal(receipt);
      return receipt;
    }
    try {
      const operation = msg.sendMsg(
        "0",
        { ...peer, guildId: String(nativeId) },
        elements,
        new Map(),
      );
      Promise.resolve(operation)
        .then((result) => {
          try {
            checked(result);
            if (receipt.state === "submitting") receipt.state = "submitted";
          } catch (e) {
            if (receipt.state !== "confirmed") {
              receipt.state = "failed";
              receipt.error = e.message;
            }
          }
          this.settle(receipt);
        })
        .catch((e) => {
          if (receipt.state !== "confirmed") {
            receipt.state = "unknown";
            receipt.error = e.message;
          }
          this.settle(receipt);
        });
    } catch (e) {
      receipt.state = "unknown";
      receipt.error = e.message;
    }
    this.settle(receipt);
    const timer = setTimeout(() => {
      if (["submitting", "submitted"].includes(receipt.state)) {
        receipt.state = "unknown";
        this.settle(receipt);
      }
    }, 12000);
    timer.unref();
    return receipt;
  }
  async resolve(p, peer) {
    if (!isNativeId(p.id) || !isNativeId(p.element_id, true))
      throw new Error("Media message and element IDs required");
    const context = this.sessionContext();
    const forwarded =
      Array.isArray(p.forward_path) && p.forward_path.length > 0;
    const raws = forwarded
      ? await this.forwardMessages(peer, p.forward_path)
      : checked(await context.msg.getMsgsByMsgId(peer, [String(p.id)]))
          .msgList || [];
    context.check();
    const raw = raws.find((raw) => String(raw.msgId) === p.id);
    const e = raw?.elements?.find(
      (e) => String(e.elementId) === String(p.element_id),
    );
    if (!e || (!forwarded && !samePeer(raw, peer)))
      throw new Error("Attachment unavailable");
    if (e.marketFaceElement)
      return {
        path: await this.resources.marketFace(
          String(e.marketFaceElement.emojiId || ""),
        ),
      };
    const v = e.picElement ?? e.fileElement ?? e.videoElement ?? e.pttElement;
    const source = v?.sourcePath || v?.filePath || "";
    if (source && fs.existsSync(source)) return { path: source };
    let resourceError;
    if (
      forwarded &&
      e.picElement &&
      (/^https?:\/\//i.test(v.originImageUrl || "") ||
        /^[a-fA-F0-9]{32}$/.test(v.md5HexStr || ""))
    ) {
      try {
        const local = await this.resources.image(v);
        context.check();
        return { path: local };
      } catch (error) {
        context.check();
        resourceError = error;
        // A stale URL or a missing legacy CDN copy must not bypass QQ's downloader.
      }
    }
    if (source && path.isAbsolute(source)) {
      await fs.promises.mkdir(path.dirname(source), { recursive: true, mode: 0o700 });
      context.check();
    }
    // QQ routes forwarded media through the outer card, not the inner record.
    const downloadId = forwarded ? p.forward_path[0] : p.id;
    const key = `${downloadId}:${p.element_id}`;
    const identity = JSON.stringify([peer, p.forward_path || [], p.id]);
    let request = this.mediaWaiters.get(key);
    if (request && request.identity !== identity)
      throw new Error("Media identity is busy in another record");
    if (!request) {
      if (this.mediaWaiters.size >= 8)
        throw new Error("Too many active QQ media downloads");
      let resolve, reject;
      const promise = new Promise((ok, fail) => {
        resolve = ok;
        reject = fail;
      });
      let timer;
      request = {
        promise,
        identity,
        finish: (error, value) => {
          clearTimeout(timer);
          if (this.mediaWaiters.get(key) === request)
            this.mediaWaiters.delete(key);
          error ? reject(error) : resolve(value);
        },
      };
      this.mediaWaiters.set(key, request);
      timer = setTimeout(
        () => request.finish(new Error("Media download timed out")),
        e.picElement ? 90000 : 120000,
      );
      try {
        const submitted = context.msg.downloadRichMedia({
          fileModelId: "0",
          downSourceType: 0,
          downloadScene: 0,
          triggerType: 1,
          msgId: String(downloadId),
          chatType: peer.chatType,
          peerUid: peer.peerUid,
          elementId: String(p.element_id),
          thumbSize: 0,
          downloadType: 1,
          filePath: source,
        });
        Promise.resolve(submitted)
          .then(checked)
          .catch((error) => request.finish(error));
      } catch (error) {
        request.finish(error);
      }
    }
    try {
      const downloaded = await request.promise;
      context.check();
      return { path: downloaded.filePath };
    } catch (nativeError) {
      context.check();
      if (!e.picElement) throw nativeError;
      try {
        if (resourceError) throw resourceError;
        const local = await this.resources.image(v);
        context.check();
        return { path: local };
      } catch (resourceError) {
        context.check();
        const failure = `QQ 原生下载：${cleanText(nativeError.message)}；QQ 资源地址：${cleanText(resourceError.message)}`;
        const thumbnails =
          v.thumbPath instanceof Map
            ? [...v.thumbPath.values()]
            : Object.values(v.thumbPath || {});
        const cached = thumbnails
          .filter(
            (file) => typeof file === "string" && file && fs.existsSync(file),
          )
          .map((file) => ({ file, size: fs.statSync(file).size }))
          .sort((a, b) => b.size - a.size)[0];
        if (cached)
          return {
            path: cached.file,
            warning: `原图不可用，仅显示 QQ 缓存的缩略图。${failure}`,
          };
        throw new Error(
          `原图不可用。${failure}。可按 r 重试，或请发送者重新发送。`,
        );
      }
    }
  }
}
module.exports = { NativeBridge, nativePeer, message, cleanText };
