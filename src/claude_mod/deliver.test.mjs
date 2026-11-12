import { test } from "node:test";
import assert from "node:assert/strict";
import { createServer } from "node:http";
import { createHash, randomUUID } from "node:crypto";

const sha = (text) => createHash("sha256").update(text).digest("hex");
const flush = () => new Promise((r) => setImmediate(r));

async function readBody(req) {
  const chunks = [];
  let size = 0;
  for await (const chunk of req) { size += chunk.length; if (size <= 65536) chunks.push(chunk); }
  if (size > 65536) return { tooLarge: true };
  try { return { value: JSON.parse(Buffer.concat(chunks).toString("utf8")) }; } catch { return { bad: true }; }
}

async function createContractInbox(token, { registerConflict = false } = {}) {
  let receiver = null;
  const queue = [], claimed = new Map(), receipts = [];
  const calls = { register: 0, next: 0, receipt: 0 };
  const server = createServer(async (req, res) => {
    const send = (status, body) => { res.writeHead(status, { "Content-Type": "application/json" }); res.end(JSON.stringify(body)); };
    try {
      if (req.headers.origin) return send(403, { error: "Origin not allowed" });
      if (req.headers.authorization !== `Bearer ${token}`) return send(401, { error: "Unauthorized" });
      const url = new URL(req.url, "http://127.0.0.1");
      const body = await readBody(req);
      if (body.tooLarge || body.bad) return send(400, { error: "bad" });
      const data = body.value ?? null;
      if (req.method === "POST" && url.pathname === "/register") {
        calls.register += 1;
        if (registerConflict || receiver) return send(409, { error: "receiver exists" });
        receiver = randomUUID();
        return send(200, { receiver });
      }
      if (req.method === "POST" && url.pathname === "/next") {
        calls.next += 1;
        if (!data || data.receiver !== receiver) return send(403, { error: "wrong receiver" });
        if (queue.length === 0) return send(200, null);
        const msg = queue.shift();
        claimed.set(msg.id, msg);
        return send(200, { id: msg.id, text: msg.text, sha256: msg.sha256 });
      }
      if (req.method === "POST" && url.pathname === "/receipt") {
        calls.receipt += 1;
        const msg = claimed.get(data?.id);
        if (!data || data.receiver !== receiver || !msg || data.sha256 !== msg.sha256 || !["started", "held"].includes(data.state)) {
          return send(409, { error: "id/sha/state mismatch or an unclaimed id" });
        }
        claimed.delete(data.id);
        receipts.push({ ...data });
        return send(200, {});
      }
      return send(404, { error: "not found" });
    } catch { return send(500, { error: "error" }); }
  });
  await new Promise((ok) => server.listen(0, "127.0.0.1", ok));
  return {
    url: `http://127.0.0.1:${server.address().port}/`, token, calls, receipts,
    enqueue: (id, text) => { queue.push({ id, text, sha256: sha(text) }); },
    close: () => new Promise((ok) => server.close(ok)),
  };
}

function makeHost({ inbox, fileContent, fileExists = true, sessionId = "session-1", interactive = true, submitImpl, fetchImpl }) {
  const timers = [], statuses = [], submits = [], fetchCalls = [];
  return {
    host: {
      fs: { exists: async () => fileExists, read: async () => fileContent ?? JSON.stringify({ url: inbox.url, token: inbox.token }) },
      session: { id: async () => sessionId },
      http: { fetch: async (url, init) => { fetchCalls.push(url); if (fetchImpl) return fetchImpl(url, init, fetchCalls.length); const r = await fetch(url, init); return { ok: r.ok, status: r.status, text: await r.text() }; } },
      clock: { every: (ms, cb) => { timers.push({ ms, cb }); } },
      ui: { status: (t) => { statuses.push(t); } },
      prompt: { submit: async (input) => { submits.push(input); if (submitImpl) return submitImpl(input, submits.length); return {}; } },
    },
    timers, statuses, submits, fetchCalls, event: { cwd: "/work", isInteractive: interactive },
  };
}

