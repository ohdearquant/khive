const COLOR_FUNCTION =
  /\b(?:rgb|rgba|hsl|hsla|hwb|lab|lch|oklab|oklch|color)\([^)]*\)/giu;
const HEX_COLOR = /#[\da-f]{3,8}\b/giu;
const COLOR_IDENTIFIER = /(?<![-\w])[a-z]+(?![-\w])/giu;
const CSS_NAMED_COLORS = new Set(
  `aliceblue antiquewhite aqua aquamarine azure beige bisque black blanchedalmond blue blueviolet brown burlywood cadetblue chartreuse chocolate coral cornflowerblue cornsilk crimson cyan darkblue darkcyan darkgoldenrod darkgray darkgreen darkgrey darkkhaki darkmagenta darkolivegreen darkorange darkorchid darkred darksalmon darkseagreen darkslateblue darkslategray darkslategrey darkturquoise darkviolet deeppink deepskyblue dimgray dimgrey dodgerblue firebrick floralwhite forestgreen fuchsia gainsboro ghostwhite gold goldenrod gray green greenyellow grey honeydew hotpink indianred indigo ivory khaki lavender lavenderblush lawngreen lemonchiffon lightblue lightcoral lightcyan lightgoldenrodyellow lightgray lightgreen lightgrey lightpink lightsalmon lightseagreen lightskyblue lightslategray lightslategrey lightsteelblue lightyellow lime limegreen linen magenta maroon mediumaquamarine mediumblue mediumorchid mediumpurple mediumseagreen mediumslateblue mediumspringgreen mediumturquoise mediumvioletred midnightblue mintcream mistyrose moccasin navajowhite navy oldlace olive olivedrab orange orangered orchid palegoldenrod palegreen paleturquoise palevioletred papayawhip peachpuff peru pink plum powderblue purple rebeccapurple red rosybrown royalblue saddlebrown salmon sandybrown seagreen seashell sienna silver skyblue slateblue slategray slategrey snow springgreen steelblue tan teal thistle tomato transparent turquoise violet wheat white whitesmoke yellow yellowgreen`
    .split(" "),
);
const TAILWIND_PALETTES = new Set(
  `slate gray zinc neutral stone red orange amber yellow lime green emerald teal cyan sky blue indigo violet purple fuchsia pink rose black white`
    .split(" "),
);
const TAILWIND_COLOR_PREFIXES = [
  "ring-offset",
  "border-block-start",
  "border-block-end",
  "border-inline-start",
  "border-inline-end",
  "border-x",
  "border-y",
  "border-t",
  "border-r",
  "border-b",
  "border-l",
  "border-s",
  "border-e",
  "divide-x",
  "divide-y",
  "decoration",
  "placeholder",
  "accent",
  "caret",
  "divide",
  "outline",
  "border",
  "shadow",
  "stroke",
  "fill",
  "ring",
  "from",
  "via",
  "to",
  "text",
  "bg",
];

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
      property: property.toLowerCase(),
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

function isColorBearingCssProperty(property) {
  const normalized = property.toLowerCase();
  if (normalized.startsWith("--") || normalized.endsWith("-color")) return true;
  if (
    /^(?:color|fill|stroke|background(?:-image)?|outline|box-shadow|text-shadow|text-decoration|column-rule|scrollbar-color|border-image|filter|backdrop-filter)$/u
      .test(normalized)
  ) return true;
  return /^border(?:-(?:top|right|bottom|left|block|inline)(?:-(?:start|end))?)?$/u
    .test(normalized);
}

function isJavaScriptColorProperty(property) {
  const normalized = property.replaceAll("-", "").toLowerCase();
  return normalized.startsWith("--") ||
    /^(?:color|fill|stroke|background|backgroundcolor|bordertopcolor|borderrightcolor|borderbottomcolor|borderleftcolor|bordercolor|outlinecolor|textdecorationcolor|caretcolor|accentcolor|stopcolor|floodcolor|lightingcolor|boxshadow|textshadow)$/u
      .test(normalized);
}

