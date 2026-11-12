import esbuild from "esbuild";

const production = process.argv[2] === "production";

const context = await esbuild.context({
  entryPoints: ["src/main.ts"],
  bundle: true,
  // Obsidian provides these at runtime.
  external: ["obsidian", "electron", "@codemirror/*", "@lezer/*"],
  format: "cjs",
  target: "es2022",
  platform: "browser",
  // The core is shipped inside main.js: Obsidian plugins are a single script.
  alias: { "vault-axi-core-wasm": "./wasm/core_bg.wasm" },
  loader: { ".wasm": "binary" },
  // wasm-bindgen's async loader (unused: the plugin calls initSync) references import.meta.url.
  define: { "import.meta.url": '"about:blank"' },
  sourcemap: production ? false : "inline",
  minify: production,
  treeShaking: true,
  logLevel: "info",
  outfile: "main.js",
});

if (production) {
  await context.rebuild();
  await context.dispose();
} else {
  await context.watch();
}