async function startMod(opts) {
  const mod = await import(`./hooks/deliver.mjs?i=${randomUUID()}`);
  const hooks = new Map();
  const ctx = makeHost(opts);
  mod.register((event, fn) => hooks.set(event, fn));
  await hooks.get("session.start")(ctx.host, ctx.event, async (e) => e);
  return { ...ctx, hooks };
}

test("happy path", async (t) => {
  const inbox = await createContractInbox("a".repeat(64));
  t.after(() => inbox.close());
  inbox.enqueue("m1", "hello");
  const ctx = await startMod({ inbox });
  assert.equal(ctx.timers.length, 1); assert.equal(ctx.timers[0].ms, 500);
  assert.ok(ctx.statuses.includes("Crowded inbox connected"));
  await ctx.timers[0].cb();
  assert.deepEqual(ctx.submits, [{ text: "hello" }]);
  assert.equal(inbox.receipts.length, 1);
  assert.equal(inbox.receipts[0].state, "started");
  assert.equal(inbox.receipts[0].reason, "");
  assert.equal(inbox.receipts[0].sha256, sha("hello"));
});

test("byte-exact text matrix", async (t) => {
  const texts = ["plain", "quotes \"' \\\\", "line\n\r\n", "café 日本語 😀"];
  for (const text of texts) {
    const inbox = await createContractInbox("b".repeat(64));
    t.after(() => inbox.close());
    inbox.enqueue("m", text);
    const ctx = await startMod({ inbox });
    await ctx.timers[0].cb();
    assert.deepEqual(ctx.submits, [{ text }]);
    assert.equal(inbox.receipts[0].sha256, sha(text));
  }
});

test("FIFO and no overlapping poll", async (t) => {
  const inbox = await createContractInbox("c".repeat(64));
  t.after(() => inbox.close());
  inbox.enqueue("m1", "one"); inbox.enqueue("m2", "two");
  let resolveSubmit;
  const ctx = await startMod({ inbox, submitImpl: (_i, n) => n === 1 ? new Promise((ok) => { resolveSubmit = ok; }) : {} });
  const p1 = ctx.timers[0].cb();
  for (let i = 0; i < 50 && inbox.calls.next === 0; i++) await flush();
  assert.equal(inbox.calls.next, 1);
  await ctx.timers[0].cb(); await ctx.timers[0].cb(); await flush();
  assert.equal(inbox.calls.next, 1); assert.equal(ctx.submits.length, 1);
  resolveSubmit({});
  await p1;
  await ctx.timers[0].cb();
  for (let i = 0; i < 50 && ctx.submits.length < 2; i++) await flush();
  assert.deepEqual(ctx.submits.map((s) => s.text), ["one", "two"]);
});

test("drop -> held and stop and no retry", async (t) => {
  const inbox = await createContractInbox("d".repeat(64));
  t.after(() => inbox.close());
  inbox.enqueue("m1", "dropme");
  const ctx = await startMod({ inbox, submitImpl: () => ({ drop: "nope" }) });
  await ctx.timers[0].cb();
  assert.equal(ctx.submits.length, 1);
  assert.equal(inbox.receipts[0].state, "held");
  assert.equal(inbox.receipts[0].reason, "nope");
  assert.ok(ctx.statuses.some((s) => /held/i.test(s)));
  inbox.enqueue("m2", "later");
  await ctx.timers[0].cb();
  assert.equal(ctx.submits.length, 1);
});

test("submit throws -> uncertain, no resubmit", async (t) => {
  const inbox = await createContractInbox("e".repeat(64));
  t.after(() => inbox.close());
  inbox.enqueue("m1", "boom");
  const ctx = await startMod({ inbox, submitImpl: () => { throw new Error("boom"); } });
  await ctx.timers[0].cb();
  assert.equal(ctx.submits.length, 1);
  assert.equal(inbox.receipts.length, 0);
  assert.ok(ctx.statuses.some((s) => /uncertain/i.test(s)));
  inbox.enqueue("m2", "later");
  await ctx.timers[0].cb();
  assert.equal(ctx.submits.length, 1);
});