function tokenizeJavaScript(content) {
  const tokens = [];

  function push(type, value, start, end) {
    tokens.push({ type, value, start, end });
  }

  function scanString(index, quote) {
    const start = index + 1;
    index += 1;
    while (index < content.length && content[index] !== quote) {
      if (content[index] === "\\") index += 1;
      index += 1;
    }
    push("string", content.slice(start, index), start, index);
    return Math.min(content.length, index + 1);
  }

  function scanTemplate(index) {
    index += 1;
    let chunkStart = index;
    while (index < content.length) {
      if (content[index] === "\\") {
        index += 2;
        continue;
      }
      if (content[index] === "`") {
        if (index > chunkStart) {
          push("string", content.slice(chunkStart, index), chunkStart, index);
        }
        return index + 1;
      }
      if (content[index] === "$" && content[index + 1] === "{") {
        if (index > chunkStart) {
          push("string", content.slice(chunkStart, index), chunkStart, index);
        }
        push("punctuation", "{", index + 1, index + 2);
        index = scanCode(index + 2, true);
        chunkStart = index;
        continue;
      }
      index += 1;
    }
    if (index > chunkStart) {
      push("string", content.slice(chunkStart, index), chunkStart, index);
    }
    return index;
  }

  function scanCode(index, stopAtClosingBrace = false) {
    while (index < content.length) {
      const character = content[index];
      if (/\s/u.test(character)) {
        index += 1;
        continue;
      }
      if (character === "/" && content[index + 1] === "/") {
        index += 2;
        while (index < content.length && content[index] !== "\n") index += 1;
        continue;
      }
      if (character === "/" && content[index + 1] === "*") {
        index += 2;
        while (
          index < content.length &&
          !(content[index] === "*" && content[index + 1] === "/")
        ) index += 1;
        index = Math.min(content.length, index + 2);
        continue;
      }
      if (character === '"' || character === "'") {
        index = scanString(index, character);
        continue;
      }
      if (character === "`") {
        index = scanTemplate(index);
        continue;
      }
      if (character === "{") {
        push("punctuation", character, index, index + 1);
        index = scanCode(index + 1, true);
        continue;
      }
      if (character === "}" && stopAtClosingBrace) {
        push("punctuation", character, index, index + 1);
        return index + 1;
      }
      if (/[a-z_$]/iu.test(character)) {
        const start = index;
        index += 1;
        while (/[\w$]/u.test(content[index] ?? "")) index += 1;
        push("identifier", content.slice(start, index), start, index);
        continue;
      }
      push("punctuation", character, index, index + 1);
      index += 1;
    }
    return index;
  }

  scanCode(0);
  return tokens;
}

function matchingToken(tokens, start, opening, closing, limit = tokens.length) {
  let depth = 0;
  for (let index = start; index < limit; index += 1) {
    if (tokens[index].value === opening) depth += 1;
    else if (tokens[index].value === closing) {
      depth -= 1;
      if (depth === 0) return index;
    }
  }
  return -1;
}

function classUtility(className) {
  let squareDepth = 0;
  let variantEnd = -1;
  for (let index = 0; index < className.length; index += 1) {
    if (className[index] === "[") squareDepth += 1;
    else if (className[index] === "]") {
      squareDepth = Math.max(0, squareDepth - 1);
    } else if (className[index] === ":" && squareDepth === 0) {
      variantEnd = index;
    }
  }
  return className.slice(variantEnd + 1).replace(/^!/u, "");
}

function tailwindColorMatches(content, offset) {
  const found = [];
  for (const match of content.matchAll(/\S+/gu)) {
    const className = match[0];
    const utility = classUtility(className);
    let hasLiteralColor = false;

    if (utility.startsWith("[") && utility.endsWith("]")) {
      const declaration = utility.slice(1, -1);
      const colon = declaration.indexOf(":");
      if (colon > 0 && isColorBearingCssProperty(declaration.slice(0, colon))) {
        hasLiteralColor =
          literalColorMatches(declaration.slice(colon + 1)).length > 0;
      }
    } else {
      for (const prefix of TAILWIND_COLOR_PREFIXES) {
        const marker = `${prefix}-`;
        if (!utility.startsWith(marker)) continue;
        const value = utility.slice(marker.length);
        if (value.startsWith("[") && value.endsWith("]")) {
          hasLiteralColor = literalColorMatches(value.slice(1, -1)).length > 0;
        } else {
          hasLiteralColor = TAILWIND_PALETTES.has(value.split(/[-/]/u, 1)[0]);
        }
        break;
      }
    }

    if (hasLiteralColor) {
      found.push({ index: offset + match.index, literal: className });
    }
  }
  return found;
}

