import ts from "typescript";

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
  "text-shadow",
  "drop-shadow",
  "inset-shadow",
  "inset-ring",
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

const CSS_COLOR_PROPERTIES = new Set([
  "-webkit-text-stroke",
  "backdrop-filter",
  "background",
  "background-image",
  "border-image",
  "border-image-source",
  "box-shadow",
  "color",
  "column-rule",
  "fill",
  "filter",
  "outline",
  "scrollbar-color",
  "stroke",
  "text-decoration",
  "text-emphasis",
  "text-shadow",
]);
const CSS_BORDER_DIRECTIONS = new Set([
  "block",
  "bottom",
  "inline",
  "left",
  "right",
  "top",
]);

function isColorBearingCssProperty(property) {
  const normalized = property.toLowerCase();
  if (normalized.startsWith("--") || normalized.endsWith("-color")) return true;
  if (CSS_COLOR_PROPERTIES.has(normalized)) return true;
  const parts = normalized.split("-");
  if (parts[0] !== "border") return false;
  if (parts.length === 1) return true;
  if (parts[1] === "image") return true;
  return CSS_BORDER_DIRECTIONS.has(parts[1]) &&
    (parts.length === 2 ||
      (parts.length === 3 && (parts[2] === "start" || parts[2] === "end")));
}

const JAVASCRIPT_COLOR_PROPERTIES = new Set([
  "accentcolor",
  "backdropfilter",
  "background",
  "backgroundcolor",
  "backgroundimage",
  "border",
  "borderblock",
  "borderblockend",
  "borderblockstart",
  "borderbottom",
  "borderbottomcolor",
  "bordercolor",
  "borderimage",
  "borderimagesource",
  "borderinline",
  "borderinlineend",
  "borderinlinestart",
  "borderleft",
  "borderleftcolor",
  "borderright",
  "borderrightcolor",
  "bordertop",
  "bordertopcolor",
  "boxshadow",
  "caretcolor",
  "color",
  "fill",
  "filter",
  "floodcolor",
  "lightingcolor",
  "outline",
  "outlinecolor",
  "stopcolor",
  "stroke",
  "textdecoration",
  "textdecorationcolor",
  "textemphasis",
  "textshadow",
  "webkittextstroke",
]);

function isJavaScriptColorProperty(property) {
  if (property.startsWith("--")) return true;
  const normalized = property.replaceAll("-", "").toLowerCase();
  return JAVASCRIPT_COLOR_PROPERTIES.has(normalized);
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
  let utility = className.slice(variantEnd + 1);
  if (utility.startsWith("!")) utility = utility.slice(1);
  if (utility.endsWith("!")) utility = utility.slice(0, -1);
  return utility;
}

function arbitraryTailwindValue(value) {
  if (!value.startsWith("[")) return null;
  let depth = 0;
  let closingBracket = -1;
  for (let index = 0; index < value.length; index += 1) {
    if (value[index] === "\\") {
      index += 1;
      continue;
    }
    if (value[index] === "[") depth += 1;
    else if (value[index] === "]") {
      depth -= 1;
      if (depth === 0) {
        closingBracket = index;
        break;
      }
    }
  }
  if (closingBracket === -1) return null;
  const modifier = value.slice(closingBracket + 1);
  if (modifier && (!modifier.startsWith("/") || modifier.length === 1)) {
    return null;
  }
  return value.slice(1, closingBracket);
}

function tailwindColorMatches(content, offset) {
  const found = [];
  let start = 0;
  while (start < content.length) {
    while (start < content.length && /\s/u.test(content[start])) start += 1;
    if (start >= content.length) break;
    let end = start + 1;
    while (end < content.length && !/\s/u.test(content[end])) end += 1;
    const className = content.slice(start, end);
    const utility = classUtility(className);
    let hasLiteralColor = false;

    const arbitraryProperty = arbitraryTailwindValue(utility);
    if (arbitraryProperty !== null && utility.startsWith("[")) {
      const declaration = arbitraryProperty;
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
        const arbitraryValue = arbitraryTailwindValue(value);
        if (arbitraryValue !== null) {
          hasLiteralColor = literalColorMatches(arbitraryValue).length > 0;
        } else {
          hasLiteralColor = TAILWIND_PALETTES.has(value.split(/[-/]/u, 1)[0]);
        }
        break;
      }
    }

    if (hasLiteralColor) {
      found.push({ index: offset + start, literal: className });
    }
    start = end;
  }
  return found;
}

