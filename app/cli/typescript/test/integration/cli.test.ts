import assert from "node:assert/strict";
import { execFile } from "node:child_process";
import { join } from "node:path";
import { after, before, test } from "node:test";
import { promisify } from "node:util";
import { randomPrivateKeyHex, TestServer } from "../helpers/harness.js";
import { packageRoot } from "../helpers/vectors.js";

const execFileAsync = promisify(execFile);

let server: TestServer;
const cliJs = join(packageRoot(), "dist", "src", "cli.js");

interface CliRun {
  code: number;
  stdout: string;
  stderr: string;
}

async function runCli(
  args: string[],
  env: Record<string, string> = {},
): Promise<CliRun> {
  // Scrub any POCKETSKYNET_* the developer's shell may carry, then apply
  // exactly what this test asked for.
  const base: NodeJS.ProcessEnv = { ...process.env };
  for (const key of Object.keys(base)) {
    if (key.startsWith("POCKETSKYNET_")) delete base[key];
  }
  try {
    const { stdout, stderr } = await execFileAsync(
      process.execPath,
      [cliJs, ...args],
      {
        env: { ...base, ...env },
        timeout: 30_000,
      },
    );
    return { code: 0, stdout, stderr };
  } catch (err) {
    const failure = err as { code?: number; stdout?: string; stderr?: string };
    return {
      code: typeof failure.code === "number" ? failure.code : 1,
      stdout: failure.stdout ?? "",
      stderr: failure.stderr ?? "",
    };
  }
}

before(async () => {
  server = await TestServer.start();
});

after(async () => {
  await server?.stop();
});

test("cli: health exits 0", async () => {
  const run = await runCli(["health", "--server", server.baseUrl]);
  assert.equal(run.code, 0, run.stderr);
  assert.match(run.stdout, /status: ok/);
});

test("cli: login prints address, username and token; exits 0", async () => {
  const key = randomPrivateKeyHex();
  const run = await runCli([
    "login",
    "--server",
    server.baseUrl,
    "--key",
    key,
    "--username",
    "cli_alice",
  ]);
  assert.equal(run.code, 0, run.stderr);
  assert.match(run.stdout, /address: {2}0x[0-9a-fA-F]{40}/);
  assert.match(run.stdout, /username: cli_alice/);
  assert.match(
    run.stdout,
    /token: {4}[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+/,
  );
});

test("cli: full flow via POCKETSKYNET_KEY env — rooms, create-room, send, messages", async () => {
  const key = randomPrivateKeyHex();
  const env = { POCKETSKYNET_KEY: key, POCKETSKYNET_SERVER: server.baseUrl };

  const rooms = await runCli(["rooms"], env);
  assert.equal(rooms.code, 0, rooms.stderr);
  assert.match(rooms.stdout, /My Note/);
  assert.match(rooms.stdout, /My Jarvis/);
  assert.match(rooms.stdout, /My Lobby/);

  const created = await runCli(["create-room", "cli room"], env);
  assert.equal(created.code, 0, created.stderr);
  const roomId = created.stdout.trim().split("\t")[0]!;
  assert.ok(roomId.length >= 10, `room id from stdout: ${created.stdout}`);

  const sent = await runCli(
    ["send", roomId, "hello", "from", "the", "cli"],
    env,
  );
  assert.equal(sent.code, 0, sent.stderr);
  assert.match(sent.stdout, /serial=\d+/);

  const messages = await runCli(["messages", roomId], env);
  assert.equal(messages.code, 0, messages.stderr);
  assert.match(messages.stdout, /hello from the cli/);

  const json = await runCli(["messages", roomId, "--json"], env);
  assert.equal(json.code, 0, json.stderr);
  const parsed = JSON.parse(json.stdout) as { content: string }[];
  assert.equal(parsed.length, 1);
  assert.equal(parsed[0]!.content, "hello from the cli");
});

test("cli: API failure (non-member room) exits 1 with the message on stderr", async () => {
  const key = randomPrivateKeyHex();
  const env = { POCKETSKYNET_KEY: key, POCKETSKYNET_SERVER: server.baseUrl };
  const run = await runCli(["messages", "room_not_yours_1234567890"], env);
  assert.equal(run.code, 1);
  assert.match(run.stderr, /HTTP 403/);
  assert.match(run.stderr, /Access denied/);
});

test("cli: unreachable server exits 1", async () => {
  const run = await runCli(["health", "--server", "http://127.0.0.1:9"]);
  assert.equal(run.code, 1);
  assert.match(run.stderr, /error:/);
});

test("cli: usage errors exit 2", async () => {
  const unknown = await runCli(["frobnicate", "--server", server.baseUrl]);
  assert.equal(unknown.code, 2);
  assert.match(unknown.stderr, /unknown command/);

  const missingArgs = await runCli(["send", "--server", server.baseUrl], {
    POCKETSKYNET_KEY: randomPrivateKeyHex(),
  });
  assert.equal(missingArgs.code, 2);

  const badFlag = await runCli(["health", "--frob"]);
  assert.equal(badFlag.code, 2);

  const noCommand = await runCli([]);
  assert.equal(noCommand.code, 2);
});

test("cli: auth command without a key exits 1 with a clear message", async () => {
  const run = await runCli(["rooms", "--server", server.baseUrl]);
  assert.equal(run.code, 1);
  assert.match(run.stderr, /private key/);
});

test("cli: --help exits 0 and prints usage", async () => {
  const run = await runCli(["--help"]);
  assert.equal(run.code, 0);
  assert.match(run.stdout, /Usage:/);
  assert.match(run.stdout, /pskynet-ts/);
});

test("cli: hostile message content is sent verbatim, never interpreted", async () => {
  const key = randomPrivateKeyHex();
  const env = { POCKETSKYNET_KEY: key, POCKETSKYNET_SERVER: server.baseUrl };
  const created = await runCli(["create-room", "hostile room"], env);
  assert.equal(created.code, 0, created.stderr);
  const roomId = created.stdout.trim().split("\t")[0]!;

  const hostile = "$(touch /tmp/pwned-by-pskynet) `id` ; --insecure";
  const sent = await runCli(["send", roomId, hostile], env);
  assert.equal(sent.code, 0, sent.stderr);

  const listed = await runCli(["messages", roomId, "--json"], env);
  const parsed = JSON.parse(listed.stdout) as { content: string }[];
  assert.equal(parsed[0]!.content, hostile);
});
