import { describe, expect, it } from "vitest";

import { CliUsageError, parseCliArgs } from "../../src/cli/args.js";

describe("parseCliArgs", () => {
  it("parses runtime, workspace, session, json, and message", () => {
    const options = parseCliArgs([
      "--runtime",
      "remote:codex",
      "--workspace",
      ".",
      "--session",
      "abc123",
      "--remote-runtimes",
      "claude,codex,gemini",
      "--json",
      "hello",
      "world",
    ]);

    expect(options.runtimeTarget).toBe("remote:codex");
    expect(options.sessionHint).toBe("abc123");
    expect(options.json).toBe(true);
    expect(options.remoteRuntimes).toEqual(["claude", "codex", "gemini"]);
    expect(options.message).toBe("hello world");
  });

  it("throws on missing message", () => {
    expect(() => parseCliArgs(["--runtime", "demo"])).toThrow(CliUsageError);
  });

  it("allows listing runtimes without a message", () => {
    const options = parseCliArgs(["--list-runtimes"]);
    expect(options.listRuntimes).toBe(true);
    expect(options.message).toBeUndefined();
  });
});
