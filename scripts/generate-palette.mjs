#!/usr/bin/env node
// Генератор палитры дизайн-системы из brand-цвета.
// Правь параметры ниже и запускай: npm run gen:palette
// Результат пишется в apps/web/src/styles/global.css между маркерами palette:begin/end;
// вручную сгенерированный блок не редактируется. Единственный файл стилей приложения — global.css.

import { readFile, writeFile } from "node:fs/promises";

const BRAND_HEX = "#704593";
const ROSE_HUE = 344;
const FUNCTIONAL_HUES = { success: 165, warning: 75, danger: 27, info: 279 };
const AMBIENT_VIOLET_HUE = 300;

// Тональная сетка OKLCH [L, C] на тему; hue по семейству.
// Откалибрована по одобренной палитре Voltrex от 4 ноября 2026.
// Решение владельца 2026-11-08: бордеры светлой темы вдвое слабее прежних
// (L между фоном 0.978 и прежним input 0.769), тёмная тема не менялась.
const TOKENS = [
  { name: "background", light: [0.978, 0.004], dark: [0.177, 0.004] },
  { name: "foreground", light: [0.255, 0.019], dark: [0.948, 0.007] },
  { name: "card", light: [1.0, 0.0], dark: [0.213, 0.012] },
  { name: "card-foreground", light: [0.255, 0.019], dark: [0.948, 0.007] },
  { name: "primary", light: "brand", dark: [0.786, 0.089] },
  { name: "primary-foreground", light: [1.0, 0.0], dark: [0.221, 0.035] },
  { name: "muted", light: [0.934, 0.013], dark: [0.26, 0.011] },
  { name: "muted-foreground", light: [0.481, 0.031], dark: [0.732, 0.018] },
  { name: "accent", light: [0.904, 0.031], dark: [0.359, 0.046] },
  { name: "accent-foreground", light: [0.337, 0.087], dark: [0.949, 0.023] },
  { name: "input", light: [0.874, 0.019], dark: [0.339, 0.016] },
  { name: "ring", light: "brand", dark: [0.786, 0.089] },
  { name: "success", hue: FUNCTIONAL_HUES.success, light: [0.53, 0.109], dark: [0.759, 0.156] },
  { name: "success-foreground", hue: FUNCTIONAL_HUES.success, light: [1.0, 0.0], dark: [0.247, 0.04] },
  { name: "warning", hue: FUNCTIONAL_HUES.warning, light: [0.542, 0.117], dark: [0.804, 0.146] },
  { name: "warning-foreground", hue: FUNCTIONAL_HUES.warning, light: [1.0, 0.0], dark: [0.265, 0.045] },
  { name: "danger", hue: FUNCTIONAL_HUES.danger, light: [0.501, 0.178], dark: [0.74, 0.126] },
  { name: "danger-foreground", hue: FUNCTIONAL_HUES.danger, light: [1.0, 0.0], dark: [0.245, 0.066] },
  { name: "info", hue: FUNCTIONAL_HUES.info, light: [0.468, 0.168], dark: [0.773, 0.095] },
  { name: "info-foreground", hue: FUNCTIONAL_HUES.info, light: [1.0, 0.0], dark: [0.278, 0.069] },
  { name: "ambient-violet", hue: AMBIENT_VIOLET_HUE, light: [0.786, 0.092], dark: [0.52, 0.222] },
  { name: "ambient-rose", hue: ROSE_HUE, light: [0.833, 0.061], dark: [0.639, 0.119] },
];

const SHAPE_TOKENS = [
  "  --opacity-ambient: 0.333333;",
  "  --radius-sm: 0.375rem;",
  "  --radius-md: 0.5rem;",
  "  --radius-lg: 0.75rem;",
  "  --radius-xl: 1rem;",
  "  --shadow-card: 0 1px 2px rgb(16 12 24 / 0.05), 0 8px 24px -12px rgb(16 12 24 / 0.12);",
];