test("409 on register -> inert", async (t) => {
  const inbox = await createContractInbox("f".repeat(64), { registerConflict: true });
  t.after(() => inbox.close());
  const ctx = await startMod({ inbox });
  assert.equal(ctx.timers.length, 0);
  assert.equal(inbox.calls.register, 1);
  assert.ok(ctx.statuses.includes("Crowded inbox not connected"));
});

test("missing or invalid file and wrong-shape url/token -> inert", async (t) => {
  const token = "0".repeat(64);
  const inbox = await createContractInbox(token);
  t.after(() => inbox.close());
  const cases = [
    { fileExists: false }, { fileContent: "not json" },
    { fileContent: JSON.stringify({ url: "https://127.0.0.1:1/", token }) },
    { fileContent: JSON.stringify({ url: "http://example.com/", token }) },
    { fileContent: JSON.stringify({ url: "http://example.com:1234/", token }) },
    { fileContent: JSON.stringify({ url: "http://127.0.0.2:1234/", token }) },
    { fileContent: JSON.stringify({ url: "http://localhost:1234/", token }) },
    { fileContent: JSON.stringify({ url: "http://127.0.0.1/", token }) },
    { fileContent: JSON.stringify({ url: "http://user:pass@127.0.0.1:1/", token }) },
    { fileContent: JSON.stringify({ url: "http://127.0.0.1:1/path", token }) },
    { fileContent: JSON.stringify({ url: "http://127.0.0.1:1/?q=1", token }) },
    { fileContent: JSON.stringify({ url: "http://127.0.0.1:1/#h", token }) },
    { fileContent: JSON.stringify({ url: inbox.url, token: "ABC" }) },
    { fileContent: JSON.stringify({ url: inbox.url, token: "g".repeat(63) }) },
  ];
  for (const c of cases) {
    const before = inbox.calls.register;
    const ctx = await startMod({ inbox, ...c });
    assert.equal(ctx.timers.length, 0, JSON.stringify(c));
    assert.ok(ctx.statuses.includes("Crowded inbox not connected"), JSON.stringify(c));
    assert.equal(inbox.calls.register, before, JSON.stringify(c));
    assert.equal(ctx.fetchCalls.length, 0, JSON.stringify(c));
  }
});

test("non-interactive does nothing", async (t) => {
  const inbox = await createContractInbox("1".repeat(64));
  t.after(() => inbox.close());
  const ctx = await startMod({ inbox, interactive: false });
  assert.equal(ctx.timers.length, 0);
  assert.equal(inbox.calls.register, 0);
});

test("/clear case: delivery continues with same receiver", async (t) => {
  const inbox = await createContractInbox("2".repeat(64));
  t.after(() => inbox.close());
  const mod = await import(`./hooks/deliver.mjs?i=${randomUUID()}`);
  const hooks = new Map();
  const ctx = makeHost({ inbox });
  mod.register((e, fn) => hooks.set(e, fn));
  await hooks.get("session.start")(ctx.host, ctx.event, async (e) => e);
  assert.equal(inbox.calls.register, 1);
  inbox.enqueue("m1", "first"); await ctx.timers[0].cb();
  inbox.enqueue("m2", "second"); await ctx.timers[0].cb();
  assert.deepEqual(ctx.submits.map((s) => s.text), ["first", "second"]);
  assert.equal(inbox.calls.register, 1);
  assert.equal(ctx.timers.length, 1);
});

