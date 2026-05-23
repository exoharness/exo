// Demo library tool for the TypeScript harness tool system. This example shows
// how a networked service integration can be packaged as a default-export Tool.
// It is not exposed by any example harness by default.

import net from "node:net";
import tls from "node:tls";

import type { JsonObject, Tool, ToolResult, TurnContext } from "@exo/harness";

interface IrcConfig {
  server: string;
  port: number;
  nick: string;
  username: string;
  realname: string;
  tls: boolean;
  dryRun: boolean;
  passwordSecretId: string | null;
}

const ircTool = {
  definition: {
    name: "irc_send_message",
    description: "Send a message to an IRC channel.",
    parameters: {
      type: "object",
      additionalProperties: false,
      properties: {
        channel: {
          type: "string",
          description: "IRC channel name, for example #exo.",
        },
        text: {
          type: "string",
          description: "Message text to send.",
        },
      },
      required: ["channel", "text"],
    },
    outputSchema: {
      type: "object",
      additionalProperties: false,
      properties: {
        ok: { type: "boolean" },
        dryRun: { type: "boolean" },
        server: { type: "string" },
        channel: { type: "string" },
      },
      required: ["ok", "dryRun", "server", "channel"],
    },
  },
  initializationParameters: {
    type: "object",
    additionalProperties: false,
    properties: {
      server: {
        type: "string",
        description: "IRC server hostname.",
      },
      port: {
        type: "number",
        description: "IRC server port.",
      },
      nick: {
        type: "string",
        description: "Nickname to use for the IRC connection.",
      },
      username: {
        type: "string",
        description: "Username to send in the IRC USER command.",
      },
      realname: {
        type: "string",
        description: "Real name to send in the IRC USER command.",
      },
      tls: {
        type: "boolean",
        description: "Whether to connect with TLS.",
      },
      dryRun: {
        type: "boolean",
        description: "If true, build IRC commands without opening a socket.",
      },
      passwordSecretId: {
        type: ["string", "null"],
        description: "Optional secret id containing an IRC server password.",
      },
    },
    required: ["server", "port", "nick", "username", "realname", "tls"],
  },
  initialize(args) {
    const config = parseConfig(args);
    return {
      async execute(args, execution): Promise<ToolResult> {
        return sendIrcMessage(execution.context, config, args);
      },
    };
  },
} satisfies Tool;

export default ircTool;

async function sendIrcMessage(
  context: TurnContext,
  config: IrcConfig,
  args: JsonObject,
): Promise<ToolResult> {
  const channel = stringArgument(args, "channel");
  const text = stringArgument(args, "text");
  const password = await resolvePassword(context, config.passwordSecretId);
  const commands = ircCommands(config, channel, text, password);

  if (!config.dryRun) {
    await withIrcConnection(config, async (socket) => {
      for (const command of commands) {
        socket.write(`${command}\r\n`);
      }
    });
  }

  return {
    ok: true,
    dryRun: config.dryRun,
    server: config.server,
    channel,
  };
}

function ircCommands(
  config: IrcConfig,
  channel: string,
  text: string,
  password: string | null,
): string[] {
  return [
    ...(password ? [`PASS ${password}`] : []),
    `NICK ${config.nick}`,
    `USER ${config.username} 0 * :${config.realname}`,
    `PRIVMSG ${channel} :${text}`,
    "QUIT",
  ];
}

async function resolvePassword(
  context: TurnContext,
  secretId: string | null,
): Promise<string | null> {
  if (!secretId) {
    return null;
  }
  const secret =
    await context.exoharness.current.conversation.getSecret(secretId);
  if (!secret) {
    throw new Error(`IRC password secret does not exist: ${secretId}`);
  }
  if (secret.type !== "key") {
    throw new Error("IRC password secret must be a key secret");
  }
  return secret.value;
}

async function withIrcConnection(
  config: IrcConfig,
  run: (socket: net.Socket) => Promise<void> | void,
): Promise<void> {
  await new Promise<void>((resolve, reject) => {
    const socket = config.tls
      ? tls.connect(config.port, config.server)
      : net.connect(config.port, config.server);
    socket.setEncoding("utf8");
    socket.setTimeout(10_000);
    socket.once("connect", async () => {
      try {
        await run(socket);
        socket.end(resolve);
      } catch (error) {
        socket.destroy();
        reject(error);
      }
    });
    socket.once("error", reject);
    socket.once("timeout", () => {
      socket.destroy(new Error("IRC connection timed out"));
    });
  });
}

function parseConfig(args: JsonObject): IrcConfig {
  return {
    server: stringArgument(args, "server"),
    port: numberArgument(args, "port"),
    nick: stringArgument(args, "nick"),
    username: stringArgument(args, "username"),
    realname: stringArgument(args, "realname"),
    tls: booleanArgument(args, "tls"),
    dryRun: optionalBooleanArgument(args, "dryRun") ?? false,
    passwordSecretId: optionalStringArgument(args, "passwordSecretId"),
  };
}

function stringArgument(args: JsonObject, name: string): string {
  const value = args[name];
  if (typeof value !== "string" || value.length === 0) {
    throw new Error(`IRC tool argument ${name} must be a non-empty string`);
  }
  return value;
}

function optionalStringArgument(args: JsonObject, name: string): string | null {
  const value = args[name];
  if (value === undefined || value === null) {
    return null;
  }
  if (typeof value !== "string" || value.length === 0) {
    throw new Error(`IRC tool argument ${name} must be a non-empty string`);
  }
  return value;
}

function numberArgument(args: JsonObject, name: string): number {
  const value = args[name];
  if (typeof value !== "number") {
    throw new Error(`IRC tool argument ${name} must be a number`);
  }
  return value;
}

function booleanArgument(args: JsonObject, name: string): boolean {
  const value = args[name];
  if (typeof value !== "boolean") {
    throw new Error(`IRC tool argument ${name} must be a boolean`);
  }
  return value;
}

function optionalBooleanArgument(
  args: JsonObject,
  name: string,
): boolean | null {
  const value = args[name];
  if (value === undefined || value === null) {
    return null;
  }
  if (typeof value !== "boolean") {
    throw new Error(`IRC tool argument ${name} must be a boolean`);
  }
  return value;
}
