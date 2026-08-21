#!/usr/bin/env node
/**
 * `pskynet-ts` — CLI for the PocketSkynet server.
 *
 * Exit codes: 0 success, 1 API/transport failure, 2 usage error.
 */

import { pathToFileURL } from "node:url";
import { parseArgs } from "node:util";
import { ClientOptions, PocketSkynetClient } from "./client.js";
import { ApiError, TransportError } from "./errors.js";
import { toChecksumAddress } from "./wallet.js";

export const USAGE = `pskynet-ts — TypeScript CLI for the PocketSkynet server

Usage:
  pskynet-ts <command> [args] [flags]

Commands:
  health                     Server health probe (no auth)
  login                      Challenge → sign → JWT; prints the token
  rooms                      List the rooms you are a member of
  create-room <name>         Create a channel
  send <roomId> <text>       Send a plaintext message
  messages <roomId>          List messages in a room

Flags:
  --server <url>       Server base URL (default http://127.0.0.1:9099,
                       env POCKETSKYNET_SERVER)
  --http3              Use HTTP/3 (spawns an HTTP/3-capable curl)
  --insecure           Accept self-signed TLS certificates (dev only)
  --ca <path>          Trust exactly this CA certificate (PEM)
  --key <hex>          Wallet private key (env POCKETSKYNET_KEY)
  --token <jwt>        Reuse an existing JWT (env POCKETSKYNET_TOKEN)
  --username <name>    Username for first-time login
  --limit <n>          Messages to fetch (messages command, default 50)
  --json               Print raw JSON responses
  --help               This text
`;

interface ParsedCli {
  command: string;
  positionals: string[];
  clientOptions: ClientOptions;
  limit: number | undefined;
  json: boolean;
  help: boolean;
}

export class UsageError extends Error {}

/** Parse argv (no `node script` prefix). Pure, for unit testing. */
export function parseCli(
  argv: string[],
  env: NodeJS.ProcessEnv = process.env,
): ParsedCli {
  let values, positionals;
  try {
    ({ values, positionals } = parseArgs({
      args: argv,
      allowPositionals: true,
      options: {
        server: { type: "string" },
        http3: { type: "boolean", default: false },
        insecure: { type: "boolean", default: false },
        ca: { type: "string" },
        key: { type: "string" },
        token: { type: "string" },
        username: { type: "string" },
        limit: { type: "string" },
        json: { type: "boolean", default: false },
        help: { type: "boolean", default: false },
      },
    }));
  } catch (err) {
    throw new UsageError(err instanceof Error ? err.message : String(err));
  }

  const [command = "", ...rest] = positionals;
  const clientOptions: ClientOptions = {
    baseUrl:
      values.server ?? env["POCKETSKYNET_SERVER"] ?? "http://127.0.0.1:9099",
    http3: values.http3,
    insecure: values.insecure,
  };
  if (values.ca !== undefined) clientOptions.caPath = values.ca;
  const key = values.key ?? env["POCKETSKYNET_KEY"];
  if (key !== undefined && key.length > 0) clientOptions.privateKey = key;
  const token = values.token ?? env["POCKETSKYNET_TOKEN"];
  if (token !== undefined && token.length > 0) clientOptions.token = token;
  if (values.username !== undefined) clientOptions.username = values.username;

  let limit: number | undefined;
  if (values.limit !== undefined) {
    limit = Number(values.limit);
    if (!Number.isInteger(limit) || limit < 1 || limit > 100) {
      throw new UsageError("--limit must be an integer between 1 and 100");
    }
  }

  return {
    command,
    positionals: rest,
    clientOptions,
    limit,
    json: values.json,
    help: values.help,
  };
}

function requireArgs(cli: ParsedCli, count: number, shape: string): void {
  if (cli.positionals.length < count) {
    throw new UsageError(`usage: pskynet-ts ${cli.command} ${shape}`);
  }
}

