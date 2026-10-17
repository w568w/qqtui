"use strict";
const { test } = require("node:test");
const assert = require("node:assert/strict");
const fs = require("node:fs");
const os = require("node:os");
const path = require("node:path");
const { Journal } = require("../bridge/journal.cjs");
const { permission } = require("../bridge/presentation.cjs");
test("journal migrates once, preserves deduplication and never downgrades confirmation", async () => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "qqtui-journal-"));
  const file = path.join(dir, "send.json");
  const record = {
    account: "123",
    request_id: "old",
    native_id: "99",
    peer: { chat_type: 1, uid: "u" },
    state: "submitted",
  };
  fs.writeFileSync(file, JSON.stringify([["123:old", record]]));
  let journal = new Journal(file);
  try {
    assert.equal((await journal.get("123:old")).state, "unknown");
    assert.equal(
      fs.readFileSync(file, "utf8"),
      fs.readFileSync(file + ".before-sqlite", "utf8"),
    );
    await journal.put("123:old", { ...record, state: "confirmed" });
    await journal.put("123:old", { ...record, state: "submitted" });
    assert.equal((await journal.byNative("123", "99")).state, "confirmed");
    await journal.close();
    journal = new Journal(file);
    assert.equal((await journal.get("123:old")).state, "confirmed");
    assert.equal(await journal.get("other:old"), null);
  } finally {
    await journal.close();
    fs.rmSync(dir, { recursive: true, force: true });
  }
});
test("invalid legacy journals fail closed without altering the source", async () => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "qqtui-journal-invalid-"));
  const file = path.join(dir, "send.json");
  fs.writeFileSync(file, "broken");
  const journal = new Journal(file);
  try {
    await assert.rejects(journal.get("key"));
    assert.equal(fs.readFileSync(file, "utf8"), "broken");
  } finally {
    if (journal.worker) await journal.worker.terminate();
    fs.rmSync(dir, { recursive: true, force: true });
  }
});
test("native permission distinguishes all-member mute, personal mute, privileged roles and unknown identity", () => {
  assert.equal(
    permission({ memberRole: 2 }, { shutUpAllTimestamp: 1 }).scope,
    "all",
  );
  assert.equal(
    permission({ memberRole: 3 }, { shutUpAllTimestamp: 1 }).state,
    "allowed",
  );
  assert.equal(
    permission({ memberRole: 4 }, { shutUpAllTimestamp: 1 }).state,
    "allowed",
  );
  const until = Math.floor(Date.now() / 1000) + 30;
  assert.deepEqual(
    permission({ memberRole: 2 }, { shutUpMeTimestamp: until }),
    {
      state: "blocked",
      scope: "self",
      reason: "你已被禁言",
      until: String(until),
    },
  );
  assert.equal(
    permission({ memberRole: 2 }, { shutUpMeTimestamp: 1 }).state,
    "allowed",
  );
  assert.equal(permission({}, {}).state, "unknown");
});

test("only changes to the current user's membership invalidate speech permission", () => {
  const { Presentation } = require("../bridge/presentation.cjs");
  const events = [];
  const adapter = new Presentation({
    selfUid: "self",
    event: (kind) => events.push(kind),
  });
  adapter.member("group", new Map([["other", { role: 2, shutUpTime: 0 }]]));
  assert.equal(events.length, 0);
  adapter.member("group", new Map([["self", { role: 2, shutUpTime: 0 }]]));
  assert.equal(events.length, 1);
  adapter.member("group", new Map([["self", { role: 2, shutUpTime: 0 }]]));
  assert.equal(events.length, 1);
  adapter.member("group", new Map([["self", { role: 2, shutUpTime: 100 }]]));
  assert.equal(events.length, 2);
});