test("R1 bad /next shape -> uncertain, no submit, stopped", async (t) => {
  const token = "3".repeat(64);
  const bads = [{}, { id: "1", text: 1, sha256: "x" }, { id: "1", text: "hi" }];
  for (const bad of bads) {
    const inbox = await createContractInbox(token);
    t.after(() => inbox.close());
    const fetchImpl = async (url, init) => {
      const u = new URL(url);
      if (u.pathname === "/next") return { ok: true, status: 200, text: JSON.stringify(bad) };
      const r = await fetch(url, init);
      return { ok: r.ok, status: r.status, text: await r.text() };
    };
    const ctx = await startMod({ inbox, fetchImpl });
    await ctx.timers[0].cb();
    assert.equal(ctx.submits.length, 0, JSON.stringify(bad));
    assert.ok(ctx.statuses.some((s) => /uncertain/i.test(s)), JSON.stringify(bad));
    await ctx.timers[0].cb();
    assert.equal(ctx.submits.length, 0, JSON.stringify(bad));
  }
});

test("R1 missing receiver from register -> inert/uncertain, no submit", async (t) => {
  const token = "4".repeat(64);
  const inbox = await createContractInbox(token);
  t.after(() => inbox.close());
  const fetchImpl = async (url, init) => {
    const u = new URL(url);
    if (u.pathname === "/register") return { ok: true, status: 200, text: JSON.stringify({}) };
    const r = await fetch(url, init);
    return { ok: r.ok, status: r.status, text: await r.text() };
  };
  const ctx = await startMod({ inbox, fetchImpl });
  assert.equal(ctx.timers.length, 0);
  assert.ok(ctx.statuses.includes("Crowded inbox not connected"));
});

test("R2 second session.start is once-only", async (t) => {
  const inbox = await createContractInbox("5".repeat(64));
  t.after(() => inbox.close());
  const mod = await import(`./hooks/deliver.mjs?i=${randomUUID()}`);
  const hooks = new Map();
  const ctx = makeHost({ inbox });
  mod.register((e, fn) => hooks.set(e, fn));
  await hooks.get("session.start")(ctx.host, ctx.event, async (e) => e);
  await hooks.get("session.start")(ctx.host, ctx.event, async (e) => e);
  assert.equal(inbox.calls.register, 1);
  assert.equal(ctx.timers.length, 1);
});

test("R4 oversize /next response refused", async (t) => {
  const inbox = await createContractInbox("6".repeat(64));
  t.after(() => inbox.close());
  const big = "x".repeat(70000);
  const fetchImpl = async (url, init) => {
    const u = new URL(url);
    if (u.pathname === "/next") return { ok: true, status: 200, text: JSON.stringify({ id: "m", text: big, sha256: "h" }) };
    const r = await fetch(url, init);
    return { ok: r.ok, status: r.status, text: await r.text() };
  };
  const ctx = await startMod({ inbox, fetchImpl });
  await ctx.timers[0].cb();
  assert.equal(ctx.submits.length, 0);
  assert.ok(ctx.statuses.some((s) => /uncertain/i.test(s)));
  await ctx.timers[0].cb();
  assert.equal(ctx.submits.length, 0);
});

test("R5 receipt failure after submit and id/sha echoed", async (t) => {
  const inbox = await createContractInbox("7".repeat(64));
  t.after(() => inbox.close());
  inbox.enqueue("m1", "hi");
  const fetchImpl = async (url, init) => {
    const u = new URL(url);
    if (u.pathname === "/receipt") return { ok: false, status: 500, text: JSON.stringify({ error: "boom" }) };
    const r = await fetch(url, init);
    return { ok: r.ok, status: r.status, text: await r.text() };
  };
  const ctx = await startMod({ inbox, fetchImpl });
  await ctx.timers[0].cb();
  assert.equal(ctx.submits.length, 1);
  assert.ok(ctx.statuses.some((s) => /uncertain/i.test(s)));
  inbox.enqueue("m2", "later");
  await ctx.timers[0].cb();
  assert.equal(ctx.submits.length, 1);
});

test("R5 receipt body echoes id and sha256", async (t) => {
  const inbox = await createContractInbox("8".repeat(64));
  t.after(() => inbox.close());
  inbox.enqueue("m1", "echo");
  const ctx = await startMod({ inbox });
  await ctx.timers[0].cb();
  assert.equal(inbox.receipts[0].id, "m1");
  assert.equal(inbox.receipts[0].sha256, sha("echo"));
});
