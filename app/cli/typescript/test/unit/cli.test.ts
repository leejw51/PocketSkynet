import assert from "node:assert/strict";
import { test } from "node:test";
import { parseCli, UsageError } from "../../src/cli.js";

const noEnv = {} as NodeJS.ProcessEnv;

test("cli parse: defaults", () => {
  const cli = parseCli(["health"], noEnv);
  assert.equal(cli.command, "health");
  assert.equal(cli.clientOptions.baseUrl, "http://127.0.0.1:9099");
  assert.equal(cli.clientOptions.http3, false);
  assert.equal(cli.clientOptions.insecure, false);
  assert.equal(cli.clientOptions.privateKey, undefined);
  assert.equal(cli.json, false);
});

test("cli parse: flags land in client options", () => {
  const cli = parseCli(
    [
      "send",
      "room_0123456789",
      "hello",
      "world",
      "--server",
      "https://10.0.0.2:9101",
      "--http3",
      "--insecure",
      "--key",
      "0x" + "ab".repeat(32),
      "--username",
      "alice",
    ],
    noEnv,
  );
  assert.equal(cli.command, "send");
  assert.deepEqual(cli.positionals, ["room_0123456789", "hello", "world"]);
  assert.equal(cli.clientOptions.baseUrl, "https://10.0.0.2:9101");
  assert.equal(cli.clientOptions.http3, true);
  assert.equal(cli.clientOptions.insecure, true);
  assert.equal(cli.clientOptions.privateKey, "0x" + "ab".repeat(32));
  assert.equal(cli.clientOptions.username, "alice");
});

test("cli parse: POCKETSKYNET_KEY and POCKETSKYNET_SERVER env fallbacks", () => {
  const env = {
    POCKETSKYNET_KEY: "0x" + "cd".repeat(32),
    POCKETSKYNET_SERVER: "http://192.168.1.5:9099",
    POCKETSKYNET_TOKEN: "jwt-token",
  } as unknown as NodeJS.ProcessEnv;
  const cli = parseCli(["rooms"], env);
  assert.equal(cli.clientOptions.privateKey, "0x" + "cd".repeat(32));
  assert.equal(cli.clientOptions.baseUrl, "http://192.168.1.5:9099");
  assert.equal(cli.clientOptions.token, "jwt-token");
  // Explicit flag beats env:
  const overridden = parseCli(["rooms", "--server", "http://127.0.0.1:1"], env);
  assert.equal(overridden.clientOptions.baseUrl, "http://127.0.0.1:1");
});

test("cli parse: bad flags and bad limits are usage errors", () => {
  assert.throws(
    () => parseCli(["health", "--no-such-flag"], noEnv),
    UsageError,
  );
  assert.throws(
    () => parseCli(["messages", "r", "--limit", "0"], noEnv),
    UsageError,
  );
  assert.throws(
    () => parseCli(["messages", "r", "--limit", "101"], noEnv),
    UsageError,
  );
  assert.throws(
    () => parseCli(["messages", "r", "--limit", "many"], noEnv),
    UsageError,
  );
  assert.equal(parseCli(["messages", "r", "--limit", "5"], noEnv).limit, 5);
});