function colorMatchesInTokens(tokens, start, end) {
  const found = [];
  for (let index = start; index < end; index += 1) {
    const token = tokens[index];
    if (token.type !== "string") continue;
    found.push(...literalColorMatches(token.value, token.start));
  }
  return found;
}

function tailwindMatchesInTokens(tokens, start, end) {
  const found = [];
  for (let index = start; index < end; index += 1) {
    const token = tokens[index];
    if (token.type !== "string") continue;
    found.push(...tailwindColorMatches(token.value, token.start));
  }
  return found;
}

function styleColorMatches(tokens, start, end) {
  const found = [];
  for (let index = start; index < end - 1; index += 1) {
    const key = tokens[index];
    if (
      (key.type !== "identifier" && key.type !== "string") ||
      tokens[index + 1].value !== ":" ||
      !isJavaScriptColorProperty(key.value)
    ) continue;

    let cursor = index + 2;
    let braces = 0;
    let brackets = 0;
    let parentheses = 0;
    while (cursor < end) {
      const value = tokens[cursor].value;
      if (value === "{") braces += 1;
      else if (value === "}") {
        if (braces === 0 && brackets === 0 && parentheses === 0) break;
        braces -= 1;
      } else if (value === "[") brackets += 1;
      else if (value === "]") brackets -= 1;
      else if (value === "(") parentheses += 1;
      else if (value === ")") parentheses -= 1;
      else if (
        value === "," && braces === 0 && brackets === 0 && parentheses === 0
      ) break;
      cursor += 1;
    }
    found.push(...colorMatchesInTokens(tokens, index + 2, cursor));
    index = cursor - 1;
  }
  return found;
}

function jsxColorMatches(content) {
  const tokens = tokenizeJavaScript(content);
  const found = [];
  for (let open = 0; open < tokens.length; open += 1) {
    if (tokens[open].value !== "<") continue;
    const first = tokens[open + 1];
    if (!first || (first.type !== "identifier" && first.value !== ">")) {
      continue;
    }

    let close = open + 1;
    let braces = 0;
    for (; close < tokens.length; close += 1) {
      const value = tokens[close].value;
      if (value === "{") braces += 1;
      else if (value === "}") braces = Math.max(0, braces - 1);
      else if (value === ">" && braces === 0) break;
    }
    if (close >= tokens.length) continue;

    braces = 0;
    for (let index = open + 2; index < close; index += 1) {
      const token = tokens[index];
      if (token.value === "{") {
        braces += 1;
        continue;
      }
      if (token.value === "}") {
        braces = Math.max(0, braces - 1);
        continue;
      }
      if (
        braces !== 0 || token.type !== "identifier" ||
        tokens[index + 1]?.value !== "="
      ) continue;

      const attribute = token.value;
      const valueStart = index + 2;
      let valueEnd = valueStart + 1;
      if (tokens[valueStart]?.value === "{") {
        const matching = matchingToken(tokens, valueStart, "{", "}", close);
        if (matching === -1) continue;
        valueEnd = matching;
      }
      if (attribute === "className") {
        found.push(...tailwindMatchesInTokens(tokens, valueStart, valueEnd));
      } else if (attribute === "style") {
        found.push(...styleColorMatches(tokens, valueStart, valueEnd));
      } else if (isJavaScriptColorProperty(attribute)) {
        found.push(...colorMatchesInTokens(tokens, valueStart, valueEnd));
      }
      index = valueEnd;
    }
    open = close;
  }
  return found;
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
        if (!isColorBearingCssProperty(range.property)) continue;
        found.push(...literalColorMatches(range.content, range.offset, true));
      }
    } else if (/\.[cm]?[jt]sx?$/u.test(path)) {
      found.push(...jsxColorMatches(source.content));
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
