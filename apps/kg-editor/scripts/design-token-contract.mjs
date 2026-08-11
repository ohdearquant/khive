const COLOR_FUNCTION =
  /\b(?:rgb|rgba|hsl|hsla|hwb|lab|lch|oklab|oklch|color)\([^)]*\)/giu;
const HEX_COLOR = /#[\da-f]{3,8}\b/giu;
const COLOR_IDENTIFIER = /(?<![-\w])[a-z]+(?![-\w])/giu;
const CSS_NAMED_COLORS = new Set(
  `aliceblue antiquewhite aqua aquamarine azure beige bisque black blanchedalmond blue blueviolet brown burlywood cadetblue chartreuse chocolate coral cornflowerblue cornsilk crimson cyan darkblue darkcyan darkgoldenrod darkgray darkgreen darkgrey darkkhaki darkmagenta darkolivegreen darkorange darkorchid darkred darksalmon darkseagreen darkslateblue darkslategray darkslategrey darkturquoise darkviolet deeppink deepskyblue dimgray dimgrey dodgerblue firebrick floralwhite forestgreen fuchsia gainsboro ghostwhite gold goldenrod gray green greenyellow grey honeydew hotpink indianred indigo ivory khaki lavender lavenderblush lawngreen lemonchiffon lightblue lightcoral lightcyan lightgoldenrodyellow lightgray lightgreen lightgrey lightpink lightsalmon lightseagreen lightskyblue lightslategray lightslategrey lightsteelblue lightyellow lime limegreen linen magenta maroon mediumaquamarine mediumblue mediumorchid mediumpurple mediumseagreen mediumslateblue mediumspringgreen mediumturquoise mediumvioletred midnightblue mintcream mistyrose moccasin navajowhite navy oldlace olive olivedrab orange orangered orchid palegoldenrod palegreen paleturquoise palevioletred papayawhip peachpuff peru pink plum powderblue purple rebeccapurple red rosybrown royalblue saddlebrown salmon sandybrown seagreen seashell sienna silver skyblue slateblue slategray slategrey snow springgreen steelblue tan teal thistle tomato transparent turquoise violet wheat white whitesmoke yellow yellowgreen`
    .split(" "),
);
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

function maskCssComments(content) {
  const masked = Array.from(content);
  let quote = null;
  for (let index = 0; index < masked.length; index += 1) {
    const character = masked[index];
    if (quote) {
      if (character === "\\") index += 1;
      else if (character === quote) quote = null;
      continue;
    }
    if (character === '"' || character === "'") {
      quote = character;
      continue;
    }
    if (character !== "/" || masked[index + 1] !== "*") continue;
    masked[index] = " ";
    masked[index + 1] = " ";
    index += 2;
    while (index < masked.length) {
      if (masked[index] === "*" && masked[index + 1] === "/") {
        masked[index] = " ";
        masked[index + 1] = " ";
        index += 1;
        break;
      }
      if (masked[index] !== "\n" && masked[index] !== "\r") masked[index] = " ";
      index += 1;
    }
  }
  return masked.join("");
}

function maskQuotedStrings(content) {
  const masked = Array.from(content);
  let quote = null;
  for (let index = 0; index < masked.length; index += 1) {
    const character = masked[index];
    if (!quote) {
      if (character === '"' || character === "'") {
        quote = character;
        masked[index] = " ";
      }
      continue;
    }
    if (character === "\\") {
      masked[index] = " ";
      if (index + 1 < masked.length && masked[index + 1] !== "\n") {
        masked[index + 1] = " ";
        index += 1;
      }
    } else {
      if (character === quote) quote = null;
      if (character !== "\n" && character !== "\r") masked[index] = " ";
    }
  }
  return masked.join("");
}

function cssDeclarationRanges(content) {
  const clean = maskCssComments(content);
  const ranges = [];
  let depth = 0;
  let segmentStart = 0;
  let index = 0;

  while (index < clean.length) {
    const character = clean[index];
    if (character === "{") {
      depth += 1;
      segmentStart = index + 1;
      index += 1;
      continue;
    }
    if (character === "}") {
      depth = Math.max(0, depth - 1);
      segmentStart = index + 1;
      index += 1;
      continue;
    }
    if (character === ";") {
      segmentStart = index + 1;
      index += 1;
      continue;
    }
    if (character !== ":" || depth === 0) {
      index += 1;
      continue;
    }

    const property = clean.slice(segmentStart, index).trim();
    if (!/^-{0,2}[a-z][a-z\d-]*$/iu.test(property)) {
      index += 1;
      continue;
    }

    let valueStart = index + 1;
    while (/\s/u.test(clean[valueStart] ?? "")) valueStart += 1;
    let cursor = valueStart;
    let parentheses = 0;
    let quote = null;
    while (cursor < clean.length) {
      const valueCharacter = clean[cursor];
      if (quote) {
        if (valueCharacter === "\\") cursor += 1;
        else if (valueCharacter === quote) quote = null;
      } else if (valueCharacter === '"' || valueCharacter === "'") {
        quote = valueCharacter;
      } else if (valueCharacter === "(") {
        parentheses += 1;
      } else if (valueCharacter === ")") {
        parentheses = Math.max(0, parentheses - 1);
      } else if (
        parentheses === 0 && (valueCharacter === ";" || valueCharacter === "}")
      ) {
        break;
      }
      cursor += 1;
    }
    ranges.push({
      content: clean.slice(valueStart, cursor),
      offset: valueStart,
    });
    if (clean[cursor] === "}") depth = Math.max(0, depth - 1);
    segmentStart = cursor + 1;
    index = cursor + 1;
  }

  return ranges;
}

