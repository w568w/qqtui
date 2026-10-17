"use strict";
const fs = require("node:fs");
const fsp = fs.promises;
const path = require("node:path");
const crypto = require("node:crypto");

const QQ_IMAGE_ORIGIN = "https://gchat.qpic.cn";
class UntrustedResourceUrl extends Error {}
function trustedResourceUrl(value) {
  const parsed = new URL(value);
  if (parsed.protocol === "http:") parsed.protocol = "https:";
  if (
    parsed.protocol !== "https:" ||
    parsed.username ||
    parsed.password ||
    parsed.port ||
    !["qq.com", "qq.com.cn", "qpic.cn", "gtimg.cn"].some(
      (domain) =>
        parsed.hostname === domain || parsed.hostname.endsWith("." + domain),
    )
  )
    throw new UntrustedResourceUrl("Untrusted QQ resource URL");
  return parsed;
}
function faceName(value, fallback = "") {
  let name = String(value || fallback).replace(
    /[\x00-\x08\x0b-\x1f\x7f-\x9f]/g,
    "",
  );
  if (name.startsWith("[") && name.endsWith("]")) name = name.slice(1, -1);
  if (name.startsWith("/")) name = name.slice(1);
  return name;
}
// QQ's resource layout is an adapter detail. Callers supply Electron's userData and app version.
class QqResources {
  constructor(options = {}) {
    this.emojiDir =
      options.emojiDir ||
      (options.userData
        ? path.join(
            options.userData,
            "versions",
            options.qqVersion,
            "resource",
            "default-emojis",
          )
        : "");
    this.emojiResources =
      options.emojiResources ||
      (options.userData
        ? ["global", path.join("nt_qq", "global")].map((root) =>
            path.join(
              options.userData,
              root,
              "nt_data",
              "Emoji",
              "emoji-resource",
              "sysface_res",
            ),
          )
        : []);
    const cache = options.cacheDir || options.stateDir;
    this.marketFaceDir = cache ? path.join(cache, "market-faces") : "";
    this.stickerDir = cache ? path.join(cache, "stickers") : "";
    this.imageDir = cache ? path.join(cache, "forwarded-images") : "";
    this.legacyMarketFaceDir =
      options.stateDir && options.stateDir !== cache
        ? path.join(options.stateDir, "market-faces")
        : "";
    this.downloads = new Map();
    this.faces = new Map();
    this.reload();
  }
  reload() {
    this.faces.clear();
    this.errors = [];
    const configs = [
      ...this.emojiResources.map((dir) =>
        path.join(dir, "..", "face_config.json"),
      ),
      ...(this.emojiDir
        ? [path.join(this.emojiDir, "default_config.json")]
        : []),
    ];
    for (const file of configs) {
      if (!fs.existsSync(file)) continue;
      try {
        const config = JSON.parse(fs.readFileSync(file, "utf8"));
        const rows =
          config.sysface ||
          Object.values(config).flatMap((panel) =>
            (panel.SysEmojiGroupList || []).flatMap(
              (group) => group.SysEmojiList || [],
            ),
          );
        for (const row of rows) {
          const id = String(row.QSid ?? row.emojiId ?? "");
          if (!/^[0-9]{1,5}$/.test(id) || this.faces.has(id)) continue;
          const stickerType =
            row.AniStickerType ??
            (row.aniStickerPackId ? row.emojiType : undefined);
          this.faces.set(id, {
            id,
            name: faceName(row.QDes || row.describe, `表情 ${id}`),
            native: {
              faceIndex: Number(id),
              faceType: stickerType ? 3 : Number(id) >= 222 ? 2 : 1,
              faceText: row.QDes || row.describe,
              stickerId: row.AniStickerId ?? row.aniStickerId,
              stickerType,
              packId: row.AniStickerPackId ?? row.aniStickerPackId,
              sourceType: 1,
            },
          });
        }
      } catch (error) {
        this.errors.push(`QQ emoji metadata: ${file}: ${error.message}`);
      }
    }
  }
  facePath(id) {
    if (!/^[0-9]{1,5}$/.test(id)) return "";
    const files = [
      ...this.emojiResources.flatMap((root) => [
        path.join(root, "apng", `s${id}.png`),
        path.join(root, "static", `s${id}.png`),
      ]),
      ...(this.emojiDir ? [path.join(this.emojiDir, `${id}.png`)] : []),
    ];
    return files.find((file) => fs.existsSync(file)) || "";
  }
  reactionLabel(id, type) {
    if (type === "2" && /^[0-9]+$/.test(id) && Number(id) <= 0x10ffff)
      return String.fromCodePoint(Number(id));
    return this.faces.get(id)?.name || `表情 ${id}`;
  }
  async download(key, url, target) {
    if (fs.existsSync(target)) return target;
    if (this.downloads.has(key)) return this.downloads.get(key);
    if (this.downloads.size >= 8) throw new Error("Too many emoji downloads");
    const promise = (async () => {
      const signal = AbortSignal.timeout(15000);
      let response;
      for (let hop = 0; hop < 4; hop++) {
        const parsed = trustedResourceUrl(url);
        response = await fetch(parsed.href, { signal, redirect: "manual" });
        if (![301, 302, 303, 307, 308].includes(response.status)) break;
        const location = response.headers.get("location");
        await response.body?.cancel();
        if (!location) throw new Error("QQ emoji redirect has no location");
        url = new URL(location, parsed).href;
      }
      if (!response.ok || !response.body)
        throw new Error(`QQ resource download: HTTP ${response.status}`);
      const chunks = [];
      let size = 0;
      for await (const chunk of response.body) {
        size += chunk.length;
        if (size > 16 * 1024 * 1024) throw new Error("QQ image exceeds 16 MiB");
        chunks.push(chunk);
      }
      const bytes = Buffer.concat(chunks);
      if (
        !bytes.subarray(0, 6).toString().startsWith("GIF8") &&
        !bytes
          .subarray(0, 8)
          .equals(Buffer.from([137, 80, 78, 71, 13, 10, 26, 10])) &&
        !(bytes.length >= 2 && bytes.readUInt16BE(0) === 0xffd8) &&
        bytes.subarray(0, 4).toString() !== "RIFF"
      )
        throw new Error("QQ resource response is not an image");
      await fsp.mkdir(path.dirname(target), { recursive: true, mode: 0o700 });
      const temporary = target + "." + crypto.randomUUID();
      try {
        await fsp.writeFile(temporary, bytes, { mode: 0o600, flag: "wx" });
        await fsp.rename(temporary, target);
      } finally {
        await fsp.unlink(temporary).catch(() => {});
      }
      return target;
    })();
    this.downloads.set(key, promise);
    try {
      return await promise;
    } finally {
      this.downloads.delete(key);
    }
  }
  marketFacePath(id) {
    if (!/^[a-fA-F0-9]{32}$/.test(id)) return "";
    return (
      [this.marketFaceDir, this.legacyMarketFaceDir]
        .filter(Boolean)
        .map((dir) => path.join(dir, `${id}.gif`))
        .find((file) => fs.existsSync(file)) || ""
    );
  }
  async marketFace(id) {
    if (!/^[a-fA-F0-9]{32}$/.test(id))
      throw new Error("Invalid market emoji ID");
    const existing = this.marketFacePath(id);
    if (existing) return existing;
    if (!this.marketFaceDir)
      throw new Error("Emoji cache directory is not configured");
    return this.download(
      "market:" + id,
      `https://gxh.vip.qq.com/club/item/parcel/item/${id.slice(0, 2)}/${id}/raw300.gif`,
      path.join(this.marketFaceDir, `${id}.gif`),
    );
  }
  imageTarget(entry) {
    if (!this.imageDir) return "";
    const md5 = String(entry.md5HexStr || "");
    const key = /^[a-fA-F0-9]{32}$/.test(md5)
      ? md5.toLowerCase()
      : crypto
          .createHash("sha256")
          .update(String(entry.originImageUrl || ""))
          .digest("hex");
    return path.join(this.imageDir, key);
  }
  imagePath(entry) {
    const target = this.imageTarget(entry);
    return target && fs.existsSync(target) ? target : "";
  }
  async image(entry) {
    const target = this.imageTarget(entry);
    const md5 = String(entry.md5HexStr || "");
    const legacy = /^[a-fA-F0-9]{32}$/.test(md5)
      ? `${QQ_IMAGE_ORIGIN}/gchatpic_new/0/0-0-${md5.toUpperCase()}/0`
      : "";
    const supplied = /^https?:\/\//i.test(entry.originImageUrl || "")
      ? trustedResourceUrl(entry.originImageUrl).href
      : "";
    if (!target || !(supplied || legacy))
      throw new Error("QQ has not supplied a downloadable image reference");
    let file;
    try {
      file = await this.download("image:" + target, supplied || legacy, target);
    } catch (error) {
      if (
        error instanceof UntrustedResourceUrl ||
        !supplied ||
        !legacy ||
        supplied === legacy
      )
        throw error;
      file = await this.download("image:" + target, legacy, target);
    }
    return file;
  }
  favoritePath(entry) {
    for (let file of [entry.emoOriginalPath, entry.emoPath]) {
      if (typeof file !== "string" || !file) continue;
      try {
        if (file.startsWith("file:"))
          file = require("node:url").fileURLToPath(file);
        if (path.isAbsolute(file) && fs.statSync(file).isFile()) return file;
      } catch (_) {}
    }
    return "";
  }
  async favorite(entry) {
    const file = this.favoritePath(entry);
    if (file) return file;
    if (entry.isMarkFace && /^[a-fA-F0-9]{32}$/.test(entry.eId || ""))
      return this.marketFace(entry.eId);
    if (!entry.url) throw new Error("QQ favorite has no downloadable resource");
    const key = crypto
      .createHash("sha256")
      .update(String(entry.md5 || entry.resId || entry.url))
      .digest("hex");
    return this.download(
      "favorite:" + key,
      entry.url,
      path.join(this.stickerDir, key),
    );
  }
}
module.exports = { QqResources, faceName };
