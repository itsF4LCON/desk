// Builds the static client into dist/: app.js (esbuild), style.css (Tailwind), fonts, index.html.
import { build } from "esbuild";
import { execFileSync } from "node:child_process";
import { cpSync, mkdirSync, rmSync } from "node:fs";

rmSync("dist", { recursive: true, force: true });
mkdirSync("dist/fonts", { recursive: true });

await build({
  entryPoints: ["src/app.ts"],
  bundle: true,
  minify: true,
  format: "esm",
  target: "es2022",
  outfile: "dist/app.js",
});

execFileSync("npx", ["@tailwindcss/cli", "-i", "src/style.css", "-o", "dist/style.css", "--minify"], { stdio: "inherit" });

for (const w of [300, 400, 500, 600]) {
  cpSync(`node_modules/@fontsource/inter/files/inter-latin-${w}-normal.woff2`, `dist/fonts/inter-${w}.woff2`);
}
for (const w of [400, 500]) {
  cpSync(`node_modules/@fontsource/jetbrains-mono/files/jetbrains-mono-latin-${w}-normal.woff2`, `dist/fonts/jetbrains-mono-${w}.woff2`);
}
cpSync("src/index.html", "dist/index.html");
cpSync("src/favicon.svg", "dist/favicon.svg");
console.log("built dist/");