function srgbToOklch(hex) {
  const r = Number.parseInt(hex.slice(1, 3), 16) / 255;
  const g = Number.parseInt(hex.slice(3, 5), 16) / 255;
  const b = Number.parseInt(hex.slice(5, 7), 16) / 255;
  const lin = (v) => (v <= 0.04045 ? v / 12.92 : ((v + 0.055) / 1.055) ** 2.4);
  const R = lin(r), G = lin(g), B = lin(b);
  const l = Math.cbrt(0.4122214708 * R + 0.5363325363 * G + 0.0514459929 * B);
  const m = Math.cbrt(0.2119034982 * R + 0.6806995451 * G + 0.1073969566 * B);
  const s = Math.cbrt(0.0883024619 * R + 0.2817188376 * G + 0.6299787005 * B);
  const L = 0.2104542553 * l + 0.793617785 * m - 0.0040720468 * s;
  const a = 1.9779984951 * l - 2.428592205 * m + 0.4505937099 * s;
  const bb = 0.0259040371 * l + 0.7827717662 * m - 0.808675766 * s;
  const c = Math.sqrt(a * a + bb * bb);
  let h = (Math.atan2(bb, a) * 180) / Math.PI;
  if (h < 0) h += 360;
  return { l: L, c, h };
}

function oklchToHex(l, c, h) {
  const rad = (h * Math.PI) / 180;
  const a = c * Math.cos(rad);
  const b = c * Math.sin(rad);
  const l_ = l + 0.3963377774 * a + 0.2158037573 * b;
  const m_ = l - 0.1055613458 * a - 0.0638541728 * b;
  const s_ = l - 0.0894841775 * a - 1.291485548 * b;
  const L = l_ ** 3, M = m_ ** 3, S = s_ ** 3;
  let r = 4.0767416621 * L - 3.3077115913 * M + 0.2309699292 * S;
  let g = -1.2684380046 * L + 2.6097574011 * M - 0.3413193965 * S;
  let b2 = -0.0041960863 * L - 0.7034186147 * M + 1.707614701 * S;
  const enc = (v) => {
    const clamped = Math.min(1, Math.max(0, v));
    return clamped <= 0.0031308 ? clamped * 12.92 : 1.055 * clamped ** (1 / 2.4) - 0.055;
  };
  const to255 = (v) => Math.round(enc(v) * 255);
  return `#${[to255(r), to255(g), to255(b2)].map((v) => v.toString(16).padStart(2, "0")).join("")}`;
}

const brand = srgbToOklch(BRAND_HEX);
const lines = [];
for (const token of TOKENS) {
  const hue = token.hue ?? brand.h;
  const light = token.light === "brand" ? [brand.l, brand.c] : token.light;
  const dark = token.dark;
  lines.push(`  --color-${token.name}: light-dark(${oklchToHex(light[0], light[1], hue)}, ${oklchToHex(dark[0], dark[1], hue)});`);
}

const block = `@theme {
  /* palette:begin — сгенерировано scripts/generate-palette.mjs из BRAND_HEX; меняй параметры генератора, не этот блок */
${lines.join("\n")}

${SHAPE_TOKENS.join("\n")}
  /* palette:end */
}`;

const path = new URL("../apps/web/src/styles/global.css", import.meta.url);
const current = await readFile(path, "utf8");
const updated = current.includes("/* palette:begin")
  ? current.replace(/@theme\s*\{[\s\S]*?\/\* palette:end \*\/\n\}/, block)
  : current.replace(/@theme\s*\{[^}]*\}/, block);
if (updated === current) {
  console.error("Не найден @theme-блок в apps/web/src/styles/global.css");
  process.exit(1);
}
await writeFile(path, updated);
console.log(`brand: ${BRAND_HEX} -> oklch(${brand.l.toFixed(3)} ${brand.c.toFixed(3)} ${brand.h.toFixed(1)})`);
for (const line of lines) {
  console.log(line);
}
console.log(`Палитра записана в apps/web/src/styles/global.css (${lines.length} цветовых токенов + ${SHAPE_TOKENS.length} токенов формы).`);
