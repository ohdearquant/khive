import { describe, expect, it } from "vitest";

import {
  deduplicateSettledPositions,
  settleGraphLayout,
} from "@/lib/graph-layout";
import { demoReviewFixture } from "@/lib/fixtures/demo-review";

describe("settleGraphLayout", () => {
  it("settles the golden review graph independently of input order", () => {
    const nodes = demoReviewFixture.graph.nodes.items;
    const edges = demoReviewFixture.graph.edges.items;
    const settled = settleGraphLayout(nodes, edges).map(({ id, x, y }) => ({
      id,
      x,
      y,
    }));

    expect(
      settleGraphLayout([...nodes].reverse(), [...edges].reverse()).map(
        ({ id, x, y }) => ({ id, x, y }),
      ),
    )
      .toEqual(settled);
    expect(settled).toMatchInlineSnapshot(`
      [
        {
          "id": "7a66357c-8bd4-4fb8-b822-8ded7232ab31",
          "x": 77,
          "y": 86.327,
        },
        {
          "id": "a1f00000-0000-4000-8000-000000000001",
          "x": 64.679,
          "y": 46.634,
        },
        {
          "id": "a1f00000-0000-4000-8000-000000000002",
          "x": 77,
          "y": 10,
        },
        {
          "id": "c2d00000-0000-4000-8000-000000000030",
          "x": 23,
          "y": 79.157,
        },
        {
          "id": "d0b00000-0000-4000-8000-000000000021",
          "x": 23,
          "y": 38.517,
        },
      ]
    `);
  });

  it("keeps mobile node footprints inside the clipped stage", () => {
    const stageWidth = 300;
    const nodeHalfWidth = 60;
    const settled = settleGraphLayout(
      Array.from({ length: 20 }, (_, index) => ({ id: `node-${index}` })),
      [],
    );

    for (const node of settled) {
      const center = node.x / 100 * stageWidth;
      expect(center - nodeHalfWidth).toBeGreaterThanOrEqual(0);
      expect(center + nodeHalfWidth).toBeLessThanOrEqual(stageWidth);
    }
  });

  it("bounds pair-distance work for a schema-maximum graph", () => {
    const originalSqrt = Math.sqrt;
    const originalHypot = Math.hypot;
    let distanceEvaluations = 0;
    Math.sqrt = (value) => {
      distanceEvaluations += 1;
      return originalSqrt(value);
    };
    Math.hypot = (...values) => {
      distanceEvaluations += 1;
      return originalHypot(...values);
    };

    try {
      settleGraphLayout(
        Array.from({ length: 200 }, (_, index) => ({ id: `node-${index}` })),
        [],
      );
    } finally {
      Math.sqrt = originalSqrt;
      Math.hypot = originalHypot;
    }

    expect(distanceEvaluations).toBeLessThanOrEqual(400_000);
  }, 15_000);

  it("never settles two nodes onto the same point at the schema maximum", () => {
    const settled = settleGraphLayout(
      Array.from({ length: 200 }, (_, index) => ({ id: `node-${index}` })),
      [],
    );

    const occupied = new Set<string>();
    for (const { x, y } of settled) {
      const key = `${x},${y}`;
      expect(occupied.has(key)).toBe(false);
      occupied.add(key);
    }

    // None of these 200 points ever exhausts the golden-angle spiral, so
    // this pins the exact output and catches any regression the exhaustion
    // fallback in deduplicateSettledPositions might otherwise introduce for
    // the common case.
    expect(settled).toMatchInlineSnapshot(`
      [
        {
          "id": "node-0",
          "x": 23,
          "y": 90,
        },
        {
          "id": "node-1",
          "x": 77,
          "y": 15.264,
        },
        {
          "id": "node-10",
          "x": 33.791,
          "y": 83.045,
        },
        {
          "id": "node-100",
          "x": 30.584,
          "y": 68.564,
        },
        {
          "id": "node-101",
          "x": 62.718,
          "y": 16.766,
        },
        {
          "id": "node-102",
          "x": 77,
          "y": 75.783,
        },
        {
          "id": "node-103",
          "x": 61.32,
          "y": 80.763,
        },
        {
          "id": "node-104",
          "x": 35.194,
          "y": 82.316,
        },
        {
          "id": "node-105",
          "x": 56.363,
          "y": 22.987,
        },
        {
          "id": "node-106",
          "x": 42.041,
          "y": 10,
        },
        {
          "id": "node-107",
          "x": 26.125,
          "y": 10,
        },
        {
          "id": "node-108",
          "x": 62.636,
          "y": 70.672,
        },
        {
          "id": "node-109",
          "x": 49.63,
          "y": 89.519,
        },
        {
          "id": "node-11",
          "x": 43.25,
          "y": 23.049,
        },
        {
          "id": "node-110",
          "x": 77,
          "y": 79.266,
        },
        {
          "id": "node-111",
          "x": 23,
          "y": 20.463,
        },
        {
          "id": "node-112",
          "x": 29.2,
          "y": 90,
        },
        {
          "id": "node-113",
          "x": 67.424,
          "y": 47.051,
        },
        {
          "id": "node-114",
          "x": 77,
          "y": 20.573,
        },
        {
          "id": "node-115",
          "x": 40.874,
          "y": 10,
        },
        {
          "id": "node-116",
          "x": 23,
          "y": 68.354,
        },
        {
          "id": "node-117",
          "x": 64.125,
          "y": 16.787,
        },
        {
          "id": "node-118",
          "x": 77,
          "y": 63.787,
        },
        {
          "id": "node-119",
          "x": 23,
          "y": 88.566,
        },
        {
          "id": "node-12",
          "x": 53.122,
          "y": 79.991,
        },
        {
          "id": "node-120",
          "x": 51.477,
          "y": 10,
        },
        {
          "id": "node-121",
          "x": 54.898,
          "y": 10,
        },
        {
          "id": "node-122",
          "x": 34.337,
          "y": 82.642,
        },
        {
          "id": "node-123",
          "x": 75.895,
          "y": 37.389,
        },
        {
          "id": "node-124",
          "x": 52.008,
          "y": 50.682,
        },
        {
          "id": "node-125",
          "x": 26.974,
          "y": 47.691,
        },
        {
          "id": "node-126",
          "x": 29.013,
          "y": 21.812,
        },
        {
          "id": "node-127",
          "x": 48.555,
          "y": 12.113,
        },
        {
          "id": "node-128",
          "x": 77,
          "y": 10,
        },
        {
          "id": "node-129",
          "x": 63.684,
          "y": 18.409,
        },
        {
          "id": "node-13",
          "x": 77,
          "y": 77.11,
        },
        {
          "id": "node-130",
          "x": 77,
          "y": 90,
        },
        {
          "id": "node-131",
          "x": 62.801,
          "y": 82.58,
        },
        {
          "id": "node-132",
          "x": 23,
          "y": 10.691,
        },
        {
          "id": "node-133",
          "x": 52.059,
          "y": 90,
        },
        {
          "id": "node-134",
          "x": 23,
          "y": 79.429,
        },
        {
          "id": "node-135",
          "x": 27.448,
          "y": 10,
        },
        {
          "id": "node-136",
          "x": 77,
          "y": 15.732,
        },
        {
          "id": "node-137",
          "x": 23,
          "y": 89.502,
        },
        {
          "id": "node-138",
          "x": 77,
          "y": 64.337,
        },
        {
          "id": "node-139",
          "x": 61.235,
          "y": 73.027,
        },
        {
          "id": "node-14",
          "x": 77,
          "y": 34.218,
        },
        {
          "id": "node-140",
          "x": 77,
          "y": 75.955,
        },
        {
          "id": "node-141",
          "x": 23,
          "y": 15.6,
        },
        {
          "id": "node-142",
          "x": 35.547,
          "y": 90,
        },
        {
          "id": "node-143",
          "x": 45.726,
          "y": 87.875,
        },
        {
          "id": "node-144",
          "x": 44.438,
          "y": 21.265,
        },
        {
          "id": "node-145",
          "x": 73.668,
          "y": 90,
        },
        {
          "id": "node-146",
          "x": 70.554,
          "y": 65.043,
        },
        {
          "id": "node-147",
          "x": 65.492,
          "y": 17.854,
        },
        {
          "id": "node-148",
          "x": 67.082,
          "y": 90,
        },
        {
          "id": "node-149",
          "x": 27.991,
          "y": 22.097,
        },
        {
          "id": "node-15",
          "x": 77,
          "y": 14.635,
        },
        {
          "id": "node-150",
          "x": 23,
          "y": 79.727,
        },
        {
          "id": "node-151",
          "x": 39.144,
          "y": 10,
        },
        {
          "id": "node-152",
          "x": 77,
          "y": 80.301,
        },
        {
          "id": "node-153",
          "x": 28.139,
          "y": 10,
        },
        {
          "id": "node-154",
          "x": 62.049,
          "y": 83.166,
        },
        {
          "id": "node-155",
          "x": 35.314,
          "y": 75.012,
        },
        {
          "id": "node-156",
          "x": 40.225,
          "y": 10,
        },
        {
          "id": "node-157",
          "x": 63.568,
          "y": 82.403,
        },
        {
          "id": "node-158",
          "x": 33.782,
          "y": 82.437,
        },
        {
          "id": "node-159",
          "x": 27.021,
          "y": 21.894,
        },
        {
          "id": "node-16",
          "x": 76.561,
          "y": 89.76,
        },
        {
          "id": "node-160",
          "x": 30.34,
          "y": 33.442,
        },
        {
          "id": "node-161",
          "x": 69.145,
          "y": 10,
        },
        {
          "id": "node-162",
          "x": 69.037,
          "y": 69.936,
        },
        {
          "id": "node-163",
          "x": 64.203,
          "y": 31.364,
        },
        {
          "id": "node-164",
          "x": 77,
          "y": 78.027,
        },
        {
          "id": "node-165",
          "x": 26.76,
          "y": 10,
        },
        {
          "id": "node-166",
          "x": 66.374,
          "y": 17.921,
        },
        {
          "id": "node-167",
          "x": 74.646,
          "y": 32.957,
        },
        {
          "id": "node-168",
          "x": 39.887,
          "y": 75.894,
        },
        {
          "id": "node-169",
          "x": 57.039,
          "y": 10,
        },
        {
          "id": "node-17",
          "x": 34.034,
          "y": 85.538,
        },
        {
          "id": "node-170",
          "x": 36.645,
          "y": 10,
        },
        {
          "id": "node-171",
          "x": 73.065,
          "y": 22.179,
        },
        {
          "id": "node-172",
          "x": 23,
          "y": 88.597,
        },
        {
          "id": "node-173",
          "x": 77,
          "y": 20.749,
        },
        {
          "id": "node-174",
          "x": 23,
          "y": 79.852,
        },
        {
          "id": "node-175",
          "x": 29.943,
          "y": 69.94,
        },
        {
          "id": "node-176",
          "x": 62.698,
          "y": 24.593,
        },
        {
          "id": "node-177",
          "x": 23.25,
          "y": 89.999,
        },
        {
          "id": "node-178",
          "x": 67.191,
          "y": 13.537,
        },
        {
          "id": "node-179",
          "x": 59.845,
          "y": 78.498,
        },
        {
          "id": "node-18",
          "x": 36.768,
          "y": 83.145,
        },
        {
          "id": "node-180",
          "x": 69.296,
          "y": 31.065,
        },
        {
          "id": "node-181",
          "x": 57.509,
          "y": 90,
        },
        {
          "id": "node-182",
          "x": 27.4,
          "y": 20.887,
        },
        {
          "id": "node-183",
          "x": 77,
          "y": 66.675,
        },
        {
          "id": "node-184",
          "x": 36.664,
          "y": 30.085,
        },
        {
          "id": "node-185",
          "x": 56.409,
          "y": 18.643,
        },
        {
          "id": "node-186",
          "x": 67.869,
          "y": 90,
        },
        {
          "id": "node-187",
          "x": 30.242,
          "y": 19.205,
        },
        {
          "id": "node-188",
          "x": 77,
          "y": 22.234,
        },
        {
          "id": "node-189",
          "x": 23,
          "y": 78.118,
        },
        {
          "id": "node-19",
          "x": 45.917,
          "y": 19.909,
        },
        {
          "id": "node-190",
          "x": 69.604,
          "y": 74.153,
        },
        {
          "id": "node-191",
          "x": 23,
          "y": 34.056,
        },
        {
          "id": "node-192",
          "x": 76.761,
          "y": 10.072,
        },
        {
          "id": "node-193",
          "x": 70.585,
          "y": 75.197,
        },
        {
          "id": "node-194",
          "x": 28.195,
          "y": 10,
        },
        {
          "id": "node-195",
          "x": 70.179,
          "y": 19.297,
        },
        {
          "id": "node-196",
          "x": 34.231,
          "y": 90,
        },
        {
          "id": "node-197",
          "x": 23,
          "y": 10,
        },
        {
          "id": "node-198",
          "x": 76.75,
          "y": 90,
        },
        {
          "id": "node-199",
          "x": 38.318,
          "y": 26.029,
        },
        {
          "id": "node-2",
          "x": 67.802,
          "y": 10.876,
        },
        {
          "id": "node-20",
          "x": 33.849,
          "y": 84.237,
        },
        {
          "id": "node-21",
          "x": 29.439,
          "y": 26.167,
        },
        {
          "id": "node-22",
          "x": 69.121,
          "y": 90,
        },
        {
          "id": "node-23",
          "x": 38.979,
          "y": 10,
        },
        {
          "id": "node-24",
          "x": 77,
          "y": 14.596,
        },
        {
          "id": "node-25",
          "x": 32.235,
          "y": 73.634,
        },
        {
          "id": "node-26",
          "x": 23,
          "y": 18.311,
        },
        {
          "id": "node-27",
          "x": 76.806,
          "y": 77.489,
        },
        {
          "id": "node-28",
          "x": 23,
          "y": 83.675,
        },
        {
          "id": "node-29",
          "x": 28.371,
          "y": 10,
        },
        {
          "id": "node-3",
          "x": 71.582,
          "y": 90,
        },
        {
          "id": "node-30",
          "x": 23,
          "y": 26.528,
        },
        {
          "id": "node-31",
          "x": 57.788,
          "y": 10,
        },
        {
          "id": "node-32",
          "x": 42.206,
          "y": 82.151,
        },
        {
          "id": "node-33",
          "x": 44.95,
          "y": 13.927,
        },
        {
          "id": "node-34",
          "x": 64.616,
          "y": 81.403,
        },
        {
          "id": "node-35",
          "x": 32.063,
          "y": 82.688,
        },
        {
          "id": "node-36",
          "x": 27.405,
          "y": 19.365,
        },
        {
          "id": "node-37",
          "x": 62.625,
          "y": 83.686,
        },
        {
          "id": "node-38",
          "x": 42.138,
          "y": 90,
        },
        {
          "id": "node-39",
          "x": 74.266,
          "y": 30.813,
        },
        {
          "id": "node-4",
          "x": 30.17,
          "y": 90,
        },
        {
          "id": "node-40",
          "x": 33.174,
          "y": 24.944,
        },
        {
          "id": "node-41",
          "x": 77,
          "y": 80.234,
        },
        {
          "id": "node-42",
          "x": 30.357,
          "y": 73.428,
        },
        {
          "id": "node-43",
          "x": 68.136,
          "y": 14.003,
        },
        {
          "id": "node-44",
          "x": 55.214,
          "y": 83.008,
        },
        {
          "id": "node-45",
          "x": 30.56,
          "y": 20.397,
        },
        {
          "id": "node-46",
          "x": 74.201,
          "y": 23.086,
        },
        {
          "id": "node-47",
          "x": 23,
          "y": 83.723,
        },
        {
          "id": "node-48",
          "x": 68.788,
          "y": 10,
        },
        {
          "id": "node-49",
          "x": 76.794,
          "y": 90,
        },
        {
          "id": "node-5",
          "x": 23,
          "y": 10.787,
        },
        {
          "id": "node-50",
          "x": 67.283,
          "y": 86.773,
        },
        {
          "id": "node-51",
          "x": 77,
          "y": 78.991,
        },
        {
          "id": "node-52",
          "x": 25.146,
          "y": 18.625,
        },
        {
          "id": "node-53",
          "x": 72.41,
          "y": 90,
        },
        {
          "id": "node-54",
          "x": 41.623,
          "y": 13.845,
        },
        {
          "id": "node-55",
          "x": 77,
          "y": 10.171,
        },
        {
          "id": "node-56",
          "x": 66.799,
          "y": 85.585,
        },
        {
          "id": "node-57",
          "x": 37.482,
          "y": 10,
        },
        {
          "id": "node-58",
          "x": 61.465,
          "y": 14.607,
        },
        {
          "id": "node-59",
          "x": 32.772,
          "y": 88.06,
        },
        {
          "id": "node-6",
          "x": 77,
          "y": 20.1,
        },
        {
          "id": "node-60",
          "x": 57.019,
          "y": 88.885,
        },
        {
          "id": "node-61",
          "x": 28.35,
          "y": 82.119,
        },
        {
          "id": "node-62",
          "x": 68.448,
          "y": 14.865,
        },
        {
          "id": "node-63",
          "x": 64.302,
          "y": 76.194,
        },
        {
          "id": "node-64",
          "x": 24.221,
          "y": 29.971,
        },
        {
          "id": "node-65",
          "x": 62.828,
          "y": 90,
        },
        {
          "id": "node-66",
          "x": 43.951,
          "y": 85.781,
        },
        {
          "id": "node-67",
          "x": 68.099,
          "y": 26.517,
        },
        {
          "id": "node-68",
          "x": 77,
          "y": 81.869,
        },
        {
          "id": "node-69",
          "x": 24.234,
          "y": 10,
        },
        {
          "id": "node-7",
          "x": 76.9,
          "y": 10.49,
        },
        {
          "id": "node-70",
          "x": 23,
          "y": 89.888,
        },
        {
          "id": "node-71",
          "x": 31.509,
          "y": 16.757,
        },
        {
          "id": "node-72",
          "x": 75.357,
          "y": 90,
        },
        {
          "id": "node-73",
          "x": 23,
          "y": 17.191,
        },
        {
          "id": "node-74",
          "x": 76.514,
          "y": 18.915,
        },
        {
          "id": "node-75",
          "x": 76.185,
          "y": 71.887,
        },
        {
          "id": "node-76",
          "x": 35.689,
          "y": 22.679,
        },
        {
          "id": "node-77",
          "x": 69.187,
          "y": 90,
        },
        {
          "id": "node-78",
          "x": 23,
          "y": 71.143,
        },
        {
          "id": "node-79",
          "x": 31.18,
          "y": 10.183,
        },
        {
          "id": "node-8",
          "x": 70.534,
          "y": 77.841,
        },
        {
          "id": "node-80",
          "x": 27.759,
          "y": 81.935,
        },
        {
          "id": "node-81",
          "x": 75.911,
          "y": 16.65,
        },
        {
          "id": "node-82",
          "x": 48.568,
          "y": 80.812,
        },
        {
          "id": "node-83",
          "x": 39.71,
          "y": 16.949,
        },
        {
          "id": "node-84",
          "x": 62.196,
          "y": 89.784,
        },
        {
          "id": "node-85",
          "x": 25.012,
          "y": 77.667,
        },
        {
          "id": "node-86",
          "x": 70.433,
          "y": 10,
        },
        {
          "id": "node-87",
          "x": 70.534,
          "y": 88.303,
        },
        {
          "id": "node-88",
          "x": 23,
          "y": 10.176,
        },
        {
          "id": "node-89",
          "x": 61.901,
          "y": 10,
        },
        {
          "id": "node-9",
          "x": 38.664,
          "y": 90,
        },
        {
          "id": "node-90",
          "x": 28.352,
          "y": 16.197,
        },
        {
          "id": "node-91",
          "x": 77,
          "y": 85.593,
        },
        {
          "id": "node-92",
          "x": 23,
          "y": 23.401,
        },
        {
          "id": "node-93",
          "x": 77,
          "y": 28.123,
        },
        {
          "id": "node-94",
          "x": 28.794,
          "y": 90,
        },
        {
          "id": "node-95",
          "x": 23,
          "y": 11.964,
        },
        {
          "id": "node-96",
          "x": 74.121,
          "y": 81.198,
        },
        {
          "id": "node-97",
          "x": 23.606,
          "y": 86.849,
        },
        {
          "id": "node-98",
          "x": 76.454,
          "y": 10.766,
        },
        {
          "id": "node-99",
          "x": 75.284,
          "y": 88.101,
        },
      ]
    `);
  });

  it("falls back to a free stage coordinate when all spiral nudges are already occupied", () => {
    const targetIndex = 65;
    const goldenAngle = Math.PI * (3 - Math.sqrt(5));
    const horizontalPadding = 23;
    const verticalPadding = 10;
    const clampX = (value: number) =>
      Math.min(100 - horizontalPadding, Math.max(horizontalPadding, value));
    const clampY = (value: number) =>
      Math.min(100 - verticalPadding, Math.max(verticalPadding, value));

    // Replay the production spiral formula to build a walk of 64 cumulative
    // nudges starting from the same coordinate the target point will start
    // from, so every candidate the target tries during its own de-duplication
    // pass is already occupied by an earlier point.
    const spiralWalk: { x: number; y: number }[] = [{ x: 50, y: 50 }];
    for (let attempt = 1; attempt <= 64; attempt += 1) {
      const previous = spiralWalk[spiralWalk.length - 1];
      const angle = targetIndex * goldenAngle * 7 + attempt * goldenAngle;
      const radius = 0.25 * attempt;
      spiralWalk.push({
        x: clampX(previous.x + Math.cos(angle) * radius),
        y: clampY(previous.y + Math.sin(angle) * radius),
      });
    }

    const seed = spiralWalk[0];
    const points = [
      seed, // index 0: occupies the coordinate the target starts from
      ...spiralWalk.slice(1), // indices 1-64: occupy every spiral candidate
      seed, // index 65 === targetIndex: starts on an already-occupied point
    ];
    expect(points.length - 1).toBe(targetIndex);

    const deduped = deduplicateSettledPositions(points);

    const keys = deduped.map(({ x, y }) => `${x},${y}`);
    expect(new Set(keys).size).toBe(keys.length);
  });
});
