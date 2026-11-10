// Behavioural tests for the generated crowded-tui OpenCode plugin.
// Run with the generated file copied to a .mjs path:
//
//   cp <generated-tui.js> /tmp/crowded-tui.mjs
//   TUI_UNDER_TEST=/tmp/crowded-tui.mjs node --test tests/tui_session_report.mjs
//
// The Rust suite (src/toolbox.rs) writes the file with the toolbox generator
// and runs exactly this, skipping when node is unavailable.
//
// The plugin runs against a fake host: a fake clock (the plugin's own
// timeouts and the 300 ms route poll advance only when a test says so), a
// fake `Bun.spawn` whose children finish, ignore signals or never exit under
// the test's control, and a fake TUI context with a controllable
// session.create, navigate and event emitter.
import test from "node:test";
import assert from "node:assert/strict";
import { chmodSync, existsSync, mkdtempSync, readFileSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { spawn as nodeSpawn } from "node:child_process";

const tuiPath = process.env.TUI_UNDER_TEST;
if (!tuiPath) {
  throw new Error("set TUI_UNDER_TEST to the generated tui file (.mjs)");
}
const { default: plugin } = await import(tuiPath);

const realSetTimeout = globalThis.setTimeout;
const flush = () => new Promise((resolve) => setImmediate(resolve));

function fakeClock() {
  let now = 0;
  let seq = 0;
  const timers = new Map();
  const add = (fn, ms, every) => {
    seq += 1;
    timers.set(seq, { at: now + ms, fn, every });
    return seq;
  };
  return {
    setTimeout: (fn, ms) => add(fn, ms, null),
    setInterval: (fn, ms) => add(fn, ms, ms),
    clear: (id) => timers.delete(id),
    get size() {
      return timers.size;
    },
    async tick(ms) {
      const end = now + ms;
      for (;;) {
        await flush();
        const due = [...timers.entries()]
          .filter(([, timer]) => timer.at <= end)
          .sort((a, b) => a[1].at - b[1].at || a[0] - b[0])[0];
        if (!due) {
          break;
        }
        const [id, timer] = due;
        now = timer.at;
        if (timer.every) {
          timer.at += timer.every;
        } else {
          timers.delete(id);
        }
        timer.fn();
      }
      now = end;
      await flush();
    },
  };
}

// Starts the plugin against the fake host. Options:
//   autocreate   false sets CROWDED_TUI_AUTOCREATE=0
//   ignore       signals a fake child ignores (default: none, it dies on any)
//   immortal     fake children never die
//   killExit     exit code of a child that a signal ends (default 143)
//   navigate     "sync" (default) | "throw" | "async" | "reject"
//   createThrows session.create throws synchronously
//   noBin        no CROWDED_BIN in the environment
async function start(t, options = {}) {
  const clock = fakeClock();
  const saved = {
    setTimeout: globalThis.setTimeout,
    setInterval: globalThis.setInterval,
    clearTimeout: globalThis.clearTimeout,
    clearInterval: globalThis.clearInterval,
    Bun: globalThis.Bun,
    env: { ...process.env },
    write: process.stdout.write,
    errWrite: process.stderr.write,
  };
  const stdout = [];
  const stderr = [];
  const host = { clock, spawns: [], delivered: [], creates: [], navigations: [], subs: {}, unsubscribed: 0, stdout, stderr };
  globalThis.setTimeout = clock.setTimeout;
  globalThis.setInterval = clock.setInterval;
  globalThis.clearTimeout = clock.clear;
  globalThis.clearInterval = clock.clear;
  process.env.CROWDED_TOKEN = "secret-token-value";
  process.env.CROWDED_ROOM = "7";
  if (options.noBin) {
    delete process.env.CROWDED_BIN;
  } else {
    process.env.CROWDED_BIN = "/fake/crowded";
  }
  if (options.autocreate === false) {
    process.env.CROWDED_TUI_AUTOCREATE = "0";
  } else {
    delete process.env.CROWDED_TUI_AUTOCREATE;
  }
  // The test runner itself reports over stdout, so output is forwarded and only
  // writes that originate in the plugin file are recorded.
  const spy = (original) =>
    function spied(...args) {
      if (new Error().stack.includes(tuiPath)) {
        stdout.push(args.map(String).join(" "));
      }
      return original.apply(this, args);
    };
  process.stdout.write = spy(saved.write);
  process.stderr.write = (...args) => {
    if (new Error().stack.includes(tuiPath)) {
      stderr.push(args.map(String).join(" "));
      return true;
    }
    return saved.errWrite.apply(process.stderr, args);
  };
  saved.console = { log: console.log, info: console.info };
  for (const name of Object.keys(saved.console)) {
    console[name] = spy(saved.console[name]);
  }
  globalThis.Bun = {
    spawn(argv) {
      if (options.spawnThrows) {
        throw new Error("spawn failed");
      }
      const child = { argv, args: argv.slice(2), signals: [], dead: false, delivered: false };
      // `--ack` is checked on its own; the label is the report itself.
      child.acked = child.args.includes("--ack");
      child.label = child.args.filter((arg) => arg !== "--ack").join(" ");
      child.exited = new Promise((resolve) => {
        child.settle = resolve;
      });
      child.die = (code = 0) => {
        if (!child.dead) {
          child.dead = true;
          child.settle(code);
        }
      };
      child.finish = () => {
        if (!child.dead) {
          child.delivered = true;
          host.delivered.push(child.label);
          child.die(0);
        }
      };
      child.kill = (signal) => {
        child.signals.push(signal);
        if (options.killThrows) {
          throw new Error("kill failed");
        }
        if (!options.immortal && !(options.ignore || []).includes(signal)) {
          child.die(options.killExit ?? 143);
        }
      };
      host.spawns.push(child);
      return child;
    },
  };
  host.state = { route: { type: "home" } };
  const ctx = {
    ui: {
      router: {
        current: () => {
          if (options.currentThrows) {
            throw new Error("current failed");
          }
          return host.state.route;
        },
        navigate(dest) {
          host.navigations.push(dest.sessionID);
          if (options.navigate === "throw") {
            throw new Error("navigate failed");
          }
          if (options.navigate === "reject") {
            return Promise.reject(new Error("navigate failed"));
          }
          if (options.navigate === "async") {
            return new Promise((resolve) => {
              host.finishNavigate = () => {
                host.state.route = dest;
                resolve();
              };
            });
          }
          host.state.route = dest;
          return undefined;
        },
      },
    },
    data: {
      on(type, fn) {
        if (options.subscribeThrows) {
          throw new Error("on failed");
        }
        host.subs[type] = fn;
        return () => {
          delete host.subs[type];
          host.unsubscribed += 1;
        };
      },
    },
    client: {
      session: {
        create({ location }) {
          if (options.createThrows) {
            throw new Error("create failed");
          }
          return new Promise((resolve, reject) => {
            host.creates.push({ dir: location.directory, resolve: (id) => resolve({ id }), reject: () => reject(new Error("nope")), resolveRaw: resolve });
          });
        },
      },
    },
  };
  host.ctx = options.noUi ? {} : ctx;
  host.dispose = await plugin.setup(host.ctx);
  t.after(() => {
    if (typeof host.dispose === "function") {
      host.dispose();
    }
    globalThis.setTimeout = saved.setTimeout;
    globalThis.setInterval = saved.setInterval;
    globalThis.clearTimeout = saved.clearTimeout;
    globalThis.clearInterval = saved.clearInterval;
    globalThis.Bun = saved.Bun;
    process.stdout.write = saved.write;
    process.stderr.write = saved.errWrite;
    Object.assign(console, saved.console);
    for (const key of Object.keys(process.env)) {
      if (!(key in saved.env)) {
        delete process.env[key];
      }
    }
    Object.assign(process.env, saved.env);
  });
  host.go = async (dest) => {
    host.state.route = dest;
    await clock.tick(300);
  };
  host.session = (id) => host.go({ type: "session", sessionID: id });
  let homes = 0;
  host.home = (directory) => host.go({ type: "home", location: { directory: directory || `/proj/${homes++}` } });
  // Puts the route on a session without waiting for the poll, the way a
  // session.created event can arrive before the poll has seen the route.
  host.show = (id) => {
    host.state.route = { type: "session", sessionID: id };
  };
  host.emitCreated = async (id) => {
    host.subs["session.created"]({ data: { sessionID: id } });
    await flush();
  };
  host.starts = () => host.spawns.map((child) => child.label);
  host.finishAll = async () => {
    for (;;) {
      const open = host.spawns.find((child) => !child.dead);
      if (!open) {
        return;
      }
      open.finish();
      await flush();
    }
  };
  host.settle = async () => {
    await host.finishAll();
    await clock.tick(0);
    await host.finishAll();
  };
  return host;
}

// Reports `--session a` as delivered and a pending flip completed, ready for
// the next step.
async function boundTo(t, id, options) {
  const host = await start(t, options);
  await host.session(id);
  await host.settle();
  return host;
}

// ---- idle ----

test("idle: startup on home reports and creates nothing", async (t) => {
  const host = await start(t);
  await host.clock.tick(2000);
  assert.deepEqual(host.starts(), []);
  assert.deepEqual(host.creates, []);
});

test("idle to bound: a session route is reported once", async (t) => {
  const host = await start(t);
  await host.session("ses-a");
  await host.settle();
  assert.deepEqual(host.starts(), ["--session ses-a"]);
});

test("idle to bound: a session.created event for the displayed session reports its id", async (t) => {
  const host = await start(t);
  host.show("ses-a");
  await host.emitCreated("ses-a");
  await host.settle();
  assert.deepEqual(host.starts(), ["--session ses-a"]);
});

// ---- bound ----

test("bound: the same session again, by route or by event, is not reported twice", async (t) => {
  const host = await boundTo(t, "ses-a");
  await host.go({ type: "session", sessionID: "ses-a", extra: 1 });
  await host.emitCreated("ses-a");
  await host.settle();
  assert.deepEqual(host.starts(), ["--session ses-a"]);
});

test("bound: a session-to-session switch reports the new id", async (t) => {
  const host = await boundTo(t, "ses-a");
  await host.session("ses-b");
  await host.settle();
  assert.deepEqual(host.starts(), ["--session ses-a", "--session ses-b"]);
});

test("bound to pending: with auto-create off a home flip reports pending only, then the next session", async (t) => {
  const host = await boundTo(t, "ses-a", { autocreate: false });
  await host.home();
  await host.settle();
  await host.clock.tick(2000);
  assert.deepEqual(host.starts(), ["--session ses-a", "--pending"]);
  assert.deepEqual(host.creates, []);
  await host.session("ses-b");
  await host.settle();
  assert.deepEqual(host.starts(), ["--session ses-a", "--pending", "--session ses-b"]);
});

// ---- pending ----

test("pending: returning to the session already reported reports it again and clears pending", async (t) => {
  const host = await boundTo(t, "ses-a", { autocreate: false });
  await host.home();
  await host.settle();
  await host.session("ses-a");
  await host.settle();
  assert.deepEqual(host.starts(), ["--session ses-a", "--pending", "--session ses-a"]);
});

test("pending: a session.created event for the displayed session reports its id", async (t) => {
  const host = await boundTo(t, "ses-a", { autocreate: false });
  await host.home();
  await host.settle();
  host.show("ses-b");
  await host.emitCreated("ses-b");
  await host.settle();
  assert.deepEqual(host.starts(), ["--session ses-a", "--pending", "--session ses-b"]);
});

// ---- creating ----

test("bound to creating: the create starts only after the pending report has finished", async (t) => {
  const host = await boundTo(t, "ses-a");
  await host.home("/proj/x");
  assert.deepEqual(host.starts(), ["--session ses-a", "--pending"]);
  assert.equal(host.creates.length, 0, "the pending report is still running");
  host.spawns[1].finish();
  await flush();
  assert.equal(host.creates.length, 1);
  assert.equal(host.creates[0].dir, "/proj/x");
});

test("creating: the created session is navigated to and reported once, in that order", async (t) => {
  const host = await boundTo(t, "ses-a");
  await host.home();
  host.spawns[1].finish();
  await flush();
  host.creates[0].resolve("ses-new");
  await flush();
  assert.deepEqual(host.navigations, ["ses-new"]);
  assert.deepEqual(host.starts(), ["--session ses-a", "--pending", "--session ses-new"]);
  await host.emitCreated("ses-new");
  await host.clock.tick(600);
  await host.settle();
  assert.deepEqual(host.starts(), ["--session ses-a", "--pending", "--session ses-new"]);
  assert.deepEqual(host.state.route, { type: "session", sessionID: "ses-new" });
});

test("creating: session.created arriving before the create resolves is navigated to and reported once", async (t) => {
  const host = await boundTo(t, "ses-old");
  await host.home();
  host.spawns[1].finish();
  await flush();
  await host.emitCreated("auto");
  assert.deepEqual(host.navigations, [], "nothing is decided before the create resolves");
  host.creates[0].resolve("auto");
  await flush();
  await host.settle();
  assert.deepEqual(host.navigations, ["auto"]);
  assert.deepEqual(host.starts().filter((label) => label === "--session auto"), ["--session auto"]);
  assert.deepEqual(host.state.route, { type: "session", sessionID: "auto" });
});

test("creating: a displayed session that is not the created one is the human's choice and nothing is navigated", async (t) => {
  const host = await boundTo(t, "ses-old");
  await host.home();
  host.spawns[1].finish();
  await flush();
  host.show("human");
  await host.emitCreated("human");
  host.creates[0].resolve("auto");
  await flush();
  await host.settle();
  assert.deepEqual(host.navigations, []);
  assert.deepEqual(host.starts(), ["--session ses-old", "--pending", "--session human"]);
});

test("creating: a session the human reaches while the create runs wins and the result is discarded", async (t) => {
  const host = await boundTo(t, "ses-old");
  await host.home();
  host.spawns[1].finish();
  await flush();
  await host.session("human");
  host.creates[0].resolve("auto");
  await flush();
  await host.settle();
  assert.deepEqual(host.navigations, []);
  assert.deepEqual(host.starts(), ["--session ses-old", "--pending", "--session human"]);
  await host.emitCreated("auto");
  await host.settle();
  assert.deepEqual(host.starts(), ["--session ses-old", "--pending", "--session human"], "the unused session never rebinds the room");
});

test("creating: a session the human reaches before the create resolves, without a route event in between, wins", async (t) => {
  const host = await boundTo(t, "ses-old");
  await host.home();
  host.spawns[1].finish();
  await flush();
  host.state.route = { type: "session", sessionID: "human" };
  host.creates[0].resolve("auto");
  await flush();
  await host.settle();
  assert.deepEqual(host.navigations, []);
  assert.deepEqual(host.starts(), ["--session ses-old", "--pending", "--session human"]);
});

test("creating: a human session during the pending report cancels the create", async (t) => {
  const host = await boundTo(t, "ses-old");
  await host.home();
  await host.session("human");
  await host.settle();
  assert.deepEqual(host.creates, []);
  assert.deepEqual(host.starts(), ["--session ses-old", "--pending", "--session human"]);
});

test("creating: a failing create leaves the room pending without reporting", async (t) => {
  const host = await boundTo(t, "ses-old");
  await host.home();
  host.spawns[1].finish();
  await flush();
  host.creates[0].reject();
  await flush();
  await host.settle();
  assert.deepEqual(host.navigations, []);
  assert.deepEqual(host.starts(), ["--session ses-old", "--pending"]);
  await host.session("ses-next");
  await host.settle();
  assert.deepEqual(host.starts(), ["--session ses-old", "--pending", "--session ses-next"]);
});

test("creating: a failing create with a displayed human session in between reports that session", async (t) => {
  const host = await boundTo(t, "ses-old");
  await host.home();
  host.spawns[1].finish();
  await flush();
  host.show("human");
  await host.emitCreated("human");
  host.creates[0].reject();
  await flush();
  await host.settle();
  assert.deepEqual(host.starts(), ["--session ses-old", "--pending", "--session human"]);
});

test("creating: a navigation that throws is not reported, even by a late session.created", async (t) => {
  const host = await boundTo(t, "ses-old", { navigate: "throw" });
  await host.home();
  host.spawns[1].finish();
  await flush();
  host.creates[0].resolve("auto");
  await flush();
  await host.emitCreated("auto");
  await host.settle();
  assert.deepEqual(host.navigations, ["auto"]);
  assert.deepEqual(host.starts(), ["--session ses-old", "--pending"]);
});

test("creating: an asynchronous navigation is reported when it lands and not when it rejects", async (t) => {
  const landing = await boundTo(t, "ses-old", { navigate: "async" });
  await landing.home();
  landing.spawns[1].finish();
  await flush();
  landing.creates[0].resolve("auto");
  await flush();
  assert.deepEqual(landing.starts(), ["--session ses-old", "--pending"], "not reported before it lands");
  landing.finishNavigate();
  await landing.clock.tick(300);
  await landing.settle();
  assert.deepEqual(landing.starts().filter((label) => label === "--session auto"), ["--session auto"]);

  const refused = await boundTo(t, "ses-old", { navigate: "reject" });
  await refused.home();
  refused.spawns[1].finish();
  await flush();
  refused.creates[0].resolve("auto");
  await flush();
  await refused.settle();
  assert.deepEqual(refused.starts(), ["--session ses-old", "--pending"]);
});

test("creating: a home flip while a create is unresolved does not start a second create", async (t) => {
  const host = await boundTo(t, "ses-old");
  await host.home();
  host.spawns[1].finish();
  await flush();
  await host.session("human");
  await host.home();
  await host.settle();
  assert.equal(host.creates.length, 1);
});

test("opt-out: CROWDED_TUI_AUTOCREATE=0 never creates", async (t) => {
  const host = await boundTo(t, "ses-old", { autocreate: false });
  await host.home();
  await host.settle();
  await host.clock.tick(5000);
  assert.deepEqual(host.creates, []);
  assert.deepEqual(host.navigations, []);
});

test("creating: without CROWDED_BIN nothing is spawned and the create still runs", async (t) => {
  const host = await start(t, { noBin: true });
  await host.session("ses-old");
  await host.home();
  await flush();
  assert.deepEqual(host.spawns, []);
  assert.equal(host.creates.length, 1);
});

// ---- ids ----

test("ids: only a conservative id is navigated to or reported", async (t) => {
  const host = await boundTo(t, "ses-old");
  const bad = ["", "a b", "a/b", "x".repeat(129), "ünï", "-- --pending"];
  for (const id of bad) {
    await host.go({ type: "session", sessionID: id });
    await host.emitCreated(id);
  }
  host.subs["session.created"]({ data: { sessionID: 42 } });
  host.subs["session.created"]({});
  host.subs["session.created"](null);
  await host.settle();
  assert.deepEqual(host.starts(), ["--session ses-old"]);

  await host.home();
  host.spawns[1].finish();
  await flush();
  host.creates[0].resolve("bad id");
  await flush();
  await host.settle();
  assert.deepEqual(host.navigations, []);
  assert.deepEqual(host.starts(), ["--session ses-old", "--pending"]);

  const edge = await boundTo(t, "x".repeat(128));
  assert.deepEqual(edge.starts(), ["--session " + "x".repeat(128)]);
});

// ---- dispose ----

test("dispose: no report starts afterwards, the child is killed, the queue is dropped, timers and listeners go", async (t) => {
  const host = await start(t, { autocreate: false });
  await host.session("ses-old");
  host.spawns[0].finish();
  await flush();
  await host.home();
  assert.equal(host.spawns.length, 2, "the pending report is running");
  await host.session("ses-new");
  host.dispose();
  host.dispose();
  await flush();
  assert.deepEqual(host.spawns[1].signals, ["SIGKILL"]);
  host.spawns[1].finish();
  await host.clock.tick(10000);
  assert.equal(host.spawns.length, 2, "the queued --session new never starts");
  assert.equal(host.unsubscribed, 1);
  assert.equal(host.clock.size, 0, "no timer is left");
});

test("dispose: a create that resolves later neither navigates nor reports", async (t) => {
  const host = await boundTo(t, "ses-old");
  await host.home();
  host.spawns[1].finish();
  await flush();
  host.dispose();
  host.creates[0].resolve("auto");
  await flush();
  await host.clock.tick(1000);
  assert.deepEqual(host.navigations, []);
  assert.deepEqual(host.starts(), ["--session ses-old", "--pending"]);
});

test("dispose: events after unload are ignored", async (t) => {
  const host = await start(t);
  host.dispose();
  host.state.route = { type: "session", sessionID: "late" };
  await host.clock.tick(1000);
  assert.deepEqual(host.starts(), []);
});

// ---- report channel ----

test("reports: a pending child past its bound is killed, retried, and never reaches Crowded after the newer session", async (t) => {
  const host = await start(t, { autocreate: false });
  await host.session("old");
  host.spawns[0].finish();
  await flush();
  await host.home();
  await host.session("new");
  assert.deepEqual(host.starts(), ["--session old", "--pending"], "new waits behind the running pending");
  await host.clock.tick(4600);
  assert.deepEqual(host.spawns[1].signals, []);
  await host.clock.tick(200);
  assert.deepEqual(host.spawns[1].signals, ["SIGTERM"]);
  await flush();
  assert.deepEqual(host.starts(), ["--session old", "--pending", "--pending"], "the killed pending is sent again before the session");
  host.spawns[2].finish();
  await flush();
  host.spawns[3].finish();
  await flush();
  assert.deepEqual(host.delivered, ["--session old", "--pending", "--session new"]);
});

test("reports: a child that ignores SIGTERM is sent SIGKILL and the next report waits for its exit", async (t) => {
  const host = await start(t, { autocreate: false, ignore: ["SIGTERM"] });
  await host.session("a");
  await host.clock.tick(5500);
  assert.deepEqual(host.spawns[0].signals, ["SIGTERM"]);
  await host.session("b");
  assert.equal(host.spawns.length, 1, "b waits");
  await host.clock.tick(300);
  assert.deepEqual(host.spawns[0].signals, ["SIGTERM", "SIGKILL"]);
  await flush();
  assert.deepEqual(host.starts(), ["--session a", "--session b"]);
});

test("reports: a child that cannot be killed keeps the next report waiting with a bounded backlog", async (t) => {
  const host = await start(t, { autocreate: false, immortal: true });
  await host.session("a");
  await host.clock.tick(7000);
  for (let i = 0; i < 20; i += 1) {
    await host.session(`s${i}`);
    await host.home();
  }
  await host.clock.tick(60000);
  assert.equal(host.spawns.length, 1, "nothing starts while the child lives");
  host.spawns[0].die(0);
  await flush();
  assert.equal(host.spawns.length, 2, "exactly one queued report starts");
  assert.equal(host.spawns[1].label, "--pending", "the latest state wins");
  host.spawns[1].finish();
  await flush();
  assert.equal(host.spawns.length, 2, "and nothing else was queued");
});

test("reports: a stuck child is said once on stderr, never on stdout, and is not abandoned", async (t) => {
  const host = await start(t, { autocreate: false, immortal: true });
  await host.session("a");
  await host.clock.tick(5000);
  assert.deepEqual(host.stderr, [], "not stuck while SIGTERM may still work");
  await host.clock.tick(1000 + 2000 + 100);
  assert.equal(host.stderr.length, 1);
  assert.match(host.stderr[0], /survived SIGKILL/);
  await host.clock.tick(120000);
  assert.equal(host.stderr.length, 1, "said once");
  await host.session("b");
  await host.clock.tick(120000);
  assert.equal(host.spawns.length, 1, "no second child beside a stuck one");
  assert.deepEqual(host.stdout, []);
});

test("reports: a child without a kill method that exits late is waited for", async (t) => {
  const host = await start(t, { autocreate: false, killThrows: true, immortal: true });
  await host.session("a");
  await host.clock.tick(9000);
  await host.session("b");
  assert.equal(host.spawns.length, 1);
  host.spawns[0].die(0);
  await flush();
  assert.equal(host.spawns.length, 2, "the report that waited starts after the exit");
});

test("reports: a long chain of route changes leaves at most one queued report, the latest state", async (t) => {
  const host = await start(t, { autocreate: false, immortal: true });
  await host.session("first");
  for (let i = 0; i < 15; i += 1) {
    await host.session(`s${i}`);
    await host.home();
  }
  await host.session("final");
  assert.equal(host.spawns.length, 1);
  host.spawns[0].die(0);
  await flush();
  assert.equal(host.spawns[1].label, "--pending");
  host.spawns[1].finish();
  await flush();
  assert.equal(host.spawns[2].label, "--session final");
  host.spawns[2].finish();
  await flush();
  assert.equal(host.spawns.length, 3);
});

test("reports: a pending boundary survives a newer session, sent first (running child still going)", async (t) => {
  const host = await start(t, { autocreate: false });
  await host.session("old");
  await host.home();
  await host.session("new");
  assert.deepEqual(host.starts(), ["--session old"]);
  host.spawns[0].finish();
  await flush();
  host.spawns[1].finish();
  await flush();
  host.spawns[2].finish();
  await flush();
  assert.deepEqual(host.delivered, ["--session old", "--pending", "--session new"]);
  assert.equal(host.spawns.length, 3);
});

test("reports: returning to the running session after a home flip still sends the pending marker, then the session", async (t) => {
  const host = await start(t, { autocreate: false });
  await host.session("a");
  await host.home();
  await host.session("a");
  await host.settle();
  assert.deepEqual(host.starts(), ["--session a", "--pending", "--session a"]);
});

test("reports: two quick route changes in every order always send pending before the session that follows it", async (t) => {
  const moves = {
    home: (h) => h.home(),
    sessionA: (h) => h.session("a"),
    sessionB: (h) => h.session("b"),
  };
  const names = Object.keys(moves);
  for (const first of names) {
    for (const second of names) {
      for (const third of [null, ...names]) {
        const host = await start(t, { autocreate: false });
        await host.session("start");
        const wanted = [first, second, third].filter(Boolean);
        for (const name of wanted) {
          await moves[name](host);
        }
        await host.settle();
        const sent = host.delivered;
        assert.equal(sent[0], "--session start");
        // Replay what Crowded would conclude and compare with the final route.
        let pendingSeen = false;
        let session = "start";
        for (const label of sent.slice(1)) {
          if (label === "--pending") {
            pendingSeen = true;
            session = null;
          } else {
            session = label.replace("--session ", "");
          }
        }
        const finalRoute = host.state.route;
        const expected = finalRoute.type === "session" ? finalRoute.sessionID : null;
        assert.equal(session, expected, `${wanted.join(",")}: Crowded ends on ${expected}, sent ${sent}`);
        // Every home flip that was followed by another move must show up as a pending report.
        const homes = wanted.filter((name, index) => name === "home" && wanted[index - 1] !== "home").length;
        if (homes > 0) {
          assert.ok(pendingSeen, `${wanted.join(",")}: a pending boundary was dropped: ${sent}`);
        }
        host.dispose();
      }
    }
  }
});

test("reports: a timed-out report of a session is sent again when the human returns to it", async (t) => {
  const host = await start(t, { autocreate: false, ignore: ["SIGTERM"] });
  await host.session("a");
  await host.clock.tick(5100);
  await host.home();
  await host.session("a");
  await host.clock.tick(1000);
  await host.settle();
  assert.deepEqual(host.delivered, ["--pending", "--session a"]);
});

test("reports: delivery order is preserved whatever order the host finishes the children in", async (t) => {
  for (const order of [[0, 1], [1, 0]]) {
    const host = await start(t, { autocreate: false });
    await host.session("a");
    await host.session("b");
    for (const index of order) {
      host.spawns[index]?.finish();
      await flush();
    }
    await host.settle();
    assert.deepEqual(host.delivered, ["--session a", "--session b"], `finish order ${order}`);
    host.dispose();
  }
});

test("reports: exactly one child is ever in flight", async (t) => {
  const host = await start(t, { autocreate: false });
  await host.session("a");
  await host.home();
  await host.session("b");
  await host.emitCreated("c");
  await host.home();
  assert.equal(host.spawns.filter((child) => !child.dead).length, 1);
  await host.settle();
});

// ---- P16: background sessions ----

test("background: a session.created that is not on screen is not reported, and is reported once the route shows it", async (t) => {
  const host = await boundTo(t, "ses-old");
  await host.emitCreated("background");
  await host.clock.tick(1000);
  await host.settle();
  assert.deepEqual(host.starts(), ["--session ses-old"], "the displayed session is still the old one");
  await host.session("background");
  await host.settle();
  assert.deepEqual(host.starts(), ["--session ses-old", "--session background"]);
});

test("background: a session created in the background during an auto-create does not take the place of the created one", async (t) => {
  const host = await boundTo(t, "ses-old");
  await host.home();
  host.spawns[1].finish();
  await flush();
  await host.emitCreated("background");
  host.creates[0].resolve("auto");
  await flush();
  await host.settle();
  assert.deepEqual(host.navigations, ["auto"]);
  assert.deepEqual(host.starts(), ["--session ses-old", "--pending", "--session auto"]);
});

// ---- P17: a killed report is not a delivered one ----

test("exit status: a pending child killed on timeout does not start the create until a pending report exits 0", async (t) => {
  const host = await boundTo(t, "old");
  await host.home();
  assert.equal(host.spawns[1].label, "--pending");
  await host.clock.tick(5000);
  await host.clock.tick(1000);
  assert.deepEqual(host.spawns[1].signals, ["SIGTERM"]);
  host.spawns[1].die(137);
  await flush();
  assert.equal(host.creates.length, 0, "Crowded never saw the pending boundary");
  assert.equal(host.spawns[2].label, "--pending", "the pending report is sent again");
  host.spawns[2].finish();
  await flush();
  assert.equal(host.creates.length, 1);
});

test("exit status: a pending child that exits 0 only after it was killed is not counted as delivered", async (t) => {
  const host = await boundTo(t, "old", { killExit: 0 });
  await host.home();
  await host.clock.tick(5100);
  assert.deepEqual(host.spawns[1].signals, ["SIGTERM"]);
  await flush();
  assert.equal(host.creates.length, 0, "the late exit status says nothing about delivery");
  assert.equal(host.spawns[2].label, "--pending");
  host.spawns[2].finish();
  await flush();
  assert.equal(host.creates.length, 1);
});

test("exit status: a pending child that exits non-zero is retried, then the create runs after a clean exit", async (t) => {
  const host = await boundTo(t, "old");
  await host.home();
  host.spawns[1].die(1);
  await flush();
  host.spawns[2].die(1);
  await flush();
  assert.equal(host.creates.length, 0);
  host.spawns[3].finish();
  await flush();
  assert.equal(host.creates.length, 1);
});

test("exit status: a session report that fails is sent again", async (t) => {
  const host = await start(t, { autocreate: false });
  await host.session("a");
  host.spawns[0].die(1);
  await flush();
  assert.equal(host.spawns[1].label, "--session a");
  host.spawns[1].finish();
  await flush();
  assert.deepEqual(host.delivered, ["--session a"]);
});

test("failed pending: three failures say so once on stderr, keep retrying slowly, and create, navigate and report nothing meanwhile", async (t) => {
  const host = await boundTo(t, "old");
  await host.home();
  for (let i = 1; i <= 3; i += 1) {
    host.spawns[i].die(1);
    await flush();
  }
  assert.equal(host.stderr.length, 1, "one visible line");
  assert.match(host.stderr[0], /not accepted/);
  assert.equal(host.spawns.length, 4, "the next retry waits for its backoff");
  await host.clock.tick(4900);
  assert.equal(host.spawns.length, 4);
  await host.clock.tick(200);
  assert.equal(host.spawns.length, 5);
  assert.equal(host.spawns[4].label, "--pending");
  host.spawns[4].die(1);
  await flush();
  await host.clock.tick(15100);
  assert.equal(host.spawns.length, 6, "the wait grows");
  host.spawns[5].die(1);
  await flush();
  await host.clock.tick(60100);
  assert.equal(host.spawns.length, 7);
  host.spawns[6].die(1);
  await flush();
  await host.clock.tick(59000);
  assert.equal(host.spawns.length, 7, "never faster than once a minute");
  await host.clock.tick(2000);
  assert.equal(host.spawns.length, 8);
  assert.equal(host.stderr.length, 1, "said once");
  assert.deepEqual(host.creates, []);
  assert.deepEqual(host.navigations, []);
  assert.ok(host.starts().every((label) => label === "--session old" || label === "--pending"), "no new session is reported");
  assert.deepEqual(host.stdout, []);
});

test("failed pending: the pending report that finally lands lets the create run", async (t) => {
  const host = await boundTo(t, "old");
  await host.home();
  for (let i = 1; i <= 3; i += 1) {
    host.spawns[i].die(1);
    await flush();
  }
  await host.clock.tick(5100);
  host.spawns[4].finish();
  await flush();
  assert.equal(host.creates.length, 1);
});

test("failed pending: a session the human selects meanwhile is reported only after the pending marker lands", async (t) => {
  const host = await boundTo(t, "old", { autocreate: false });
  await host.home();
  await host.session("new");
  for (let i = 1; i <= 3; i += 1) {
    host.spawns[i].die(1);
    await flush();
  }
  await host.clock.tick(4000);
  assert.deepEqual(host.starts(), ["--session old", "--pending", "--pending", "--pending"], "the new session waits behind the failed pending");
  await host.clock.tick(1200);
  assert.equal(host.spawns[4].label, "--pending");
  host.spawns[4].finish();
  await flush();
  assert.equal(host.spawns[5].label, "--session new");
  host.spawns[5].finish();
  await flush();
  assert.deepEqual(host.delivered, ["--session old", "--pending", "--session new"]);
});

test("failed pending: the reviewer probe never rebinds without a recorded boundary", async (t) => {
  const host = await boundTo(t, "old");
  await host.home();
  await host.session("new");
  for (let i = 1; i <= 3; i += 1) {
    host.spawns[i].die(1);
    await flush();
  }
  await host.clock.tick(30000);
  assert.ok(!host.starts().includes("--session new"), "no --session new while the boundary is undelivered");
  assert.deepEqual(host.delivered, ["--session old"]);
  assert.equal(host.creates.length, 0);
});

test("failed pending: dispose during the backoff cancels the retry", async (t) => {
  const host = await boundTo(t, "old");
  await host.home();
  for (let i = 1; i <= 3; i += 1) {
    host.spawns[i].die(1);
    await flush();
  }
  host.dispose();
  await host.clock.tick(600000);
  assert.equal(host.spawns.length, 4);
  assert.equal(host.clock.size, 0);
});

test("failed session report: retried with a backoff, and a newer session replaces it", async (t) => {
  const host = await start(t, { autocreate: false });
  await host.session("a");
  for (let i = 0; i < 3; i += 1) {
    host.spawns[i].die(1);
    await flush();
  }
  assert.equal(host.spawns.length, 3);
  await host.session("b");
  await host.clock.tick(5100);
  assert.equal(host.spawns[3].label, "--session b", "the superseded id is not sent");
  host.spawns[3].finish();
  await flush();
  assert.deepEqual(host.delivered, ["--session b"]);
});

test("exit status: a failed older session report is superseded by a newer one", async (t) => {
  const host = await start(t, { autocreate: false });
  await host.session("a");
  await host.session("b");
  host.spawns[0].die(1);
  await flush();
  await host.settle();
  assert.deepEqual(host.starts(), ["--session a", "--session b"]);
});

// ---- real child processes ----

test("reports: with real child processes a killed pending never reaches Crowded after the newer session", async (t) => {
  const dir = mkdtempSync(join(tmpdir(), "tui-real-"));
  const log = join(dir, "calls.log");
  const gate = join(dir, "gate");
  const script = join(dir, "crowded");
  // A pending report waits at a gate the test opens; every report logs its
  // arguments only when it gets through.
  writeFileSync(
    script,
    `#!/bin/sh\nif [ "$2" = "--pending" ]; then while [ ! -e ${JSON.stringify(gate)} ]; do sleep 0.02; done; fi\nprintf '%s\\n' "$*" >> ${JSON.stringify(log)}\n`,
  );
  chmodSync(script, 0o755);
  const host = await start(t, { autocreate: false });
  process.env.CROWDED_BIN = script;
  const children = [];
  globalThis.Bun = {
    spawn(argv) {
      const [command, ...args] = argv;
      const child = nodeSpawn(command, args, { stdio: "ignore" });
      const exited = new Promise((resolve) => child.on("exit", (code) => resolve(code ?? 137)));
      children.push(child);
      return { exited, kill: (signal) => child.kill(signal) };
    },
  };
  t.after(() => children.forEach((child) => child.kill("SIGKILL")));
  const lines = () => (existsSync(log) ? readFileSync(log, "utf8").split("\n").filter((line) => line.length > 0) : []);
  const until = async (condition, what) => {
    const end = Date.now() + 30000;
    while (!condition()) {
      if (Date.now() > end) {
        throw new Error(`timed out waiting for ${what}`);
      }
      await new Promise((resolve) => realSetTimeout(resolve, 20));
    }
  };
  await host.session("old");
  await until(() => lines().length === 1, "the old report");
  await host.home();
  await host.session("new");
  await until(() => children.length === 2, "the pending child");
  await host.clock.tick(5100);
  await until(() => children.length === 3, "the pending report to be sent again after the kill");
  assert.deepEqual(lines(), ["session-report --session old --ack"], "nothing got through the gate");
  writeFileSync(gate, "");
  await until(() => lines().length === 3, "pending and the new session");
  await host.settle();
  assert.deepEqual(lines(), [
    "session-report --session old --ack",
    "session-report --pending --ack",
    "session-report --session new --ack",
  ]);
});

// ---- safety ----

test("safety: nothing is written to stdout, and the token and room never reach an argument", async (t) => {
  const host = await boundTo(t, "ses-old");
  await host.home();
  host.spawns[1].finish();
  await flush();
  host.creates[0].resolve("ses-new");
  await flush();
  await host.settle();
  assert.deepEqual(host.stdout, []);
  for (const child of host.spawns) {
    assert.deepEqual(child.argv.slice(0, 2), ["/fake/crowded", "session-report"]);
    assert.ok(["--pending", "--session"].includes(child.argv[2]));
    assert.ok(child.acked, "every report asks Crowded to acknowledge it");
    assert.ok(!child.argv.join(" ").includes("secret-token-value"));
    assert.ok(!child.argv.some((arg) => arg === "7" || /room/i.test(arg)));
  }
  await host.emitCreated("ses-x");
  host.subs["session.created"]({ data: { sessionID: "ses-y", room: 9, roomID: 9 } });
  await host.settle();
  for (const child of host.spawns) {
    assert.ok(!child.argv.some((arg) => arg === "9"), "a payload cannot name a room");
  }
});

test("safety: a missing ctx.ui is a no-op", async (t) => {
  const host = await start(t, { noUi: true });
  assert.equal(host.dispose, undefined);
  await host.clock.tick(1000);
  assert.deepEqual(host.spawns, []);
});

test("safety: host failures never throw into the TUI", async (t) => {
  for (const options of [
    { currentThrows: true },
    { spawnThrows: true },
    { subscribeThrows: true },
    { createThrows: true },
    { killThrows: true, immortal: true },
  ]) {
    const host = await start(t, options);
    await host.session("a");
    await host.home();
    await host.clock.tick(8000);
    await host.session("b");
    if (host.subs["session.created"]) {
      await host.emitCreated("c");
    }
    await host.clock.tick(8000);
    host.dispose();
    host.spawns.forEach((child) => child.die(0));
    await flush();
  }
});
