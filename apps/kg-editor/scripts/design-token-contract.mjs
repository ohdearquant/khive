const COLOR_FUNCTION =
  /\b(?:rgb|rgba|hsl|hsla|hwb|lab|lch|oklab|oklch|color)\([^)]*\)/giu;
const HEX_COLOR = /#[\da-f]{3,8}\b/giu;
const NAMED_COLOR = /(?<![-\w])(?:black|white|transparent)(?![-\w])/giu;
const TAILWIND_PALETTE =
  /\b(?:bg|text|border|outline|ring|shadow|fill|stroke|from|via|to)-(?:slate|gray|zinc|neutral|stone|red|orange|amber|yellow|lime|green|emerald|teal|cyan|sky|blue|indigo|violet|purple|fuchsia|pink|rose|black|white)(?:-\d{2,3})?(?:\/\d{1,3})?\b/giu;
const TAILWIND_ARBITRARY_COLOR =
  /\b(?:bg|text|border|outline|ring|shadow|fill|stroke|from|via|to)-\[(?:#[\da-f]{3,8}|(?:rgb|rgba|hsl|hsla|hwb|lab|lch|oklab|oklch|color)\([^\]]+\))\]/giu;

function lineAt(content, index) {
  return content.slice(0, index).split("\n").length;
}

function matches(content, pattern, offset = 0) {
  return Array.from(content.matchAll(pattern), (match) => ({
    index: offset + match.index,
    literal: match[0],
  }));
}

function stringLiteralRanges(content) {
  const ranges = [];
  const strings = [
    /"((?:\\.|[^"\\])*)"/gsu,
    /'((?:\\.|[^'\\])*)'/gsu,
    /`((?:\\.|[^`\\])*)`/gsu,
  ];
  for (const pattern of strings) {
    for (const match of content.matchAll(pattern)) {
      ranges.push({ content: match[1], offset: match.index + 1 });
    }
  }
  return ranges;
}

export function findLiteralColorViolations(
  sources,
  tokenLayer = "src/app/tokens.css",
) {
  const violations = [];

  for (const source of sources) {
    const path = source.path.replaceAll("\\", "/");
    if (path === tokenLayer || path.endsWith(`/${tokenLayer}`)) continue;

    let found = [];
    if (path.endsWith(".css")) {
      found = [
        ...matches(source.content, COLOR_FUNCTION),
        ...matches(source.content, HEX_COLOR),
        ...matches(source.content, NAMED_COLOR),
      ];
    } else if (/\.[cm]?[jt]sx?$/u.test(path)) {
      for (const range of stringLiteralRanges(source.content)) {
        found.push(...matches(range.content, TAILWIND_PALETTE, range.offset));
        found.push(
          ...matches(range.content, TAILWIND_ARBITRARY_COLOR, range.offset),
        );
      }
    }

    found.sort((left, right) => left.index - right.index);
    for (const match of found) {
      violations.push({
        path,
        line: lineAt(source.content, match.index),
        literal: match.literal,
      });
    }
  }

  return violations;
}

function channel(value) {
  const trimmed = value.trim();
  if (trimmed.endsWith("%")) return Number.parseFloat(trimmed) * 2.55;
  return Number.parseFloat(trimmed);
}

function alpha(value = "1") {
  const trimmed = value.trim();
  if (trimmed.endsWith("%")) return Number.parseFloat(trimmed) / 100;
  return Number.parseFloat(trimmed);
}

export function parseCssColor(value) {
  const normalized = value.trim().toLowerCase();
  if (normalized.startsWith("#")) {
    const raw = normalized.slice(1);
    const expanded = raw.length <= 4
      ? Array.from(raw, (part) => `${part}${part}`).join("")
      : raw;
    if (expanded.length !== 6 && expanded.length !== 8) {
      throw new Error(`unsupported hex color: ${value}`);
    }
    return {
      red: Number.parseInt(expanded.slice(0, 2), 16),
      green: Number.parseInt(expanded.slice(2, 4), 16),
      blue: Number.parseInt(expanded.slice(4, 6), 16),
      alpha: expanded.length === 8
        ? Number.parseInt(expanded.slice(6, 8), 16) / 255
        : 1,
    };
  }

  const rgb = normalized.match(/^rgba?\((.*)\)$/u);
  if (!rgb) throw new Error(`unsupported CSS color: ${value}`);
  let [channels, alphaValue] = rgb[1].split("/").map((part) => part.trim());
  const parts = channels.includes(",")
    ? channels.split(",").map((part) => part.trim())
    : channels.split(/\s+/u);
  if (parts.length === 4 && alphaValue === undefined) alphaValue = parts.pop();
  if (parts.length !== 3) throw new Error(`invalid rgb color: ${value}`);
  return {
    red: channel(parts[0]),
    green: channel(parts[1]),
    blue: channel(parts[2]),
    alpha: alpha(alphaValue),
  };
}

export function composite(surface, foreground) {
  return {
    red: foreground.red * foreground.alpha +
      surface.red * (1 - foreground.alpha),
    green: foreground.green * foreground.alpha +
      surface.green * (1 - foreground.alpha),
    blue: foreground.blue * foreground.alpha +
      surface.blue * (1 - foreground.alpha),
    alpha: 1,
  };
}

export function alphaEquivalent(surface, primary, candidate) {
  const primaryComposite = composite(surface, primary);
  const candidateComposite = composite(surface, candidate);
  const primaryVector = [
    primaryComposite.red - surface.red,
    primaryComposite.green - surface.green,
    primaryComposite.blue - surface.blue,
  ];
  const candidateVector = [
    candidateComposite.red - surface.red,
    candidateComposite.green - surface.green,
    candidateComposite.blue - surface.blue,
  ];
  const denominator = primaryVector.reduce((sum, part) => sum + part * part, 0);
  if (denominator === 0) {
    throw new Error("primary text is indistinguishable from its surface");
  }
  const numerator = candidateVector.reduce(
    (sum, part, index) => sum + part * primaryVector[index],
    0,
  );
  return numerator / denominator;
}

function linearChannel(value) {
  const normalized = value / 255;
  return normalized <= 0.04045
    ? normalized / 12.92
    : ((normalized + 0.055) / 1.055) ** 2.4;
}

export function contrastRatio(first, second) {
  const luminance = (color) =>
    0.2126 * linearChannel(color.red) +
    0.7152 * linearChannel(color.green) +
    0.0722 * linearChannel(color.blue);
  const firstLuminance = luminance(first);
  const secondLuminance = luminance(second);
  const lighter = Math.max(firstLuminance, secondLuminance);
  const darker = Math.min(firstLuminance, secondLuminance);
  return (lighter + 0.05) / (darker + 0.05);
}