function literalColorMatches(content, offset = 0, maskStrings = false) {
  const inspected = maskStrings ? maskQuotedStrings(content) : content;
  const found = [
    ...matches(inspected, COLOR_FUNCTION, offset),
    ...matches(inspected, HEX_COLOR, offset),
  ];
  for (const match of inspected.matchAll(COLOR_IDENTIFIER)) {
    if (!CSS_NAMED_COLORS.has(match[0].toLowerCase())) continue;
    found.push({ index: offset + match.index, literal: match[0] });
  }
  return found;
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

function javascriptColorRanges(content) {
  const ranges = [];
  const property =
    "(?:color|background(?:Color)?|border(?:Top|Right|Bottom|Left)?Color|outlineColor|textDecorationColor|caretColor|accentColor|fill|stroke|stopColor|floodColor|lightingColor|boxShadow|textShadow)";
  const patterns = [
    new RegExp(`\\b${property}\\s*:\\s*"((?:\\\\.|[^"\\\\])*)"`, "gsu"),
    new RegExp(`\\b${property}\\s*:\\s*'((?:\\\\.|[^'\\\\])*)'`, "gsu"),
    new RegExp("\\b" + property + "\\s*:\\s*`((?:\\\\.|[^`\\\\])*)`", "gsu"),
    new RegExp(`\\b${property}\\s*=\\s*"((?:\\\\.|[^"\\\\])*)"`, "gsu"),
    new RegExp(`\\b${property}\\s*=\\s*'((?:\\\\.|[^'\\\\])*)'`, "gsu"),
    new RegExp(
      `\\b${property}\\s*=\\s*\\{\\s*"((?:\\\\.|[^"\\\\])*)"\\s*\\}`,
      "gsu",
    ),
    new RegExp(
      `\\b${property}\\s*=\\s*\\{\\s*'((?:\\\\.|[^'\\\\])*)'\\s*\\}`,
      "gsu",
    ),
  ];
  for (const pattern of patterns) {
    for (const match of content.matchAll(pattern)) {
      const value = match[1] ?? "";
      ranges.push({
        content: value,
        offset: match.index + match[0].indexOf(value),
      });
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
      for (const range of cssDeclarationRanges(source.content)) {
        found.push(...literalColorMatches(range.content, range.offset, true));
      }
    } else if (/\.[cm]?[jt]sx?$/u.test(path)) {
      for (const range of stringLiteralRanges(source.content)) {
        found.push(...matches(range.content, TAILWIND_PALETTE, range.offset));
        found.push(
          ...matches(range.content, TAILWIND_ARBITRARY_COLOR, range.offset),
        );
      }
      for (const range of javascriptColorRanges(source.content)) {
        found.push(...literalColorMatches(range.content, range.offset));
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

function finiteNumber(value, label) {
  const trimmed = value.trim();
  const numeric = trimmed.endsWith("%") ? trimmed.slice(0, -1) : trimmed;
  if (!/^[+-]?(?:\d+(?:\.\d*)?|\.\d+)(?:e[+-]?\d+)?$/iu.test(numeric)) {
    throw new Error(`invalid ${label}: ${value}`);
  }
  const parsed = Number(numeric);
  if (!Number.isFinite(parsed)) throw new Error(`invalid ${label}: ${value}`);
  return { parsed, percentage: trimmed.endsWith("%") };
}

function clamp(value, minimum, maximum) {
  return Math.min(maximum, Math.max(minimum, value));
}

function channel(value) {
  const numeric = finiteNumber(value, "rgb channel");
  const scaled = numeric.percentage ? numeric.parsed * 2.55 : numeric.parsed;
  return clamp(scaled, 0, 255);
}

function alpha(value = "1") {
  const numeric = finiteNumber(value, "alpha channel");
  const scaled = numeric.percentage ? numeric.parsed / 100 : numeric.parsed;
  return clamp(scaled, 0, 1);
}

export function parseCssColor(value) {
  const normalized = value.trim().toLowerCase();
  if (normalized.startsWith("#")) {
    const raw = normalized.slice(1);
    if (!/^(?:[\da-f]{3,4}|[\da-f]{6}|[\da-f]{8})$/u.test(raw)) {
      throw new Error(`invalid hex color: ${value}`);
    }
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
  const colorParts = rgb[1].split("/").map((part) => part.trim());
  if (colorParts.length > 2) throw new Error(`invalid rgb color: ${value}`);
  let [channels, alphaValue] = colorParts;
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
  if (surface.alpha !== 1) throw new Error("surface token must be opaque");
  if (primary.alpha !== 1) throw new Error("primary text token must be opaque");
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
