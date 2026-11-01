// Activation preflight check: import the full self-improving-agent harness
// (TypeScript import graph, tool registration, adapter wiring) before a
// freshly built candidate may be activated. A --help smoke on the Rust
// binaries cannot reach these failure modes, so the guardian runs this file
// as part of its fail-closed preflight profile.
const harness = await import("../harness.ts");

if (
  typeof harness.default !== "object" ||
  harness.default === null ||
  typeof harness.registerExoTools !== "function"
) {
  console.error("harness import smoke failed: expected harness exports are missing");
  process.exit(1);
}
console.log("harness import smoke passed");