async function run(cli: ParsedCli, out: (line: string) => void): Promise<void> {
  const client = new PocketSkynetClient(cli.clientOptions);
  try {
    switch (cli.command) {
      case "health": {
        const health = await client.health();
        out(
          cli.json
            ? JSON.stringify(health)
            : `status: ${health.status} (uptime ${health.uptime ?? "?"}s)`,
        );
        break;
      }
      case "login": {
        const result = await client.login();
        if (cli.json) {
          out(JSON.stringify(result.response));
        } else {
          out(`address:  ${toChecksumAddress(result.walletAddress)}`);
          out(`username: ${result.response.user.username ?? ""}`);
          out(`token:    ${result.response.token}`);
        }
        break;
      }
      case "rooms": {
        const rooms = await client.rooms();
        if (cli.json) {
          out(JSON.stringify(rooms));
        } else {
          for (const room of rooms) {
            out(`${room.id}\t${room.kind ?? "channel"}\t${room.name}`);
          }
        }
        break;
      }
      case "create-room": {
        requireArgs(cli, 1, "<name>");
        const room = await client.createRoom(cli.positionals[0]!);
        out(cli.json ? JSON.stringify(room) : `${room.id}\t${room.name}`);
        break;
      }
      case "send": {
        requireArgs(cli, 2, "<roomId> <text>");
        const [roomId, ...words] = cli.positionals;
        const message = await client.sendMessage(roomId!, words.join(" "));
        out(
          cli.json
            ? JSON.stringify(message)
            : `${message.id}\tserial=${message.msgSerial}`,
        );
        break;
      }
      case "messages": {
        requireArgs(cli, 1, "<roomId>");
        const opts = cli.limit !== undefined ? { limit: cli.limit } : {};
        const messages = await client.messages(cli.positionals[0]!, opts);
        if (cli.json) {
          out(JSON.stringify(messages));
        } else {
          for (const message of messages) {
            const sender = message.sender?.username ?? message.senderAddress;
            out(
              `[${new Date(message.messageTimestamp).toISOString()}] ${sender}: ${message.content}`,
            );
          }
        }
        break;
      }
      case "":
        throw new UsageError("no command given");
      default:
        throw new UsageError(`unknown command: ${cli.command}`);
    }
  } finally {
    await client.close();
  }
}

export async function main(argv: string[]): Promise<number> {
  let cli: ParsedCli;
  try {
    cli = parseCli(argv);
  } catch (err) {
    if (err instanceof UsageError) {
      process.stderr.write(`error: ${err.message}\n\n${USAGE}`);
      return 2;
    }
    throw err;
  }
  if (cli.help || cli.command === "help") {
    process.stdout.write(USAGE);
    return 0;
  }
  try {
    await run(cli, (line) => process.stdout.write(`${line}\n`));
    return 0;
  } catch (err) {
    if (err instanceof UsageError) {
      process.stderr.write(`error: ${err.message}\n`);
      return 2;
    }
    if (err instanceof ApiError) {
      const details =
        err.errors !== undefined ? ` [${err.errors.join("; ")}]` : "";
      const code = err.code !== undefined ? ` (${err.code})` : "";
      process.stderr.write(
        `error: HTTP ${err.status}${code}: ${err.message}${details}\n`,
      );
      return 1;
    }
    if (err instanceof TransportError || err instanceof Error) {
      process.stderr.write(`error: ${err.message}\n`);
      return 1;
    }
    process.stderr.write(`error: ${String(err)}\n`);
    return 1;
  }
}

// Run only when invoked as a program, not when imported by tests.
const invokedDirectly =
  process.argv[1] !== undefined &&
  import.meta.url === pathToFileURL(process.argv[1]).href;
if (invokedDirectly) {
  main(process.argv.slice(2))
    .then((code) => {
      process.exitCode = code;
    })
    .catch((err: unknown) => {
      // `main` maps known failures to exit codes itself; this catches anything
      // unexpected (e.g. a synchronous throw during teardown) so it becomes a
      // clean `error: … ` + exit 1 rather than an unhandled rejection.
      process.stderr.write(
        `error: ${err instanceof Error ? err.message : String(err)}\n`,
      );
      process.exitCode = 1;
    });
}