function scriptKind(path) {
  if (path.endsWith(".tsx")) return ts.ScriptKind.TSX;
  if (path.endsWith(".jsx")) return ts.ScriptKind.JSX;
  if (path.endsWith(".ts") || path.endsWith(".mts") || path.endsWith(".cts")) {
    return ts.ScriptKind.TS;
  }
  return ts.ScriptKind.JS;
}

function isStaticStringNode(node) {
  return ts.isStringLiteral(node) || ts.isNoSubstitutionTemplateLiteral(node) ||
    node.kind === ts.SyntaxKind.TemplateHead ||
    node.kind === ts.SyntaxKind.TemplateMiddle ||
    node.kind === ts.SyntaxKind.TemplateTail;
}

function staticStringRange(node, sourceFile, content) {
  const start = node.getStart(sourceFile);
  const headOrMiddle = node.kind === ts.SyntaxKind.TemplateHead ||
    node.kind === ts.SyntaxKind.TemplateMiddle;
  const valueStart = start + 1;
  const valueEnd = node.end - (headOrMiddle ? 2 : 1);
  return {
    content: content.slice(valueStart, valueEnd),
    offset: valueStart,
  };
}

function colorMatchesInNode(node, sourceFile, content) {
  const found = [];
  function visit(candidate) {
    if (isStaticStringNode(candidate)) {
      const range = staticStringRange(candidate, sourceFile, content);
      found.push(...literalColorMatches(range.content, range.offset));
    }
    ts.forEachChild(candidate, visit);
  }
  visit(node);
  return found;
}

function staticPropertyName(name) {
  if (
    ts.isIdentifier(name) || ts.isStringLiteral(name) ||
    ts.isNoSubstitutionTemplateLiteral(name)
  ) return name.text;
  if (ts.isComputedPropertyName(name)) {
    const expression = name.expression;
    if (
      ts.isStringLiteral(expression) ||
      ts.isNoSubstitutionTemplateLiteral(expression)
    ) return expression.text;
  }
  return null;
}

function styleColorMatches(node, sourceFile, content) {
  const found = [];
  function visit(candidate) {
    if (ts.isObjectLiteralExpression(candidate)) {
      for (const property of candidate.properties) {
        if (!ts.isPropertyAssignment(property)) continue;
        const name = staticPropertyName(property.name);
        if (name !== null && isJavaScriptColorProperty(name)) {
          found.push(
            ...colorMatchesInNode(property.initializer, sourceFile, content),
          );
        }
      }
    }
    ts.forEachChild(candidate, visit);
  }
  visit(node);
  return found;
}

function attributeValue(initializer) {
  if (!initializer) return null;
  if (ts.isJsxExpression(initializer)) return initializer.expression ?? null;
  return initializer;
}

function typescriptColorMatches(path, content) {
  const sourceFile = ts.createSourceFile(
    path,
    content,
    ts.ScriptTarget.Latest,
    true,
    scriptKind(path),
  );
  const found = [];

  function visit(node) {
    if (isStaticStringNode(node)) {
      const range = staticStringRange(node, sourceFile, content);
      found.push(...tailwindColorMatches(range.content, range.offset));
    }

    if (ts.isJsxAttribute(node) && ts.isIdentifier(node.name)) {
      const value = attributeValue(node.initializer);
      if (value !== null) {
        if (node.name.text === "style") {
          found.push(...styleColorMatches(value, sourceFile, content));
        } else if (isJavaScriptColorProperty(node.name.text)) {
          found.push(...colorMatchesInNode(value, sourceFile, content));
        }
      }
    }

    ts.forEachChild(node, visit);
  }

  visit(sourceFile);
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
      found.push(...typescriptColorMatches(path, source.content));
    }

    const unique = new Map(
      found.map((match) => [`${match.index}:${match.literal}`, match]),
    );
    const ordered = [...unique.values()].sort(
      (left, right) => left.index - right.index,
    );
    for (const match of ordered) {
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
