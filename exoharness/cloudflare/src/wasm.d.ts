declare module "*.wasm" {
  const module: WebAssembly.Module;
  export default module;
}

declare module "*codex-sandbox/version" {
  const version: string;
  export default version;
}
