// Does the pinned claude-agent-acp adapter honor `allow_always` across tool
// calls? A view-free probe: it speaks the same ACP handshake view speaks
// (crates/view-ai/src/acp/driver.rs), answers the first permission request
// with the adapter's own `allow_always` option id verbatim, and counts how
// many further requests arrive for the same tool kind. One request means the
// adapter honors the grant; more mean view's own standing-answer store is
// what delivers the semantics the user consented to.
//
// The pin this is recorded against, and the assertion that makes a bump
// re-run it, live in `crates/view-ai/src/provision.rs`.
//
//   node scripts/acp-allow-always-probe.mjs [version]   # default: the pin
//
// Requires a provisioned adapter (`view` provisions on first AI use, or
// `cargo test -p view-ai` against a warm cache) and working credentials.
import { spawn } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";

const HERE = path.dirname(new URL(import.meta.url).pathname);
const PIN = fs
  .readFileSync(path.join(HERE, "..", "crates", "view-ai", "src", "provision.rs"), "utf8")
  .match(/id:\s*"claude-code",\s*\n\s*version:\s*"([^"]+)"/);
const VERSION = process.argv[2] || (PIN && PIN[1]);
if (!VERSION) {
  console.error("no version given and none readable from provision.rs");
  process.exit(2);
}
const CACHE = process.env.XDG_CACHE_HOME || path.join(os.homedir(), ".cache");
const ENTRY = path.join(
  CACHE,
  "view/adapters/claude-code",
  VERSION,
  "extracted/package/dist/index.js",
);
if (!fs.existsSync(ENTRY)) {
  console.error(`no provisioned adapter at ${ENTRY}`);
  process.exit(2);
}

const WORK = fs.mkdtempSync(path.join(os.tmpdir(), "acp-allow-always-probe-"));
const LOG = fs.createWriteStream(path.join(WORK, "wire.log"));
for (const f of ["a", "b", "c", "d"]) fs.writeFileSync(path.join(WORK, f + ".txt"), "x1\n");

// The adapter reads CLAUDE*/AI_AGENT out of the environment to decide it is
// running under an agent; a probe launched from one must look like a plain
// terminal or the run it measures is not the run a user gets.
const env = { ...process.env };
for (const k of Object.keys(env)) {
  if (k.startsWith("CLAUDE") || k === "AI_AGENT") delete env[k];
}
env.HOME = os.homedir();

const child = spawn("node", [ENTRY], { cwd: WORK, env, stdio: ["pipe", "pipe", "pipe"] });
child.stderr.on("data", (d) => LOG.write("STDERR " + d));

let nextId = 1;
const pending = new Map();
function send(obj) {
  const line = JSON.stringify(obj);
  LOG.write("--> " + line + "\n");
  child.stdin.write(line + "\n");
}
function request(method, params) {
  const id = nextId++;
  return new Promise((res, rej) => {
    pending.set(id, { res, rej });
    send({ jsonrpc: "2.0", id, method, params });
  });
}

let permCount = 0;
const permLog = [];

function onRequest(msg) {
  const { id, method, params } = msg;
  if (method === "session/request_permission") {
    permCount += 1;
    const always = params.options.find((o) => o.kind === "allow_always");
    permLog.push({
      n: permCount,
      toolCallId: params.toolCall?.toolCallId,
      title: params.toolCall?.title,
      options: params.options.map((o) => `${o.optionId}/${o.kind}`),
    });
    if (!always) {
      send({ jsonrpc: "2.0", id, result: { outcome: { outcome: "cancelled" } } });
      return;
    }
    send({
      jsonrpc: "2.0",
      id,
      result: { outcome: { outcome: "selected", optionId: always.optionId } },
    });
    return;
  }
  if (method === "fs/read_text_file") {
    let content = "";
    try {
      content = fs.readFileSync(params.path, "utf8");
    } catch {}
    send({ jsonrpc: "2.0", id, result: { content } });
    return;
  }
  if (method === "fs/write_text_file") {
    try {
      fs.writeFileSync(params.path, params.content);
    } catch {}
    send({ jsonrpc: "2.0", id, result: {} });
    return;
  }
  send({
    jsonrpc: "2.0",
    id,
    error: { code: -32601, message: "probe does not implement " + method },
  });
}

let buf = "";
child.stdout.on("data", (chunk) => {
  buf += chunk;
  let i;
  while ((i = buf.indexOf("\n")) >= 0) {
    const line = buf.slice(0, i);
    buf = buf.slice(i + 1);
    if (!line.trim()) continue;
    LOG.write("<-- " + line + "\n");
    let msg;
    try {
      msg = JSON.parse(line);
    } catch {
      continue;
    }
    if (msg.method && msg.id !== undefined) onRequest(msg);
    else if (msg.id !== undefined && pending.has(msg.id)) {
      const p = pending.get(msg.id);
      pending.delete(msg.id);
      if (msg.error) p.rej(new Error(JSON.stringify(msg.error)));
      else p.res(msg.result);
    }
  }
});

const PROMPT =
  "You have four files in the cwd: a.txt, b.txt, c.txt, d.txt. Each contains the single line `x1`. " +
  "Using the Edit tool four separate times (once per file, no other tool), change `x1` to `x2` in a.txt, then b.txt, then c.txt, then d.txt. Then say DONE.";

function verdict(failure) {
  const honored = !failure && permCount === 1;
  console.log(
    JSON.stringify(
      {
        version: VERSION,
        // named only while the directory below still exists
        wire: honored ? null : path.join(WORK, "wire.log"),
        permission_requests: permCount,
        honors_allow_always: honored,
        failure: failure || null,
        requests: permLog,
      },
      null,
      2,
    ),
  );
  child.kill();
  LOG.end();
  // the wire log is the evidence any verdict other than a clean
  // allow-always names, so the work directory outlives one of those and
  // nothing else: a probe run per adapter release would otherwise leave a
  // directory per run in the temp root forever
  if (honored) fs.rmSync(WORK, { recursive: true, force: true });
  process.exit(failure ? 1 : 0);
}

(async () => {
  const init = await request("initialize", {
    protocolVersion: 1,
    clientCapabilities: {
      fs: { readTextFile: true, writeTextFile: true },
      terminal: false,
    },
    clientInfo: { name: "view", title: "view", version: "0.0.1" },
  });
  LOG.write("INIT " + JSON.stringify(init) + "\n");
  const sess = await request("session/new", { cwd: WORK, mcpServers: [] });
  LOG.write("SESSION " + JSON.stringify(sess) + "\n");
  const mode = await request("session/set_mode", {
    sessionId: sess.sessionId,
    modeId: process.env.PROBE_MODE || "default",
  });
  LOG.write("SET_MODE " + JSON.stringify(mode) + "\n");
  const res = await request("session/prompt", {
    sessionId: sess.sessionId,
    prompt: [{ type: "text", text: PROMPT }],
  });
  LOG.write("PROMPT_RESULT " + JSON.stringify(res) + "\n");
  verdict(null);
})().catch((e) => verdict(e.message));
