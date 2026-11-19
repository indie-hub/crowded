let connection = null;
let receiver = null;
let busy = false;
let stopped = false;
let started = false;

async function request($, path, data) {
  const response = await $.http.fetch(connection.url + path, {
    method: data === undefined ? "GET" : "POST",
    headers: { Authorization: `Bearer ${connection.token}`, "Content-Type": "application/json" },
    ...(data === undefined ? {} : { body: JSON.stringify(data) }),
  });
  if (!response.ok) {
    const err = new Error(`Inbox returned HTTP ${response.status}`);
    err.status = response.status;
    throw err;
  }
  const text = typeof response.text === "function" ? await response.text() : response.text;
  if (!text) return {};
  if (new TextEncoder().encode(text).length > 65536) {
    throw new Error("Response exceeds 64 KiB");
  }
  return JSON.parse(text);
}

function validMessage(message) {
  return message && typeof message === "object" && typeof message.id === "string" && typeof message.text === "string" && typeof message.sha256 === "string";
}

async function poll($) {
  if (busy || stopped) return;
  busy = true;
  try {
    const message = await request($, "/next", { receiver });
    if (!message) return;
    if (!validMessage(message)) {
      stopped = true;
      $.ui.status("Delivery uncertain. No automatic resend.");
      return;
    }
    const result = await $.prompt.submit({ text: message.text });
    if (result && result.drop) {
      await request($, "/receipt", { receiver, id: message.id, sha256: message.sha256, state: "held", reason: result.drop });
      stopped = true;
      $.ui.status("Crowded message held. No automatic retry.");
      return;
    }
    await request($, "/receipt", { receiver, id: message.id, sha256: message.sha256, state: "started", reason: "" });
  } catch {
    stopped = true;
    $.ui.status("Delivery uncertain. No automatic resend.");
  } finally {
    busy = false;
  }
}

function validUrl(urlStr) {
  try {
    const url = new URL(urlStr);
    if (url.protocol !== "http:" || url.hostname !== "127.0.0.1" || !url.port) return null;
    if (url.username || url.password || url.pathname !== "/" || url.search || url.hash) return null;
    return url.origin;
  } catch {
    return null;
  }
}

export function register(on) {
  on("session.start", async ($, e, next) => {
    if (started) return next(e);
    started = true;
    if (!e.isInteractive) return next(e);
    const cwd = e.cwd;
    try {
      const sessionId = await $.session.id();
      const file = `${cwd}/.crowded/claude-inbox/${sessionId}.json`;
      if (!(await $.fs.exists(file))) {
        $.ui.status("Crowded inbox not connected");
        return next(e);
      }
      const parsed = JSON.parse(await $.fs.read(file));
      const token = parsed && parsed.token;
      const origin = parsed && validUrl(parsed.url);
      if (typeof token !== "string" || !/^[a-f0-9]{64}$/.test(token) || !origin) {
        $.ui.status("Crowded inbox not connected");
        return next(e);
      }
      connection = { url: origin, token };
      try {
        const reg = await request($, "/register", { session: sessionId });
        if (!reg || typeof reg.receiver !== "string") {
          $.ui.status("Crowded inbox not connected");
          return next(e);
        }
        receiver = reg.receiver;
      } catch {
        $.ui.status("Crowded inbox not connected");
        return next(e);
      }
      $.clock.every(500, () => poll($));
      $.ui.status("Crowded inbox connected");
    } catch {
      stopped = true;
      $.ui.status("Crowded inbox not connected");
    }
    return next(e);
  });
}
