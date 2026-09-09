import { spawn, type ChildProcessWithoutNullStreams } from "node:child_process";
import { once } from "node:events";
import {
  appendFile,
  copyFile,
  mkdir,
  mkdtemp,
  readFile,
  rm,
  chmod,
} from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { afterEach, beforeEach, expect, it } from "vitest";

let root: string;
let child: ChildProcessWithoutNullStreams | undefined;
let output: string;

beforeEach(async () => {
  root = await mkdtemp(join(tmpdir(), "exo quiet logs "));
  output = "";
  await mkdir(join(root, ".exo"));
  await copyFile(new URL("../exo.sh", import.meta.url), join(root, "exo.sh"));
  for (const name of ["exo", "exo-scheduler-runner"]) {
    await copyFile(
      new URL("./fixtures/quiet-repl-exo.sh", import.meta.url),
      join(root, name),
    );
    await chmod(join(root, name), 0o755);
  }
});

afterEach(async () => {
  if (child && child.exitCode === null && child.signalCode === null) {
    const closed = once(child, "close");
    // Kill the isolated process group, including fixture children on failures.
    process.kill(-child.pid!, "SIGKILL");
    await closed;
  }
  child = undefined;
  await rm(root, { recursive: true, force: true });
});

function launch(args: string[]) {
  child = spawn("/bin/bash", [join(root, "exo.sh"), ...args], {
    cwd: tmpdir(),
    detached: true,
    env: {
      PATH: process.env.PATH,
      HOME: process.env.HOME,
      TEST_ROOT: root,
      EXO_BIN: join(root, "exo"),
      EXO_SCHEDULER_BIN: join(root, "exo-scheduler-runner"),
    },
  });
  child.stdout.on("data", (data: Buffer) => {
    output += data.toString();
  });
  child.stderr.on("data", (data: Buffer) => {
    output += data.toString();
  });
  return child;
}

const replArgs = ["--template", "minimal", "--no-sandbox", "--control"];

it("keeps service output out of the REPL while retaining it in the logs", async () => {
  const repl = launch(replArgs);
  const closed = once(repl, "close");
  await expect.poll(() => output).toContain("repl-ready");
  repl.stdin.write("finish\n");
  expect(await closed).toEqual([7, null]);
  expect(output).not.toContain("output-during-repl");
  for (const service of ["adapters", "scheduler"]) {
    expect(
      await readFile(join(root, `.exo/exo-${service}.log`), "utf8"),
    ).toContain("output-during-repl");
  }
}, 10_000);

it("restarts the REPL on a guardian request and preserves its exit status", async () => {
  const repl = launch(replArgs);
  const closed = once(repl, "close");
  await expect.poll(() => output).toContain("repl-ready");
  repl.stdin.write("restart\n");
  await expect
    .poll(() => output, { timeout: 5_000 })
    .toMatch(/repl-ready[\s\S]*repl-ready/);
  const pids = (await readFile(join(root, "repl-pids"), "utf8"))
    .trim()
    .split("\n");
  expect(() => process.kill(Number(pids[0]), 0)).toThrow();
  repl.stdin.write("finish\n");
  expect(await closed).toEqual([7, null]);
  expect(() => process.kill(Number(pids[1]), 0)).toThrow();
  expect(output).not.toContain("output-during-repl");
}, 10_000);

it("follows both service logs without starting a REPL or requiring a binary", async () => {
  await rm(join(root, "exo"));
  const logs = launch(["logs"]);
  await expect.poll(() => output).toContain("exo-adapters.log");
  await appendFile(join(root, ".exo/exo-adapters.log"), "adapter-warning\n");
  await appendFile(join(root, ".exo/exo-scheduler.log"), "scheduler-warning\n");
  await expect.poll(() => output).toContain("adapter-warning");
  await expect.poll(() => output).toContain("scheduler-warning");
  expect(output).not.toContain("repl-ready");
  const closed = once(logs, "close");
  logs.kill("SIGTERM");
  expect(await closed).toEqual([null, "SIGTERM"]);
});

it("rejects extra arguments to logs", async () => {
  const logs = launch(["logs", "unexpected"]);
  expect((await once(logs, "close"))[0]).toBe(1);
  expect(output).toContain("logs does not accept additional arguments");
});

it("exits on terminal shutdown without restarting the REPL", async () => {
  const repl = launch(replArgs);
  const closed = once(repl, "close");
  await expect.poll(() => output).toContain("repl-ready");
  const pid = Number((await readFile(join(root, "repl-pids"), "utf8")).trim());
  process.kill(-repl.pid!, "SIGTERM");
  expect(await closed).toEqual([143, null]);
  expect(() => process.kill(pid, 0)).toThrow();
  expect(output.match(/repl-ready/g)).toHaveLength(1);
});
